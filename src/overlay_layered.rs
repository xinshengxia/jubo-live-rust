// Win32 owns the visible per-pixel-alpha surface. WGL only renders to a private
// RGBA8 framebuffer; there is no OpenGL swap chain on top of the player.
use super::*;
use eframe::glow::{self, HasContext};
use std::{
    cell::RefCell,
    io, ptr,
    sync::OnceLock,
    sync::atomic::{AtomicU64, Ordering},
};
mod damage {
    include!("overlay_damage.rs");
}
use damage::Scene;
#[path = "overlay_dcomp.rs"]
mod dcomp;
use std::sync::Arc;
#[path = "overlay_sprite_frames.rs"]
mod sprite_frames;
pub(in crate::overlay) use sprite_frames::SpriteBatch;

#[derive(Clone, Copy)]
pub(in crate::overlay) struct Windows {
    pub(super) renderer: isize,
    pub(super) visible: isize,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetSystemMetrics(index: i32) -> i32;
    fn EnableWindow(window: Hwnd, enable: i32) -> i32;
    fn GetDpiForWindow(window: Hwnd) -> u32;
    fn GetClassNameW(window: Hwnd, name: *mut u16, len: i32) -> i32;
    fn UpdateLayeredWindowIndirect(window: Hwnd, update: *const LayeredUpdate) -> i32;
    fn SetLayeredWindowAttributes(window: Hwnd, color: u32, opacity: u8, flags: u32) -> i32;
}

#[repr(C)]
struct LayeredUpdate {
    bytes: u32,
    dst_dc: Hwnd,
    dst: *const Point,
    size: *const [i32; 2],
    src_dc: Hwnd,
    origin: *const Point,
    key: u32,
    blend: *const [u8; 4],
    flags: u32,
    dirty: *const Rect,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryFullProcessImageNameW(process: Hwnd, flags: u32, name: *mut u16, len: *mut u32) -> i32;
}

pub(in crate::overlay) fn use_bitmap(pid: u32) -> bool {
    match std::env::var("LIVE_HUB_OVERLAY_PLAYER").as_deref() {
        Ok("mpv") => return true,
        Ok("potplayer") => return false,
        _ => {}
    }
    // Standalone overlay commands can lack a launcher hint. Identify an existing
    // PotPlayer window even when the program was renamed, or its usual executable
    // before the player has shown its first window.
    unsafe {
        let hwnd = player_window(pid);
        let mut class = [0u16; 128];
        let n = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
        if n > 0 {
            let class = String::from_utf16_lossy(&class[..n as usize]).to_ascii_lowercase();
            if class.contains("potplayer") {
                return false;
            }
            if class == "mpv" {
                return true;
            }
        }
        let handle = OpenProcess(0x1000, 0, pid); // PROCESS_QUERY_LIMITED_INFORMATION
        if !handle.is_null() {
            let mut path = vec![0u16; 32768];
            let mut len = path.len() as u32;
            let got = QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len);
            CloseHandle(handle);
            if got != 0 {
                let path = String::from_utf16_lossy(&path[..len as usize]);
                if let Some(name) = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|v| v.to_str())
                {
                    if name.to_ascii_lowercase().starts_with("potplayer") {
                        return false;
                    }
                }
            }
        }
    }
    true
}

#[repr(C)]
struct BitmapInfo {
    header_size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bits: u16,
    compression: u32,
    image_size: u32,
    x_ppm: i32,
    y_ppm: i32,
    used: u32,
    important: u32,
    color: u32,
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: Hwnd) -> Hwnd;
    fn DeleteDC(dc: Hwnd) -> i32;
    fn SelectObject(dc: Hwnd, object: Hwnd) -> Hwnd;
    fn CreateDIBSection(
        dc: Hwnd,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut u8,
        section: Hwnd,
        offset: u32,
    ) -> Hwnd;
}

struct Dib {
    dc: Hwnd,
    bitmap: Hwnd,
    previous: Hwnd,
    bits: *mut u8,
    len: usize,
    size: [i32; 2],
}

impl Dib {
    fn new(size: [i32; 2]) -> io::Result<Self> {
        let [width, height] = size;
        let len = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .filter(|_| width > 0 && height > 0)
            .ok_or_else(|| io::Error::other("invalid overlay size"))? as usize;
        // Positive height makes this a bottom-up bitmap, matching glReadPixels.
        let info = BitmapInfo {
            header_size: 40,
            width,
            height,
            planes: 1,
            bits: 32,
            compression: 0,
            image_size: 0,
            x_ppm: 0,
            y_ppm: 0,
            used: 0,
            important: 0,
            color: 0,
        };
        unsafe {
            let dc = CreateCompatibleDC(ptr::null_mut());
            if dc.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut bits = ptr::null_mut();
            let bitmap = CreateDIBSection(dc, &info, 0, &mut bits, ptr::null_mut(), 0);
            if bitmap.is_null() {
                let err = io::Error::last_os_error();
                DeleteDC(dc);
                return Err(err);
            }
            let previous = SelectObject(dc, bitmap);
            if previous.is_null() || previous as isize == -1 {
                let err = io::Error::last_os_error();
                DeleteObject(bitmap);
                DeleteDC(dc);
                return Err(err);
            }
            Ok(Self {
                dc,
                bitmap,
                previous,
                bits,
                len,
                size,
            })
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct Framebuffer {
    fbo: glow::Framebuffer,
    color: glow::Renderbuffer,
    size: [i32; 2],
    bitmap: Option<BitmapFrame>,
}

struct BitmapFrame {
    dib: Dib,
    pack: Option<PackBuffer>,
}

impl Framebuffer {
    fn new(gl: &glow::Context, size: [i32; 2]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            size[0] > 0 && size[1] > 0,
            "invalid overlay framebuffer size"
        );
        unsafe {
            let fbo = gl.create_framebuffer().map_err(anyhow::Error::msg)?;
            let color = match gl.create_renderbuffer() {
                Ok(color) => color,
                Err(err) => {
                    gl.delete_framebuffer(fbo);
                    return Err(anyhow::anyhow!(err));
                }
            };
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(color));
            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, size[0], size[1]);
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::RENDERBUFFER,
                Some(color),
            );
            gl.bind_renderbuffer(glow::RENDERBUFFER, None);
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.delete_renderbuffer(color);
                gl.delete_framebuffer(fbo);
                anyhow::bail!("incomplete RGBA8 overlay framebuffer");
            }
            Ok(Self {
                fbo,
                color,
                size,
                bitmap: None,
            })
        }
    }

    fn bitmap(&mut self, gl: &glow::Context) -> anyhow::Result<&mut BitmapFrame> {
        if self.bitmap.is_none() {
            let dib = Dib::new(self.size)?;
            let pack = PackBuffer::new(gl, dib.len);
            self.bitmap = Some(BitmapFrame { dib, pack });
        }
        Ok(self.bitmap.as_mut().unwrap())
    }

    fn delete(self, gl: &glow::Context) {
        unsafe {
            if let Some(pack) = self.bitmap.and_then(|bitmap| bitmap.pack) {
                gl.delete_buffer(pack.buffer);
            }
            gl.delete_renderbuffer(self.color);
            gl.delete_framebuffer(self.fbo);
        }
    }
}

