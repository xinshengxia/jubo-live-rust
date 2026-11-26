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
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(in crate::overlay) struct Windows {
    pub(super) renderer: isize,
    pub(super) visible: isize,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetDpiForWindow(window: Hwnd) -> u32;
    fn GetClassNameW(window: Hwnd, name: *mut u16, len: i32) -> i32;
    fn UpdateLayeredWindowIndirect(window: Hwnd, update: *const LayeredUpdate) -> i32;
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
    dib: Dib,
    pack: Option<PackBuffer>,
}

impl Framebuffer {
    fn new(gl: &glow::Context, size: [i32; 2]) -> anyhow::Result<Self> {
        let dib = Dib::new(size)?;
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
            let pack = PackBuffer::new(gl, dib.len);
            Ok(Self {
                fbo,
                color,
                dib,
                pack,
            })
        }
    }

    fn delete(self, gl: &glow::Context) {
        unsafe {
            if let Some(pack) = self.pack {
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
    painting: bool,
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
        // WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE.
        // Never call SetLayeredWindowAttributes on this HWND: that disables ULW.
        let window = CreateWindowExW(
            0x0808_00a0,
            name.as_ptr(),
            wide(title).as_ptr(),
            0x8000_0000,
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
                painting: false,
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

impl Presenter {
    fn prepare(&mut self, gl: &glow::Context, size: [i32; 2]) -> anyhow::Result<()> {
        self.painting = false;
        if NEEDS_REPRESENT.swap(false, Ordering::AcqRel) {
            self.previous_scene = None;
        }
        if unsafe { IsWindowVisible(self.window) } == 0 {
            self.previous_scene = None;
            return Ok(());
        }
        if !client_area(self.window).is_some_and(|area| [area.width, area.height] == size) {
            self.previous_scene = None;
            return Ok(());
        }
        if self.frame.as_ref().map(|f| f.dib.size) != Some(size) {
            let next = Framebuffer::new(gl, size)?;
            if let Some(old) = self.frame.replace(next) {
                old.delete(gl);
            }
            self.cleared_while_hidden = false;
            self.previous_scene = None;
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
    ) -> anyhow::Result<()> {
        if !std::mem::take(&mut self.painting) {
            return Ok(());
        }
        let frame = self.frame.as_mut().unwrap();
        let dib = &mut frame.dib;
        let [w, h] = dib.size;
        // egui_glow blends premultiplied RGB. Windows accepts the same values in BGRA order.
        let bytes = unsafe { std::slice::from_raw_parts_mut(dib.bits, dib.len) };
        // Once the retained DIB is touched, only a successful window update
        // establishes a new baseline. A failed/racing submit retries a full frame.
        // During a monitor/DPI transition layout and drawing can briefly disagree.
        let scale_matches = scene.pixels_per_point == actual_scale;
        let previous = self.previous_scene.take().filter(|_| scale_matches);
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
            read_full(gl, bytes, dib.size)?;
        }
        if let Some(audit) = &mut self.audit {
            audit.check(gl, bytes, dib.size, &patches)?;
        }
        let Some(dirty) = patches.iter().copied().reduce(damage::bounds) else {
            self.previous_scene = Some(scene.clone());
            self.cleared_while_hidden = self.content_hidden;
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
        self.previous_scene = scale_matches.then(|| scene.clone());
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
            p.content_hidden = hidden;
            if !hidden {
                p.cleared_while_hidden = false;
            }
        }
    });
}

pub(in crate::overlay) fn schedule(ctx: &egui::Context) {
    if !PRESENTER.with(|s| s.borrow().is_some()) {
        return;
    }
    let rect = ctx.content_rect();
    let scene = Arc::new(Scene::collect(ctx));
    // Both callbacks execute on the same event-loop/GL thread. The locked
    // egui_glow painter restores its shader state between primitives but leaves
    // our draw framebuffer bound until the second callback restores its target.
    for (order, first) in [(egui::Order::Background, true), (egui::Order::Debug, false)] {
        let scene = scene.clone();
        let callback = eframe::egui_glow::CallbackFn::new(move |info, painter| {
            PRESENTER.with(|slot| {
                let mut state = slot.borrow_mut();
                if let Some(p) = state.as_mut() {
                    let gl = painter.gl();
                    let result = if first {
                        p.prepare(gl, info.screen_size_px.map(|v| v as i32))
                    } else {
                        let result = p.present(gl, &scene, info.pixels_per_point);
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
            if let (Some(f), Some(gl)) = (p.frame.take(), gl) {
                f.delete(gl);
            }
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
