use super::super::{Hwnd, Rect};
use anyhow::{Context, Result, bail, ensure};
use eframe::glow::{self, HasContext};
use std::{ffi::{CStr, c_char, c_void}, io, ptr::null_mut};

type Ptr = *mut c_void;
type Hresult = i32;
const BGRA8: u32 = 87; // DXGI_FORMAT_B8G8R8A8_UNORM
const D3D11_SDK_VERSION: u32 = 7;
const D3D11_CREATE_DEVICE_BGRA_SUPPORT: u32 = 0x20;
const D3D11_BIND_SHADER_RESOURCE_RENDER_TARGET: u32 = 0x28;
const DXGI_ALPHA_MODE_PREMULTIPLIED: u32 = 1;
const WGL_ACCESS_READ_WRITE_NV: u32 = 1;

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);
const DXGI_DEVICE: Guid = Guid(0x54ec77fa, 0x1377, 0x44e6, [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c]);
const DCOMP_DEVICE: Guid = Guid(0xc37ea93a, 0xe7aa, 0x450d, [0xb1, 0x6f, 0x97, 0x46, 0xcb, 0x04, 0x07, 0xf3]);
const D3D_TEXTURE_2D: Guid = Guid(0x6f15aaf2, 0xd208, 0x4e89, [0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c]);

#[repr(C)]
#[derive(Default)]
struct Point { x: i32, y: i32 }

#[repr(C)]
#[derive(Default)]
struct TextureDesc {
    width: u32,
    height: u32,
    mip_levels: u32,
    array_size: u32,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    usage: u32,
    bind_flags: u32,
    cpu_access_flags: u32,
    misc_flags: u32,
}

#[repr(C)]
struct Box3d { left: u32, top: u32, front: u32, right: u32, bottom: u32, back: u32 }

// Windows SDK COM ABI. Placeholder entries preserve the slots of methods
// outside this backend's interface. All queried/created pointers own one ref.
#[repr(C)]
struct UnknownVtbl {
    query: unsafe extern "system" fn(Ptr, *const Guid, *mut Ptr) -> Hresult,
    _add_ref: Ptr,
    release: unsafe extern "system" fn(Ptr) -> u32,
}
#[repr(C)]
struct D3dDeviceVtbl {
    _base: UnknownVtbl,
    _create_buffer_and_texture_1d: [Ptr; 2],
    create_texture_2d: unsafe extern "system" fn(Ptr, *const TextureDesc, Ptr, *mut Ptr) -> Hresult,
    _through_get_creation_flags: [Ptr; 33],
    removed_reason: unsafe extern "system" fn(Ptr) -> Hresult,
}
#[repr(C)]
struct D3dContextVtbl {
    _through_rs_set_scissor_rects: [Ptr; 46],
    copy_region: unsafe extern "system" fn(Ptr, Ptr, u32, u32, u32, u32, Ptr, u32, *const Box3d),
    _copy_resource: Ptr,
    update_subresource: unsafe extern "system" fn(Ptr, Ptr, u32, *const Box3d, *const c_void, u32, u32),
}
#[repr(C)]
struct TextureVtbl {
    _through_get_eviction_priority: [Ptr; 10],
    get_desc: unsafe extern "system" fn(Ptr, *mut TextureDesc),
}
#[repr(C)]
struct CompositionDeviceVtbl {
    _base: UnknownVtbl,
    commit: unsafe extern "system" fn(Ptr) -> Hresult,
    _wait_and_frame_statistics: [Ptr; 2],
    create_target: unsafe extern "system" fn(Ptr, Hwnd, i32, *mut Ptr) -> Hresult,
    create_visual: unsafe extern "system" fn(Ptr, *mut Ptr) -> Hresult,
    create_surface: unsafe extern "system" fn(Ptr, u32, u32, u32, u32, *mut Ptr) -> Hresult,
    _through_create_rectangle_clip: [Ptr; 16],
    create_animation: unsafe extern "system" fn(Ptr, *mut Ptr) -> Hresult,
    check_device_state: unsafe extern "system" fn(Ptr, *mut i32) -> Hresult,
}
#[repr(C)]
struct TargetVtbl {
    _base: UnknownVtbl,
    set_root: unsafe extern "system" fn(Ptr, Ptr) -> Hresult,
}
#[repr(C)]
struct VisualVtbl {
    _base: UnknownVtbl,
    animate_x: unsafe extern "system" fn(Ptr, Ptr) -> Hresult,
    offset_x: unsafe extern "system" fn(Ptr, f32) -> Hresult,
    _animate_y: Ptr,
    offset_y: unsafe extern "system" fn(Ptr, f32) -> Hresult,
    _through_object_clip: [Ptr; 7],
    rect_clip: unsafe extern "system" fn(Ptr, *const [f32; 4]) -> Hresult,
    set_content: unsafe extern "system" fn(Ptr, Ptr) -> Hresult,
    add_visual: unsafe extern "system" fn(Ptr, Ptr, i32, Ptr) -> Hresult,
    remove_visual: unsafe extern "system" fn(Ptr, Ptr) -> Hresult,
}
#[repr(C)]
struct SurfaceVtbl {
    _base: UnknownVtbl,
    begin_draw: unsafe extern "system" fn(Ptr, *const Rect, *const Guid, *mut Ptr, *mut Point) -> Hresult,
    end_draw: unsafe extern "system" fn(Ptr) -> Hresult,
}