// Queue compact reads into one reusable buffer. Reading rectangles straight into a
// full-width DIB with a large row pitch can make drivers allocate a separate
// transfer surface for each rectangle size.
struct PackBuffer {
    buffer: glow::Buffer,
    capacity: usize,
}

struct PackedRect {
    buffer_offset: usize,
    dib_offset: usize,
    rows: usize,
    row_bytes: usize,
}

impl PackBuffer {
    fn new(gl: &glow::Context, bytes: usize) -> Option<Self> {
        if gl.version().major < 3 {
            return None;
        }
        let capacity = bytes.min(4 * 1024 * 1024);
        unsafe {
            let buffer = gl.create_buffer().ok()?;
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buffer));
            gl.buffer_data_size(glow::PIXEL_PACK_BUFFER, capacity as i32, glow::STREAM_READ);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            if gl.get_error() != glow::NO_ERROR {
                gl.delete_buffer(buffer);
                return None;
            }
            Some(Self { buffer, capacity })
        }
    }

    fn read(
        &self,
        gl: &glow::Context,
        dst: &mut [u8],
        size: [i32; 2],
        regions: &[Rect],
    ) -> anyhow::Result<()> {
        if regions.is_empty() {
            return Ok(());
        }
        let [width, height] = size;
        let gles = gl.version().is_embedded;
        let format = if gles { glow::RGBA } else { glow::BGRA };
        let mut batch = Vec::with_capacity(regions.len());
        let mut used = 0;
        unsafe {
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(self.buffer));
            gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 0);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
        }
        let result = (|| {
            for r in regions {
                let w = r.right - r.left;
                let row_bytes = w as usize * 4;
                anyhow::ensure!(
                    row_bytes <= self.capacity,
                    "overlay row exceeds pixel pack buffer"
                );
                let mut y = height - r.bottom;
                let end = height - r.top;
                while y < end {
                    let rows = ((self.capacity - used) / row_bytes).min((end - y) as usize);
                    if rows == 0 {
                        Self::copy_batch(gl, dst, width, &batch, used, gles)?;
                        batch.clear();
                        used = 0;
                        continue;
                    }
                    unsafe {
                        gl.read_pixels(
                            r.left,
                            y,
                            w,
                            rows as i32,
                            format,
                            glow::UNSIGNED_BYTE,
                            glow::PixelPackData::BufferOffset(used as u32),
                        );
                    }
                    batch.push(PackedRect {
                        buffer_offset: used,
                        dib_offset: (y as usize * width as usize + r.left as usize) * 4,
                        rows,
                        row_bytes,
                    });
                    used += rows * row_bytes;
                    y += rows as i32;
                }
            }
            Self::copy_batch(gl, dst, width, &batch, used, gles)
        })();
        unsafe {
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        }
        result
    }

    fn copy_batch(
        gl: &glow::Context,
        dst: &mut [u8],
        width: i32,
        batch: &[PackedRect],
        used: usize,
        gles: bool,
    ) -> anyhow::Result<()> {
        unsafe {
            let ptr =
                gl.map_buffer_range(glow::PIXEL_PACK_BUFFER, 0, used as i32, glow::MAP_READ_BIT);
            anyhow::ensure!(
                !ptr.is_null(),
                "mapping the overlay pixel pack buffer failed"
            );
            let source = std::slice::from_raw_parts(ptr, used);
            for patch in batch {
                for row in 0..patch.rows {
                    let target = patch.dib_offset + row * width as usize * 4;
                    let from = patch.buffer_offset + row * patch.row_bytes;
                    let line = &mut dst[target..target + patch.row_bytes];
                    line.copy_from_slice(&source[from..from + patch.row_bytes]);
                    if gles {
                        for px in line.chunks_exact_mut(4) {
                            px.swap(0, 2);
                        }
                    }
                }
            }
            gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
            let error = gl.get_error();
            anyhow::ensure!(
                error == glow::NO_ERROR,
                "overlay pixel pack transfer failed ({error:#x})"
            );
        }
        Ok(())
    }
}

