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
            Ok(Self { fbo, color, dib })
        }
    }

    fn delete(self, gl: &glow::Context) {
        unsafe {
            gl.delete_renderbuffer(self.color);
            gl.delete_framebuffer(self.fbo);
        }
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
}

static CLOCK: OnceLock<Instant> = OnceLock::new();
static SUBMITTED: AtomicU64 = AtomicU64::new(0);
static LAST_MS: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);
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
        if unsafe { IsWindowVisible(self.window) } == 0 {
            return Ok(());
        }
        if !client_area(self.window).is_some_and(|area| [area.width, area.height] == size) {
            return Ok(());
        }
        if self.frame.as_ref().map(|f| f.dib.size) != Some(size) {
            let next = Framebuffer::new(gl, size)?;
            if let Some(old) = self.frame.replace(next) {
                old.delete(gl);
            }
            self.cleared_while_hidden = false;
            let line = format!("layered framebuffer ready: {}x{}", size[0], size[1]);
            super::super::overlay_log(crate::diag::Level::Info, &line);
            diag_log(&line);
        } else if self.content_hidden && self.cleared_while_hidden {
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

    fn present(&mut self, gl: &glow::Context) -> anyhow::Result<()> {
        if !std::mem::take(&mut self.painting) {
            return Ok(());
        }
        let dib = &mut self.frame.as_mut().unwrap().dib;
        let [w, h] = dib.size;
        // egui_glow blends premultiplied RGB. Windows accepts the same values in BGRA order.
        let bytes = unsafe { std::slice::from_raw_parts_mut(dib.bits, dib.len) };
        let gles = gl.version().is_embedded;
        unsafe {
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
            gl.read_pixels(
                0,
                0,
                w,
                h,
                if gles { glow::RGBA } else { glow::BGRA },
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(bytes)),
            );
        }
        if gles {
            for px in bytes.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
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
            dirty: ptr::null(), // ULW_ALPHA | ULW_EX_NORESIZE
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
    // Both callbacks execute on the same event-loop/GL thread. The locked
    // egui_glow painter restores its shader state between primitives but leaves
    // our draw framebuffer bound until the second callback restores its target.
    for (order, first) in [(egui::Order::Background, true), (egui::Order::Debug, false)] {
        let callback = eframe::egui_glow::CallbackFn::new(move |info, painter| {
            PRESENTER.with(|slot| {
                let mut state = slot.borrow_mut();
                if let Some(p) = state.as_mut() {
                    let gl = painter.gl();
                    let result = if first {
                        p.prepare(gl, info.screen_size_px.map(|v| v as i32))
                    } else {
                        let result = p.present(gl);
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