type CreateD3dDevice = unsafe extern "system" fn(
    Ptr, u32, Ptr, u32, *const u32, u32, u32, *mut Ptr, *mut u32, *mut Ptr,
) -> Hresult;
type CreateCompositionDevice = unsafe extern "system" fn(Ptr, *const Guid, *mut Ptr) -> Hresult;
type FarProc = unsafe extern "system" fn() -> isize;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: Ptr, flags: u32) -> Ptr;
    fn GetProcAddress(module: Ptr, symbol: *const c_char) -> Option<FarProc>;
    fn FreeLibrary(module: Ptr) -> i32;
}

struct SystemDll(Ptr);

impl SystemDll {
    fn load(name: &str) -> Result<Self> {
        const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
        let wide_name: Vec<_> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { LoadLibraryExW(wide_name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return Err(io::Error::last_os_error()).with_context(|| format!("load {name} from System32"));
        }
        Ok(Self(module))
    }

    fn symbol(&self, name: &CStr) -> Result<FarProc> {
        unsafe { GetProcAddress(self.0, name.as_ptr()) }.ok_or_else(|| {
            anyhow::Error::from(io::Error::last_os_error())
                .context(format!("GetProcAddress({})", name.to_string_lossy()))
        })
    }
}

impl Drop for SystemDll {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}
#[link(name = "opengl32")]
unsafe extern "system" {
    fn wglGetCurrentContext() -> Ptr;
    fn wglGetCurrentDC() -> Ptr;
    fn wglGetProcAddress(name: *const c_char) -> Ptr;
}

fn hr(status: Hresult, operation: &str) -> Result<()> {
    ensure!(status >= 0, "{operation} failed (HRESULT {:#010x})", status as u32);
    Ok(())
}

#[derive(Default)]
struct Com(Ptr);

impl Com {
    fn create(operation: &str, call: impl FnOnce(*mut Ptr) -> Hresult) -> Result<Self> {
        let mut output = Self::default();
        hr(call(&mut output.0), operation)?;
        output.validate(operation)?;
        Ok(output)
    }

    fn validate(&self, operation: &str) -> Result<()> {
        ensure!(!self.0.is_null(), "{operation} returned a null interface");
        Ok(())
    }

    // The caller selects the vtable for the IID that created this object.
    unsafe fn vtable<V>(&self) -> &V {
        unsafe { &**self.0.cast::<*const V>() }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.vtable::<UnknownVtbl>().release)(self.0) };
        }
    }
}

struct Composition { target: Com, visual: Com, device: Com }

impl Composition {
    fn new(d3d: &Com, window: Hwnd, create_device: CreateCompositionDevice) -> Result<Self> {
        let dxgi = Com::create("QueryInterface(IDXGIDevice)", |out| unsafe {
            (d3d.vtable::<UnknownVtbl>().query)(d3d.0, &DXGI_DEVICE, out)
        })?;
        let device = Com::create("DCompositionCreateDevice", |out| unsafe {
            create_device(dxgi.0, &DCOMP_DEVICE, out)
        })?;
        let methods = unsafe { device.vtable::<CompositionDeviceVtbl>() };
        let target = Com::create("CreateTargetForHwnd", |out| unsafe {
            (methods.create_target)(device.0, window, 1, out)
        })?;
        let visual = Com::create("CreateVisual", |out| unsafe { (methods.create_visual)(device.0, out) })?;
        let composition = Self { target, visual, device };
        composition.set_root(composition.visual.0)?;
        Ok(composition)
    }