struct Presenter {
    window: Hwnd,
    frame: Option<Framebuffer>,
    gpu: Option<dcomp::GpuPresenter>,
    gpu_requested: bool,
    sprites: Option<sprite_frames::Frames>,
    composition_style: bool,
    gpu_fail_after: Option<u64>,
    gpu_submitted: u64,
    painting: bool,
    sc_generation: u64,
    last_warning: Option<Instant>,
    content_hidden: bool,
    cleared_while_hidden: bool,
    last_sample: Option<Instant>,
    diagnostic: bool,
    previous_sample: Option<u64>,
    sample_changes: u64,
    capture: Option<std::path::PathBuf>,
    previous_scene: Option<Arc<Scene>>,
    audit: Option<DamageAudit>,
}

#[derive(Default)]
struct DamageAudit {
    reference: Vec<u8>,
    checked: u64,
    empty_checked: u64,
    mismatches: u64,
    full: u64,
    partial: u64,
    skipped: u64,
    pixels: u64,
    possible: u64,
    last_log: Option<Instant>,
}

// Reference path for validation, and the 1.1.1-compatible fallback on
// drivers that cannot allocate or map a compact pixel pack buffer.
fn read_full(gl: &glow::Context, bytes: &mut [u8], [w, h]: [i32; 2]) -> anyhow::Result<()> {
    let gles = gl.version().is_embedded;
    unsafe {
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
        if !gles || gl.version().major >= 3 {
            gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 0);
        }
        gl.read_pixels(
            0,
            0,
            w,
            h,
            if gles { glow::RGBA } else { glow::BGRA },
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(bytes)),
        );
        let error = gl.get_error();
        anyhow::ensure!(
            error == glow::NO_ERROR,
            "overlay pixel readback failed ({error:#x})"
        );
    }
    if gles {
        for pixel in bytes.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    Ok(())
}

impl DamageAudit {
    fn check(
        &mut self,
        gl: &glow::Context,
        bytes: &[u8],
        size: [i32; 2],
        patches: &[Rect],
    ) -> anyhow::Result<()> {
        let whole = damage::full(size);
        self.reference.resize(bytes.len(), 0);
        read_full(gl, &mut self.reference, size)?;
        self.checked += 1;
        self.possible += damage::area(whole) as u64;
        let count = patches.iter().copied().map(damage::area).sum::<i64>() as u64;
        self.pixels += count;
        if count == 0 {
            self.skipped += 1;
        } else if count == damage::area(whole) as u64 {
            self.full += 1;
        } else {
            self.partial += 1;
        }
        let mismatch = self.reference != bytes;
        if mismatch {
            self.mismatches += 1;
        } else if self.reference.chunks_exact(4).all(|pixel| pixel[3] == 0) {
            self.empty_checked += 1;
        }
        if mismatch
            || self
                .last_log
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            diag_log(&format!(
                "damage audit: checked={} mismatches={} full={} partial={} skipped={} pixels={} possible={} empty_checked={}",
                self.checked,
                self.mismatches,
                self.full,
                self.partial,
                self.skipped,
                self.pixels,
                self.possible,
                self.empty_checked
            ));
            self.last_log = Some(Instant::now());
        }
        anyhow::ensure!(!mismatch, "dirty DIB differs from full RGBA framebuffer");
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ScTarget {
    overlay: isize,
    player: isize,
    area: ClientArea,
    dpi: u32,
}

impl ScTarget {
    fn size(self) -> [i32; 2] {
        [self.area.width, self.area.height]
    }

    fn geometry(self) -> (isize, isize, [i32; 2], u32) {
        (self.overlay, self.player, self.size(), self.dpi)
    }
}

#[derive(Default)]
struct ScPresentation {
    target: Option<ScTarget>,
    generation: u64,
    ready: bool,
}

impl ScPresentation {
    fn set_target(&mut self, target: Option<ScTarget>) -> bool {
        let changed = self.target.map(ScTarget::geometry) != target.map(ScTarget::geometry);
        self.target = target;
        if changed {
            self.generation = self.generation.wrapping_add(1);
            self.ready = false;
        }
        changed
    }

    fn accept(&mut self, generation: u64, size: [i32; 2], scale: f32, submitted: bool) -> bool {
        self.ready = generation == self.generation
            && self.target.is_some_and(|target| {
                target.size() == size && (scale - target.dpi as f32 / 96.0).abs() <= 0.001
            })
            && (submitted || self.ready);
        self.ready
    }
}

static SC_PRESENTATION: std::sync::Mutex<ScPresentation> = std::sync::Mutex::new(ScPresentation {
    target: None,
    generation: 0,
    ready: false,
});

fn apply_sc_hits(hits: Option<&InteractionState>) {
    if let Some(mut state) = INTERACTION.get().and_then(|state| state.lock().ok()) {
        state.apply_bitmap_hits(hits);
    }
}

pub(super) fn follow_sc_target(overlay: Hwnd, player: Hwnd, area: Option<ClientArea>) {
    let target = area.map(|area| ScTarget {
        overlay: overlay as isize,
        player: player as isize,
        area,
        dpi: unsafe { GetDpiForWindow(player) },
    });
    if let Ok(mut presentation) = SC_PRESENTATION.lock()
        && presentation.set_target(target)
    {
        apply_sc_hits(None);
    }
}

pub(super) fn sc_input_target() -> Option<(Hwnd, Hwnd, ClientArea)> {
    let presentation = SC_PRESENTATION.lock().ok()?;
    let target = presentation.target.filter(|_| presentation.ready)?;
    Some((target.overlay as Hwnd, target.player as Hwnd, target.area))
}

fn invalidate_sc_frame() {
    let wake = if let Ok(mut presentation) = SC_PRESENTATION.lock() {
        let was_ready = std::mem::take(&mut presentation.ready);
        if was_ready {
            apply_sc_hits(None);
        }
        was_ready
    } else {
        false
    };
    if wake {
        wake_bitmap_sc_input();
    }
}

fn accept_sc_frame(
    generation: u64,
    size: [i32; 2],
    scale: f32,
    hits: &InteractionState,
    submitted: bool,
) {
    if let Ok(mut presentation) = SC_PRESENTATION.lock() {
        let was_ready = presentation.ready;
        let ready = presentation.accept(generation, size, scale, submitted);
        apply_sc_hits(ready.then_some(hits));
        if was_ready == ready {
            return;
        }
    }
    // The follow loop refreshes stable regions. Wake the input thread promptly
    // when this frame changes whether the window can accept any input.
    wake_bitmap_sc_input();
}

static CLOCK: OnceLock<Instant> = OnceLock::new();
static SUBMITTED: AtomicU64 = AtomicU64::new(0);
static LAST_MS: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);
static NEEDS_REPRESENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(super) fn visibility_changed() {
    NEEDS_REPRESENT.store(true, Ordering::Release);
}
static ALPHA_COUNTS: AtomicU64 = AtomicU64::new(0);

fn elapsed_ms() -> u64 {
    CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub(super) fn log_health(visible: bool) {
    let frames = SUBMITTED.load(Ordering::Relaxed);
    let age = if frames == 0 {
        None
    } else {
        Some(elapsed_ms().saturating_sub(LAST_MS.load(Ordering::Relaxed)))
    };
    let sample = ALPHA_COUNTS.load(Ordering::Relaxed);
    let line = format!(
        "layered presentation: visible={visible} submitted={frames} last_ms_ago={age:?} errors={} alpha_sample(clear/partial/solid)={}/{}/{}",
        FAILURES.load(Ordering::Relaxed),
        sample >> 32,
        (sample >> 16) & 65535,
        sample & 65535
    );
    super::super::overlay_log(crate::diag::Level::Info, &line);
    diag_log(&line);
}

// GDI handles and the GL context are owned solely by eframe's event-loop thread.
thread_local! { static PRESENTER: RefCell<Option<Presenter>> = const { RefCell::new(None) }; }

fn offscreen_x(width: i32) -> i32 {
    unsafe { GetSystemMetrics(76) }
        .saturating_sub(width)
        .saturating_sub(64) // SM_XVIRTUALSCREEN
}

const WS_EX_LAYERED: isize = 0x0008_0000;

fn set_extended_style(window: Hwnd, style: isize) -> io::Result<()> {
    unsafe {
        // A return of zero can mean either success or failure.
        windows_sys::Win32::Foundation::SetLastError(0);
        let previous = SetWindowLongPtrW(window, -20, style);
        if previous == 0 && windows_sys::Win32::Foundation::GetLastError() != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn reset_bitmap_style(window: Hwnd) -> io::Result<()> {
    let style = unsafe { GetWindowLongPtrW(window, -20) };
    // ULW is unavailable after SetLayeredWindowAttributes until the layer bit is reset.
    let unlayered = style & !WS_EX_LAYERED;
    set_extended_style(window, unlayered).map_err(|e| io::Error::other(format!("clear layered style: {e}")))?;
    let window_style = unsafe { GetWindowLongPtrW(window, -16) };
    if window_style & 0x0800_0000 != 0 {
        unsafe { EnableWindow(window, 1) };
    }
    set_extended_style(window, unlayered | WS_EX_LAYERED).map_err(|e| io::Error::other(format!("restore layered style: {e}")))?;
    Ok(())
}

pub(in crate::overlay) fn create(
    renderer_title: &str,
    title: &str,
    bitmap: bool,
) -> io::Result<Windows> {
    unsafe {
        let renderer = FindWindowW(ptr::null(), wide(renderer_title).as_ptr());
        if renderer.is_null() {
            return Err(io::Error::other("overlay rendering window missing"));
        }
        if !bitmap {
            return Ok(Windows {
                renderer: renderer as isize,
                visible: renderer as isize,
            });
        }
        // eframe shows its window after its first paint, even if created hidden.
        let style = GetWindowLongPtrW(renderer, -20);
        SetWindowLongPtrW(renderer, -20, (style | 0x0800_0080) & !0x40000);
        if SetWindowPos(
            renderer,
            ptr::null_mut(),
            offscreen_x(640),
            0,
            640,
            360,
            0x0014,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let name = wide("LiveHubDanmakuLayered");
        let instance = GetModuleHandleW(ptr::null());
        let mut class: WndClassExW = std::mem::zeroed();
        class.size = std::mem::size_of::<WndClassExW>() as u32;
        class.wnd_proc = Some(DefWindowProcW);
        class.instance = instance;
        class.class_name = name.as_ptr();
        if RegisterClassExW(&class) == 0 && io::Error::last_os_error().raw_os_error() != Some(1410)
        {
            return Err(io::Error::last_os_error());
        }
        let gpu_requested = matches!(
            std::env::var("LIVE_HUB_OVERLAY_PRESENTER").as_deref(),
            Ok("dcomp")
        ) && std::env::var_os("LIVE_HUB_OVERLAY_VERIFY_DAMAGE").is_none();
        let sprites_requested = matches!(
            std::env::var("LIVE_HUB_OVERLAY_PRESENTER").as_deref(),
            Ok("dcomp-sprites") | Err(std::env::VarError::NotPresent)
        ) && std::env::var_os("LIVE_HUB_OVERLAY_VERIFY_DAMAGE").is_none()
          && std::env::var_os("LIVE_HUB_OVERLAY_DIAG_SCREENSHOT").is_none()
          && super::super::diag_freeze_ms_from(|key| std::env::var(key).ok()).is_none();
        // This is the same HWND that the existing follow and SC hit-window code tracks.
        let window = CreateWindowExW(
            0x0808_00a0,
            name.as_ptr(),
            wide(title).as_ptr(),
            if sprites_requested { 0x8800_0000 } else { 0x8000_0000 },
            0,
            0,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null_mut(),
        );
        if window.is_null() {
            return Err(io::Error::last_os_error());
        }
        PRESENTER.with(|slot| {
            *slot.borrow_mut() = Some(Presenter {
                window,
                frame: None,
                gpu: None,
                gpu_requested,
                sprites: sprites_requested.then(sprite_frames::Frames::default),
                composition_style: gpu_requested || sprites_requested,
                gpu_fail_after: std::env::var_os("LIVE_HUB_OVERLAY_FIXTURE")
                    .and_then(|_| std::env::var("LIVE_HUB_OVERLAY_DIAG_DCOMP_FAIL_AFTER").ok())
                    .and_then(|frames| frames.parse().ok()),
                gpu_submitted: 0,
                painting: false,
                sc_generation: 0,
                last_warning: None,
                capture: None,
                content_hidden: false,
                cleared_while_hidden: false,
                last_sample: None,
                diagnostic: std::env::var_os(super::super::ENV_DIAG_LOG).is_some(),
                previous_sample: None,
                sample_changes: 0,
                previous_scene: None,
                audit: (std::env::var_os("LIVE_HUB_OVERLAY_VERIFY_DAMAGE").as_deref()
                    == Some(std::ffi::OsStr::new("1")))
                .then(DamageAudit::default),
            })
        });
        Ok(Windows {
            renderer: renderer as isize,
            visible: window as isize,
        })
    }
}

pub(super) fn sync_renderer(renderer: Hwnd, target: Hwnd, area: ClientArea, ctx: &egui::Context) {
    let x = offscreen_x(area.width);
    let desired = ClientArea { left: x, ..area };
    if client_area(renderer) != Some(desired) {
        unsafe {
            SetWindowPos(
                renderer,
                ptr::null_mut(),
                x,
                area.top,
                area.width,
                area.height,
                0x4014,
            );
        }
        ctx.request_repaint();
    }
    let dpi = unsafe { GetDpiForWindow(target) };
    let scale = dpi as f32 / 96.0;
    if dpi > 0 && (ctx.pixels_per_point() - scale).abs() > 0.001 {
        ctx.set_pixels_per_point(scale);
    }
}

fn save_renderer_screenshot(
    path: std::path::PathBuf,
    bytes: &[u8],
    [w, h]: [i32; 2],
) -> anyhow::Result<()> {
    use image::ImageEncoder;
    let mut rgba = Vec::with_capacity(bytes.len());
    for row in bytes.chunks_exact(w as usize * 4).rev() {
        for px in row.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    image::codecs::png::PngEncoder::new(file).write_image(
        &rgba,
        w as u32,
        h as u32,
        image::ExtendedColorType::Rgba8,
    )?;
    diag_log("renderer screenshot saved");
    Ok(())
}

impl Presenter {
    fn fall_back_to_bitmap(&mut self, gl: &glow::Context) -> anyhow::Result<()> {
        invalidate_sc_frame();
        self.gpu_requested = false;
        self.previous_scene = None;
        self.cleared_while_hidden = false;
        if let Some(mut gpu) = self.gpu.take() {
            gpu.destroy(gl);
        }
        if self.composition_style {
            reset_bitmap_style(self.window)?;
            self.composition_style = false;
        }
        Ok(())
    }

    fn initialize_gpu(&mut self, gl: &glow::Context) -> anyhow::Result<()> {
        if !self.gpu_requested {
            if self.composition_style {
                self.fall_back_to_bitmap(gl)?;
            }
        } else if self.gpu.is_none() {
            let initialized = (|| {
                if unsafe { SetLayeredWindowAttributes(self.window, 0, 255, 2) } == 0 {
                    return Err(io::Error::last_os_error().into());
                }
                dcomp::GpuPresenter::new(self.window)
            })();
            match initialized {
                Ok(gpu) => self.gpu = Some(gpu),
                Err(error) => {
                    self.report(Err(
                        error.context("DirectComposition initialization; switching to bitmap")
                    ));
                    self.fall_back_to_bitmap(gl)?;
                }
            }
        }
        Ok(())
    }

    fn present_gpu(
        &mut self,
        gl: &glow::Context,
        scene: &Arc<Scene>,
        actual_scale: f32,
        layout_size: [i32; 2],
        sc_hits: &InteractionState,
    ) -> anyhow::Result<()> {
        let frame = self.frame.as_ref().unwrap();
        let size = frame.size;
        let layout_matches = scene.pixels_per_point == actual_scale && layout_size == size;
        let previous = self.previous_scene.take().filter(|_| layout_matches);
        let dirty = damage::changed(previous.as_deref(), scene, size)
            .into_iter()
            .reduce(damage::bounds);
        if let Some(dirty) = dirty {
            if self.gpu_fail_after == Some(self.gpu_submitted) {
                anyhow::bail!("injected DirectComposition submission failure");
            }
            self.gpu
                .as_mut()
                .unwrap()
                .submit(gl, frame.fbo, size, Some(dirty))?;
            self.gpu_submitted += 1;
            LAST_MS.store(elapsed_ms(), Ordering::Relaxed);
            if SUBMITTED.fetch_add(1, Ordering::Relaxed) == 0 {
                diag_log("overlay presenter: first DirectComposition GPU frame submitted");
            }
        }
        if layout_matches
            && client_area(self.window).is_some_and(|area| [area.width, area.height] == size)
        {
            self.previous_scene = Some(scene.clone());
            self.cleared_while_hidden = self.content_hidden;
            accept_sc_frame(
                self.sc_generation,
                size,
                actual_scale,
                sc_hits,
                dirty.is_some(),
            );
        } else {
            invalidate_sc_frame();
        }
        if let Some(path) = self.capture.take() {
            let capture = (|| {
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(frame.fbo));
                }
                let mut pixels = vec![0; size[0] as usize * size[1] as usize * 4];
                read_full(gl, &mut pixels, size)?;
                save_renderer_screenshot(path, &pixels, size)
            })();
            if let Err(error) = capture {
                diag_log(&format!("DirectComposition diagnostic capture: {error:#}"));
            }
        }
        Ok(())
    }
    fn prepare(&mut self, gl: &glow::Context, size: [i32; 2]) -> anyhow::Result<()> {
        self.painting = false;
        let generation = SC_PRESENTATION
            .lock()
            .map(|state| state.generation)
            .unwrap_or(0);
        if NEEDS_REPRESENT.swap(false, Ordering::AcqRel) || generation != self.sc_generation {
            self.previous_scene = None;
            self.sc_generation = generation;
        }
        if unsafe { IsWindowVisible(self.window) } == 0
            || !client_area(self.window).is_some_and(|area| [area.width, area.height] == size)
        {
            self.previous_scene = None;
            invalidate_sc_frame();
            return Ok(());
        }
        if self.sprites.is_none() { self.initialize_gpu(gl)?; }
        if self.frame.as_ref().map(|f| f.size) != Some(size) {
            self.previous_scene = None;
            invalidate_sc_frame();
            let next = Framebuffer::new(gl, size)?;
            if let Some(old) = self.frame.replace(next) {
                old.delete(gl);
            }
            self.cleared_while_hidden = false;
            let line = format!("layered framebuffer ready: {}x{}", size[0], size[1]);
            super::super::overlay_log(crate::diag::Level::Info, &line);
            diag_log(&line);
        } else if self.content_hidden
            && self.cleared_while_hidden
            && self.previous_scene.is_some()
            && self.audit.is_none()
            && self.capture.is_none()
        {
            return Ok(());
        }
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.frame.as_ref().unwrap().fbo));
            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        self.painting = true;
        Ok(())
    }

    fn present(
        &mut self,
        gl: &glow::Context,
        scene: &Arc<Scene>,
        actual_scale: f32,
        layout_size: [i32; 2],
        sc_hits: &InteractionState,
    ) -> anyhow::Result<()> {
        if !std::mem::take(&mut self.painting) {
            return Ok(());
        }
        if self.gpu.is_some() {
            match self.present_gpu(gl, scene, actual_scale, layout_size, sc_hits) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    self.report(Err(
                        error.context("DirectComposition frame; switching to bitmap")
                    ));
                    self.fall_back_to_bitmap(gl)?;
                    unsafe {
                        gl.bind_framebuffer(
                            glow::FRAMEBUFFER,
                            Some(self.frame.as_ref().unwrap().fbo),
                        );
                    }
                }
            }
        }
        let frame = self
            .frame
            .as_mut()
            .unwrap()
            .bitmap(gl)
            .inspect_err(|_| invalidate_sc_frame())?;
        let dib = &mut frame.dib;
        let [w, h] = dib.size;
        // egui_glow blends premultiplied RGB. Windows accepts the same values in BGRA order.
        let bytes = unsafe { std::slice::from_raw_parts_mut(dib.bits, dib.len) };
        // Once the retained DIB is touched, only a successful window update
        // establishes a new baseline. A failed/racing submit retries a full frame.
        // During a monitor/DPI transition layout and drawing can briefly disagree.
        let layout_matches = scene.pixels_per_point == actual_scale && layout_size == dib.size;
        let previous = self.previous_scene.take().filter(|_| layout_matches);
        let mut patches = damage::changed(previous.as_deref(), scene, dib.size);
        if self.capture.is_some() && patches.is_empty() {
            patches.push(damage::full(dib.size));
        }
        if let Some(pack) = &frame.pack
            && let Err(err) = pack.read(gl, bytes, dib.size, &patches)
        {
            super::super::overlay_log(
                crate::diag::Level::Warn,
                &format!("overlay pixel pack unavailable; retrying direct readback: {err}"),
            );
            if let Some(pack) = frame.pack.take() {
                unsafe {
                    gl.delete_buffer(pack.buffer);
                }
            }
            patches = vec![damage::full(dib.size)];
        }
        if frame.pack.is_none() && !patches.is_empty() {
            patches = vec![damage::full(dib.size)];
            read_full(gl, bytes, dib.size).inspect_err(|_| invalidate_sc_frame())?;
        }
        if let Some(audit) = &mut self.audit {
            audit
                .check(gl, bytes, dib.size, &patches)
                .inspect_err(|_| invalidate_sc_frame())?;
        }
        let Some(dirty) = patches.iter().copied().reduce(damage::bounds) else {
            if client_area(self.window).is_some_and(|area| [area.width, area.height] == dib.size) {
                self.previous_scene = Some(scene.clone());
                self.cleared_while_hidden = self.content_hidden;
                accept_sc_frame(self.sc_generation, dib.size, actual_scale, sc_hits, false);
            } else {
                invalidate_sc_frame();
            }
            return Ok(());
        };
        let origin = Point::default();
        let blend = [0, 0, 255, 1]; // AC_SRC_OVER, SourceConstantAlpha, AC_SRC_ALPHA
        let update = LayeredUpdate {
            bytes: std::mem::size_of::<LayeredUpdate>() as u32,
            dst_dc: ptr::null_mut(),
            dst: ptr::null(),
            size: &dib.size,
            src_dc: dib.dc,
            origin: &origin,
            key: 0,
            blend: &blend,
            flags: 2 | 8,
            dirty: if previous.is_none() {
                ptr::null()
            } else {
                &dirty
            },
        };
        if unsafe { UpdateLayeredWindowIndirect(self.window, &update) } == 0 {
            let error = io::Error::last_os_error();
            invalidate_sc_frame();
            // The asynchronous watcher can resize the destination during
            // readback. A stale frame must never resize the player overlay back.
            if client_area(self.window).is_some_and(|area| [area.width, area.height] != dib.size) {
                return Ok(());
            }
            return Err(anyhow::anyhow!(
                "UpdateLayeredWindowIndirect {w}x{h}: {error}"
            ));
        }
        self.cleared_while_hidden = self.content_hidden;
        if layout_matches
            && client_area(self.window).is_some_and(|area| [area.width, area.height] == dib.size)
        {
            self.previous_scene = Some(scene.clone());
            accept_sc_frame(self.sc_generation, dib.size, actual_scale, sc_hits, true);
        } else {
            invalidate_sc_frame();
        }
        LAST_MS.store(elapsed_ms(), Ordering::Relaxed);
        let first = SUBMITTED.fetch_add(1, Ordering::Relaxed) == 0;
        let interval = Duration::from_secs(if self.diagnostic { 1 } else { 10 });
        if self.last_sample.is_none_or(|at| at.elapsed() >= interval) {
            let mut counts = [0u64; 3];
            let mut sample = 0xcbf29ce484222325u64;
            for px in bytes
                .chunks_exact(4)
                .step_by((bytes.len() / 4 / 4096).max(1))
            {
                counts[match px[3] {
                    0 => 0,
                    255 => 2,
                    _ => 1,
                }] += 1;
                if self.diagnostic {
                    sample = (sample ^ u32::from_ne_bytes([px[0], px[1], px[2], px[3]]) as u64)
                        .wrapping_mul(0x100000001b3);
                }
            }
            ALPHA_COUNTS.store(
                (counts[0] << 32) | (counts[1] << 16) | counts[2],
                Ordering::Relaxed,
            );
            self.last_sample = Some(Instant::now());
            if self.diagnostic {
                if self
                    .previous_sample
                    .replace(sample)
                    .is_some_and(|old| old != sample)
                {
                    self.sample_changes += 1;
                }
                diag_log(&format!(
                    "layered sample: submitted={} sample_changes={} nonzero={}",
                    SUBMITTED.load(Ordering::Relaxed),
                    self.sample_changes,
                    counts[1] + counts[2]
                ));
            }
        }
        if first {
            log_health(true);
        }
        if let Some(path) = self.capture.take() {
            save_renderer_screenshot(path, bytes, [w, h])?;
        }
        Ok(())
    }

    fn report(&mut self, result: anyhow::Result<()>) {
        if let Err(error) = result {
            FAILURES.fetch_add(1, Ordering::Relaxed);
            if self
                .last_warning
                .is_none_or(|time| time.elapsed() >= Duration::from_secs(5))
            {
                let line = format!("layered window: {error:#}");
                super::super::overlay_log(crate::diag::Level::Warn, &line);
                diag_log(&line);
                self.last_warning = Some(Instant::now());
            }
        }
    }
}