    fn set_root(&self, visual: Ptr) -> Result<()> {
        hr(unsafe { (self.target.vtable::<TargetVtbl>().set_root)(self.target.0, visual) }, "SetRoot")
    }

    fn set_content(&self, surface: &Com) -> Result<()> {
        hr(unsafe { (self.visual.vtable::<VisualVtbl>().set_content)(self.visual.0, surface.0) }, "SetContent")
    }

    fn commit(&self) -> Result<()> {
        hr(unsafe { (self.device.vtable::<CompositionDeviceVtbl>().commit)(self.device.0) }, "DirectComposition Commit")
    }
}

impl Drop for Composition {
    fn drop(&mut self) {
        if self.set_root(null_mut()).is_ok() {
            let _ = self.commit();
        }
    }
}

type WglRegister = unsafe extern "system" fn(Ptr, Ptr, u32, u32, u32) -> Ptr;
type WglUnregister = unsafe extern "system" fn(Ptr, Ptr) -> i32;
type WglLock = unsafe extern "system" fn(Ptr, i32, *mut Ptr) -> i32;
type WglClose = unsafe extern "system" fn(Ptr) -> i32;

fn wgl_proc(name: &CStr) -> Result<Ptr> {
    let address = unsafe { wglGetProcAddress(name.as_ptr()) };
    ensure!(!matches!(address as usize, 0..=3 | usize::MAX), "missing WGL function {}", name.to_string_lossy());
    Ok(address)
}

#[derive(Clone, Copy)]
struct WglFunctions {
    register: WglRegister,
    unregister: WglUnregister,
    lock: WglLock,
    unlock: WglLock,
    close: WglClose,
}

struct Interop { context: Ptr, handle: Ptr, fns: WglFunctions }

impl Interop {
    fn new(d3d: &Com) -> Result<Self> {
        let context = unsafe { wglGetCurrentContext() };
        let dc = unsafe { wglGetCurrentDC() };
        ensure!(!context.is_null() && !dc.is_null(), "no current WGL context for GPU presentation");
        let (extensions, fns, open) = unsafe {
            let extensions = if let Ok(proc) = wgl_proc(c"wglGetExtensionsStringARB") {
                let get: unsafe extern "system" fn(Ptr) -> *const c_char = std::mem::transmute(proc);
                get(dc)
            } else {
                let proc = wgl_proc(c"wglGetExtensionsStringEXT")?;
                let get: unsafe extern "system" fn() -> *const c_char = std::mem::transmute(proc);
                get()
            };
            let fns = WglFunctions {
                register: std::mem::transmute::<Ptr, WglRegister>(wgl_proc(c"wglDXRegisterObjectNV")?),
                unregister: std::mem::transmute::<Ptr, WglUnregister>(wgl_proc(c"wglDXUnregisterObjectNV")?),
                lock: std::mem::transmute::<Ptr, WglLock>(wgl_proc(c"wglDXLockObjectsNV")?),
                unlock: std::mem::transmute::<Ptr, WglLock>(wgl_proc(c"wglDXUnlockObjectsNV")?),
                close: std::mem::transmute::<Ptr, WglClose>(wgl_proc(c"wglDXCloseDeviceNV")?),
            };
            let open: unsafe extern "system" fn(Ptr) -> Ptr = std::mem::transmute(wgl_proc(c"wglDXOpenDeviceNV")?);
            (extensions, fns, open)
        };
        ensure!(!extensions.is_null() && unsafe { CStr::from_ptr(extensions) }.to_bytes()
            .split(|&byte| byte == b' ').any(|name| name == b"WGL_NV_DX_interop2"), "current WGL context lacks WGL_NV_DX_interop2");
        let handle = unsafe { open(d3d.0) };
        if handle.is_null() {
            return Err(io::Error::last_os_error()).context("wglDXOpenDeviceNV");
        }
        Ok(Self { context, handle, fns })
    }

    fn is_current(&self) -> bool { self.context == unsafe { wglGetCurrentContext() } }
}

impl Drop for Interop {
    fn drop(&mut self) {
        if self.is_current() {
            unsafe { (self.fns.close)(self.handle) };
        }
    }
}

struct Registration {
    context: Ptr,
    device: Ptr,
    handle: Ptr,
    fns: WglFunctions,
    locked: bool,
}

impl Registration {
    fn lock(&mut self) -> Result<InteropLock<'_>> {
        ensure!(!self.locked, "WGL/DX texture is still locked");
        let mut handle = self.handle;
        if unsafe { (self.fns.lock)(self.device, 1, &mut handle) } == 0 {
            return Err(io::Error::last_os_error()).context("wglDXLockObjectsNV");
        }
        self.locked = true;
        Ok(InteropLock(self))
    }

    fn unlock(&mut self) -> Result<()> {
        if self.locked {
            let mut handle = self.handle;
            if unsafe { (self.fns.unlock)(self.device, 1, &mut handle) } == 0 {
                return Err(io::Error::last_os_error()).context("wglDXUnlockObjectsNV");
            }
            self.locked = false;
        }
        Ok(())
    }

    fn unregister(&mut self) -> Result<()> {
        if !self.handle.is_null() {
            self.unlock()?;
            if unsafe { (self.fns.unregister)(self.device, self.handle) } == 0 {
                return Err(io::Error::last_os_error()).context("wglDXUnregisterObjectNV");
            }
            self.handle = null_mut();
        }
        Ok(())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.context == unsafe { wglGetCurrentContext() } {
            let _ = self.unregister();
        }
    }
}

struct InteropLock<'a>(&'a mut Registration);

impl InteropLock<'_> {
    fn finish(self) -> Result<()> { self.0.unlock() }
}

impl Drop for InteropLock<'_> {
    fn drop(&mut self) { let _ = self.0.unlock(); }
}

struct SurfaceDraw<'a> { surface: &'a Com, texture: Com, offset: Point, active: bool }

impl<'a> SurfaceDraw<'a> {
    fn begin(surface: &'a Com, region: &Rect) -> Result<Self> {
        let mut draw = Self { surface, texture: Com::default(), offset: Point::default(), active: false };
        hr(unsafe {
            (surface.vtable::<SurfaceVtbl>().begin_draw)(surface.0, region, &D3D_TEXTURE_2D, &mut draw.texture.0, &mut draw.offset)
        }, "DirectComposition BeginDraw")?;
        draw.active = true;
        draw.texture.validate("DirectComposition BeginDraw")?;
        Ok(draw)
    }

    fn finish(&mut self) -> Result<()> {
        if std::mem::take(&mut self.active) {
            drop(std::mem::take(&mut self.texture));
            hr(unsafe { (self.surface.vtable::<SurfaceVtbl>().end_draw)(self.surface.0) }, "DirectComposition EndDraw")?;
        }
        Ok(())
    }

    fn copy(&self, immediate: &Com, source: &Com, rect: Rect) -> Result<()> {
        let mut desc = TextureDesc::default();
        unsafe { (self.texture.vtable::<TextureVtbl>().get_desc)(self.texture.0, &mut desc) };
        let Point { x, y } = self.offset;
        ensure!(x >= 0 && y >= 0 && desc.format == BGRA8 && desc.sample_count == 1
            && x as u64 + (rect.right - rect.left) as u64 <= desc.width as u64
            && y as u64 + (rect.bottom - rect.top) as u64 <= desc.height as u64,
            "DirectComposition update exceeds the destination texture or has an incompatible format");
        let area = Box3d { left: rect.left as u32, top: rect.top as u32, front: 0,
            right: rect.right as u32, bottom: rect.bottom as u32, back: 1 };
        unsafe {
            (immediate.vtable::<D3dContextVtbl>().copy_region)(immediate.0, self.texture.0, 0, x as u32, y as u32, 0, source.0, 0, &area);
        }
        Ok(())
    }
}

impl Drop for SurfaceDraw<'_> {
    fn drop(&mut self) { let _ = self.finish(); }
}

struct Frame {
    registration: Registration,
    texture: Com,
    surface: Com,
    gl_texture: glow::Texture,
    gl_framebuffer: glow::Framebuffer,
    size: [i32; 2],
    needs_full: bool,
    attached: bool,
}