pub(in crate::overlay) fn request_capture() -> bool {
    PRESENTER.with(|slot| {
        if let Some(p) = slot.borrow_mut().as_mut() {
            p.capture = std::env::var_os("LIVE_HUB_OVERLAY_DIAG_SCREENSHOT").map(Into::into);
            true
        } else {
            false
        }
    })
}

pub(in crate::overlay) fn set_content_hidden(hidden: bool) {
    PRESENTER.with(|slot| {
        if let Some(p) = slot.borrow_mut().as_mut() {
            if hidden && !p.content_hidden {
                invalidate_sc_frame();
                p.previous_scene = None;
            }
            p.content_hidden = hidden;
            if !hidden {
                p.cleared_while_hidden = false;
            }
        }
    });
}

pub(in crate::overlay) fn sprites_enabled() -> bool {
    PRESENTER.with(|slot| slot.borrow().as_ref().is_some_and(|p| p.sprites.is_some()))
}

pub(in crate::overlay) fn has_presenter() -> bool {
    PRESENTER.with(|slot| slot.borrow().is_some())
}

pub(in crate::overlay) fn schedule(ctx: &egui::Context, sc_hits: InteractionState, batch: SpriteBatch) {
    if sprites_enabled() {
        sprite_frames::schedule(ctx, sc_hits, batch);
        return;
    }
    let rect = ctx.content_rect();
    let layout = ctx.viewport_rect().size() * ctx.pixels_per_point();
    let layout_size = [layout.x.round() as i32, layout.y.round() as i32];
    let scene = Arc::new(Scene::collect(ctx));
    let sc_hits = Arc::new(sc_hits);
    // Both callbacks execute on the same event-loop/GL thread. The locked
    // egui_glow painter restores its shader state between primitives but leaves
    // our draw framebuffer bound until the second callback restores its target.
    for (order, first) in [(egui::Order::Background, true), (egui::Order::Debug, false)] {
        let scene = scene.clone();
        let sc_hits = sc_hits.clone();
        let callback = eframe::egui_glow::CallbackFn::new(move |info, painter| {
            PRESENTER.with(|slot| {
                let mut state = slot.borrow_mut();
                if let Some(p) = state.as_mut() {
                    let gl = painter.gl();
                    let result = if first {
                        p.prepare(gl, info.screen_size_px.map(|v| v as i32))
                    } else {
                        let result =
                            p.present(gl, &scene, info.pixels_per_point, layout_size, &sc_hits);
                        unsafe {
                            gl.bind_framebuffer(glow::FRAMEBUFFER, painter.intermediate_fbo());
                        }
                        result
                    };
                    p.report(result);
                }
            });
        });
        ctx.layer_painter(egui::LayerId::new(
            order,
            egui::Id::new(("layered-frame", first)),
        ))
        .add(egui::PaintCallback {
            rect,
            callback: std::sync::Arc::new(callback),
        });
    }
}