impl Frame {
    fn new(gl: &glow::Context, interop: &Interop, device: &Com, composition: &Composition, size: [i32; 2]) -> Result<Self> {
        let [width, height] = size.map(|n| n as u32);
        let texture = Com::create("CreateTexture2D", |out| unsafe {
            let desc = TextureDesc { width, height, mip_levels: 1, array_size: 1, format: BGRA8, sample_count: 1,
                bind_flags: D3D11_BIND_SHADER_RESOURCE_RENDER_TARGET, ..TextureDesc::default() };
            (device.vtable::<D3dDeviceVtbl>().create_texture_2d)(device.0, &desc, null_mut(), out)
        })?;
        let surface = Com::create("DirectComposition CreateSurface", |out| unsafe {
            (composition.device.vtable::<CompositionDeviceVtbl>().create_surface)(composition.device.0, width, height, BGRA8, DXGI_ALPHA_MODE_PREMULTIPLIED, out)
        })?;
        unsafe {
            let gl_texture = gl.create_texture().map_err(anyhow::Error::msg).context("create WGL interop GL texture")?;
            let gl_framebuffer = match gl.create_framebuffer() {
                Ok(framebuffer) => framebuffer,
                Err(error) => {
                    gl.delete_texture(gl_texture);
                    bail!("create WGL interop framebuffer: {error}");
                }
            };
            let handle = (interop.fns.register)(interop.handle, texture.0, gl_texture.0.get(), glow::TEXTURE_2D, WGL_ACCESS_READ_WRITE_NV);
            if handle.is_null() {
                let error = io::Error::last_os_error();
                gl.delete_framebuffer(gl_framebuffer);
                gl.delete_texture(gl_texture);
                return Err(error).context("wglDXRegisterObjectNV");
            }
            let registration = Registration { context: interop.context, device: interop.handle, handle, fns: interop.fns, locked: false };
            Ok(Self { registration, texture, surface, gl_texture, gl_framebuffer, size, needs_full: true, attached: false })
        }
    }

    fn delete_gl(&mut self, gl: &glow::Context) -> Result<()> {
        self.registration.unregister()?;
        unsafe {
            gl.delete_framebuffer(self.gl_framebuffer);
            gl.delete_texture(self.gl_texture);
        }
        Ok(())
    }

    fn flip(&mut self, gl: &glow::Context, source: glow::Framebuffer, rect: Rect) -> Result<()> {
        let lock = self.registration.lock()?;
        let copied = unsafe {
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.gl_framebuffer));
            gl.framebuffer_texture_2d(glow::DRAW_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.gl_texture), 0);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(source));
            let read_status = gl.check_framebuffer_status(glow::READ_FRAMEBUFFER);
            let draw_status = gl.check_framebuffer_status(glow::DRAW_FRAMEBUFFER);
            let complete = read_status == glow::FRAMEBUFFER_COMPLETE && draw_status == glow::FRAMEBUFFER_COMPLETE;
            if complete {
                let scissor = gl.is_enabled(glow::SCISSOR_TEST);
                gl.disable(glow::SCISSOR_TEST);
                let height = self.size[1];
                gl.blit_framebuffer(rect.left, height - rect.bottom, rect.right, height - rect.top,
                    rect.left, rect.bottom, rect.right, rect.top, glow::COLOR_BUFFER_BIT, glow::NEAREST);
                if scissor {
                    gl.enable(glow::SCISSOR_TEST);
                }
            }
            // Release the shared draw binding before handing ownership to D3D.
            // The parent rebinds its framebuffer after submit.
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
            let error = gl.get_error();
            (complete, read_status, draw_status, error)
        };
        lock.finish()?;
        let (complete, read_status, draw_status, error) = copied;
        ensure!(complete, "incomplete WGL/DX framebuffer (read {read_status:#x}, draw {draw_status:#x})");
        ensure!(error == glow::NO_ERROR, "OpenGL framebuffer flip failed ({error:#x})");
        Ok(())
    }
}

struct State {
    // Drop the registration before its WGL device and D3D resources.
    frame: Option<Frame>,
    interop: Option<Interop>,
    composition: Composition,
    immediate: Com,
    device: Com,
    // Keep both modules loaded through COM teardown, including the final Release.
    _composition_dll: SystemDll,
    _d3d_dll: SystemDll,
}