pub(in crate::overlay) fn shutdown(gl: Option<&glow::Context>) {
    PRESENTER.with(|slot| {
        if let Some(mut p) = slot.borrow_mut().take() {
            invalidate_sc_frame();
            if let Some(gl) = gl {
                if let Some(mut gpu) = p.gpu.take() {
                    gpu.destroy(gl);
                }
                if let Some(frame) = p.frame.take() {
                    frame.delete(gl);
                }
            }
            p.sprites.take();
            unsafe {
                DestroyWindow(p.window);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn GetPixel(dc: Hwnd, x: i32, y: i32) -> u32;
    }

    fn sc_target(size: [i32; 2], dpi: u32) -> ScTarget {
        ScTarget {
            overlay: 1,
            player: 2,
            area: ClientArea {
                left: 0,
                top: 0,
                width: size[0],
                height: size[1],
            },
            dpi,
        }
    }

    #[test]
    fn bitmap_sc_failure_discards_hits_and_requires_a_new_submit() {
        let size = [960, 540];
        let mut presentation = ScPresentation::default();
        presentation.set_target(Some(sc_target(size, 96)));
        let generation = presentation.generation;
        assert!(!presentation.accept(generation, size, 1.0, false));
        assert!(presentation.accept(generation, size, 1.0, true));
        let mut active = InteractionState::default();
        let staged = InteractionState {
            enabled: true,
            hit: vec![[20.0, 60.0, 260.0, 200.0]],
            close: vec![[235.0, 60.0, 260.0, 85.0]],
            bar_track: Some([260.0, 60.0, 265.0, 200.0]),
            bar_thumb: Some([260.0, 60.0, 265.0, 80.0]),
            ..Default::default()
        };
        active.apply_bitmap_hits(Some(&staged));
        active.click = Some((240.0, 65.0));
        active.wheel = 1.0;
        active.bar_drag = Some((5.0, 145.0));
        assert!(presentation.accept(generation, size, 1.0, false));
        presentation.ready = false;
        active.apply_bitmap_hits(None);
        assert!(!active.enabled);
        assert!(active.hit.is_empty() && active.close.is_empty());
        assert!(active.bar_track.is_none() && active.bar_thumb.is_none());
        assert!(active.click.is_none() && active.bar_drag.is_none());
        assert_eq!(active.wheel, 0.0);
        assert!(!presentation.accept(generation, size, 1.0, false));
        assert!(presentation.accept(generation, size, 1.0, true));
        active.apply_bitmap_hits(Some(&staged));
        assert!(active.enabled);
        assert_eq!(active.hit, staged.hit);
    }

    #[test]
    fn bitmap_sc_resize_and_dpi_changes_reject_stale_frames() {
        let size = [960, 540];
        let mut presentation = ScPresentation::default();
        presentation.set_target(Some(sc_target(size, 96)));
        let original = presentation.generation;
        assert!(presentation.accept(original, size, 1.0, true));

        let larger = [1200, 800];
        assert!(presentation.set_target(Some(sc_target(larger, 120))));
        let resized = presentation.generation;
        assert!(!presentation.ready);
        assert!(!presentation.accept(original, size, 1.0, true));
        assert!(!presentation.accept(resized, larger, 1.0, true));
        assert!(presentation.accept(resized, larger, 1.25, true));

        assert!(presentation.set_target(None));
        assert!(presentation.set_target(Some(sc_target(size, 96))));
        assert!(!presentation.accept(original, size, 1.0, true));
        let current = presentation.generation;
        assert!(presentation.accept(current, size, 1.0, true));
        let mut moved = sc_target(size, 96);
        moved.area.left = -960;
        assert!(!presentation.set_target(Some(moved)));
        assert!(presentation.accept(current, size, 1.0, false));
    }

    #[test]
    fn readback_layout_preserves_bgra_and_opengl_row_order() {
        let dib = Dib::new([2, 2]).unwrap();
        let pixels = unsafe { std::slice::from_raw_parts_mut(dib.bits, dib.len) };
        // GL starts with the bottom row. One pixel is already premultiplied.
        pixels.copy_from_slice(&[0, 0, 255, 255, 0, 63, 0, 127, 255, 0, 0, 255, 0, 0, 0, 0]);
        assert_eq!(unsafe { GetPixel(dib.dc, 0, 0) }, 0x00ff0000); // blue at the top
        assert_eq!(unsafe { GetPixel(dib.dc, 0, 1) }, 0x000000ff); // red at the bottom
        assert_eq!(unsafe { GetPixel(dib.dc, 1, 1) }, 0x00003f00);
        assert_eq!(pixels[7], 127);
        assert_eq!(pixels[15], 0);
    }

    #[test]
    fn invalid_or_overflowing_surface_size_is_rejected_before_allocation() {
        for size in [[0, 8], [8, 0], [-2, 8], [65536, 65536]] {
            assert!(Dib::new(size).is_err());
        }
    }
}