pub(super) struct GpuPresenter { state: Option<State> }

impl State {
    fn new(window: Hwnd) -> Result<Self> {
        ensure!(!window.is_null(), "GPU presentation requires a valid HWND");
        let d3d_dll = SystemDll::load("d3d11.dll")?;
        let composition_dll = SystemDll::load("dcomp.dll")?;
        let create_d3d = unsafe {
            std::mem::transmute::<FarProc, CreateD3dDevice>(d3d_dll.symbol(c"D3D11CreateDevice")?)
        };
        let create_composition = unsafe {
            std::mem::transmute::<FarProc, CreateCompositionDevice>(
                composition_dll.symbol(c"DCompositionCreateDevice")?,
            )
        };
        let (mut device, mut immediate) = (Com::default(), Com::default());
        hr(unsafe {
            create_d3d(null_mut(), 1, null_mut(), D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                std::ptr::null(), 0, D3D11_SDK_VERSION, &mut device.0, null_mut(), &mut immediate.0)
        }, "D3D11CreateDevice")?;
        device.validate("D3D11CreateDevice (device)")?;
        immediate.validate("D3D11CreateDevice (immediate context)")?;
        let composition = Composition::new(&device, window, create_composition)?;
        Ok(Self { frame: None, interop: None, composition, immediate, device, _composition_dll: composition_dll, _d3d_dll: d3d_dll })
    }

}

impl GpuPresenter {
    pub(super) fn new(window: Hwnd) -> Result<Self> {
        Ok(Self { state: Some(State::new(window)?) })
    }

    pub(super) fn submit(&mut self, gl: &glow::Context, private_framebuffer: glow::Framebuffer, size: [i32; 2], dirty: Option<Rect>) -> Result<()> {
        let state = self.state.as_mut().context("GPU presenter has been destroyed")?;
        let [width, height] = size;
        ensure!((1..=16384).contains(&width) && (1..=16384).contains(&height), "invalid GPU surface size {width}x{height}");
        if state.interop.is_none() {
            state.interop = Some(Interop::new(&state.device)?);
        }
        let interop = state.interop.as_ref().unwrap();
        ensure!(interop.is_current(), "GPU presentation needs its original WGL context current");
        if state.frame.as_ref().is_none_or(|frame| frame.size != size) {
            if let Some(old) = state.frame.as_mut() {
                old.delete_gl(gl)?;
                state.frame = None;
            }
            state.frame = Some(Frame::new(gl, interop, &state.device, &state.composition, size)?);
        }
        let frame = state.frame.as_mut().unwrap();
        let full = Rect { left: 0, top: 0, right: width, bottom: height };
        let rect = if std::mem::replace(&mut frame.needs_full, true) {
            full
        } else {
            match dirty {
                Some(rect) => Rect { left: rect.left.clamp(0, width), top: rect.top.clamp(0, height),
                    right: rect.right.clamp(0, width), bottom: rect.bottom.clamp(0, height) },
                None => full,
            }
        };
        ensure!(rect.left < rect.right && rect.top < rect.bottom, "empty GPU surface update rectangle");
        frame.flip(gl, private_framebuffer, rect)?;
        let mut draw = SurfaceDraw::begin(&frame.surface, &rect)?;
        draw.copy(&state.immediate, &frame.texture, rect)?;
        draw.finish()?;
        hr(unsafe { (state.device.vtable::<D3dDeviceVtbl>().removed_reason)(state.device.0) }, "D3D11 GetDeviceRemovedReason")?;
        if !frame.attached {
            state.composition.set_content(&frame.surface)?;
        }
        state.composition.commit()?;
        frame.attached = true;
        frame.needs_full = false;
        Ok(())
    }

    pub(super) fn destroy(&mut self, gl: &glow::Context) {
        if let Some(mut state) = self.state.take() {
            if state.interop.as_ref().is_some_and(Interop::is_current) {
                if let Some(frame) = &mut state.frame {
                    let _ = frame.delete_gl(gl);
                }
            }
        }
    }
}

#[path = "overlay_dcomp_sprites.rs"]
mod sprites;
pub(super) use sprites::{SpritePresenter, Track};
