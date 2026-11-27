use super::*;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[repr(C)]
struct AnimationVtbl {
    _base: UnknownVtbl,
    _reset: Ptr,
    set_begin: unsafe extern "system" fn(Ptr, i64) -> Hresult,
    add_cubic: unsafe extern "system" fn(Ptr, f64, f32, f32, f32, f32) -> Hresult,
    _sinusoidal_and_repeat: [Ptr; 2],
    end: unsafe extern "system" fn(Ptr, f64, f32) -> Hresult,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(counter: *mut i64) -> i32;
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(in crate::overlay) struct Track {
    pub x: f32,
    pub y: f32,
    pub velocity: f32,
    pub start: f64,
    pub duration: f64,
}

impl Track {
    fn segment(self, age: f64) -> (f32, f64, f32) {
        let age = age.clamp(0.0, self.duration);
        let remaining = self.duration - age;
        (
            self.x + self.velocity * age as f32,
            remaining,
            self.x + self.velocity * self.duration as f32,
        )
    }
}

struct Sprite {
    visual: Com,
    track: Option<Track>,
}

pub(in crate::overlay) struct SpritePresenter {
    sprites: HashMap<String, Sprite>,
    scrolling: Com,
    fixed: Com,
    fixed_surface: Option<([i32; 2], Com)>,
    device: State,
    clip: Option<[f32; 4]>,
    dirty: bool,
}

fn create_visual(device: &Com) -> Result<Com> {
    Com::create("CreateVisual", |out| unsafe {
        (device.vtable::<CompositionDeviceVtbl>().create_visual)(device.0, out)
    })
}

fn add_visual(parent: &Com, child: &Com) -> Result<()> {
    hr(
        unsafe { (parent.vtable::<VisualVtbl>().add_visual)(parent.0, child.0, 1, null_mut()) },
        "AddVisual",
    )
}

fn create_surface(device: &Com, [width, height]: [i32; 2]) -> Result<Com> {
    ensure!(
        (1..=16384).contains(&width) && (1..=16384).contains(&height),
        "invalid composition surface dimensions"
    );
    Com::create("CreateSurface", |out| unsafe {
        (device.vtable::<CompositionDeviceVtbl>().create_surface)(
            device.0,
            width as u32,
            height as u32,
            BGRA8,
            DXGI_ALPHA_MODE_PREMULTIPLIED,
            out,
        )
    })
}

fn update_surface(state: &State, surface: &Com, rect: Rect, bytes: &[u8]) -> Result<()> {
    let width = (rect.right - rect.left) as u32;
    let height = (rect.bottom - rect.top) as u32;
    ensure!(
        bytes.len() as u64 == u64::from(width) * u64::from(height) * 4,
        "invalid composition pixel buffer length"
    );
    let mut draw = SurfaceDraw::begin(surface, &rect)?;
    let mut desc = TextureDesc::default();
    unsafe { (draw.texture.vtable::<TextureVtbl>().get_desc)(draw.texture.0, &mut desc) };
    let Point { x, y } = draw.offset;
    ensure!(
        x >= 0
            && y >= 0
            && desc.format == BGRA8
            && desc.sample_count == 1
            && x as u64 + u64::from(width) <= u64::from(desc.width)
            && y as u64 + u64::from(height) <= u64::from(desc.height),
        "composition update outside destination texture"
    );
    let box_ = Box3d {
        left: x as u32,
        top: y as u32,
        front: 0,
        right: x as u32 + width,
        bottom: y as u32 + height,
        back: 1,
    };
    unsafe {
        (state
            .immediate
            .vtable::<D3dContextVtbl>()
            .update_subresource)(
            state.immediate.0,
            draw.texture.0,
            0,
            &box_,
            bytes.as_ptr().cast(),
            width * 4,
            0,
        );
    }
    draw.finish()
}

fn set_content(visual: &Com, surface: &Com) -> Result<()> {
    hr(
        unsafe { (visual.vtable::<VisualVtbl>().set_content)(visual.0, surface.0) },
        "SetContent",
    )
}

fn set_track(device: &Com, visual: &Com, track: Track, now: f64) -> Result<()> {
    let methods = unsafe { visual.vtable::<VisualVtbl>() };
    hr(
        unsafe { (methods.offset_y)(visual.0, track.y) },
        "SetOffsetY",
    )?;
    let mut qpc = 0;
    ensure!(
        unsafe { QueryPerformanceCounter(&mut qpc) } != 0,
        "cannot read animation clock"
    );
    let (x, remaining, end) = track.segment(now - track.start);
    if track.velocity == 0.0 || remaining <= 0.001 {
        return hr(unsafe { (methods.offset_x)(visual.0, x) }, "SetOffsetX");
    }
    let animation = Com::create("CreateAnimation", |out| unsafe {
        (device.vtable::<CompositionDeviceVtbl>().create_animation)(device.0, out)
    })?;
    let anim = unsafe { animation.vtable::<AnimationVtbl>() };
    hr(
        unsafe { (anim.set_begin)(animation.0, qpc) },
        "SetAbsoluteBeginTime",
    )?;
    hr(
        unsafe { (anim.add_cubic)(animation.0, 0.0, x, track.velocity, 0.0, 0.0) },
        "AddCubic",
    )?;
    hr(
        unsafe { (anim.end)(animation.0, remaining, end) },
        "End animation",
    )?;
    hr(
        unsafe { (methods.animate_x)(visual.0, animation.0) },
        "Animate OffsetX",
    )
}

impl SpritePresenter {
    pub fn new(window: Hwnd) -> Result<Self> {
        let device = State::new(window)?;
        let scrolling = create_visual(&device.composition.device)?;
        let fixed = create_visual(&device.composition.device)?;
        add_visual(&device.composition.visual, &scrolling)?;
        add_visual(&device.composition.visual, &fixed)?;
        Ok(Self {
            sprites: HashMap::new(),
            scrolling,
            fixed,
            fixed_surface: None,
            device,
            clip: None,
            dirty: true,
        })
    }

    pub fn sync(&mut self, ids: &HashSet<&str>, clip: [f32; 4]) -> Result<()> {
        if self.clip != Some(clip) {
            let group = &self.scrolling;
            hr(
                unsafe { (group.vtable::<VisualVtbl>().rect_clip)(group.0, &clip) },
                "SetClip",
            )?;
            self.clip = Some(clip);
            self.dirty = true;
        }
        let remove: Vec<_> = self
            .sprites
            .keys()
            .filter(|id| !ids.contains(id.as_str()))
            .cloned()
            .collect();
        for id in remove {
            let sprite = self.sprites.remove(&id).unwrap();
            hr(
                unsafe {
                    (self.scrolling.vtable::<VisualVtbl>().remove_visual)(
                        self.scrolling.0,
                        sprite.visual.0,
                    )
                },
                "RemoveVisual",
            )?;
            self.dirty = true;
        }
        Ok(())
    }

    pub fn static_surface(&mut self, size: [i32; 2], rect: Rect, bytes: &[u8]) -> Result<()> {
        if self
            .fixed_surface
            .as_ref()
            .is_none_or(|(previous, _)| *previous != size)
        {
            let surface = create_surface(&self.device.composition.device, size)?;
            set_content(&self.fixed, &surface)?;
            self.fixed_surface = Some((size, surface));
        }
        let surface = &self.fixed_surface.as_ref().unwrap().1;
        update_surface(&self.device, surface, rect, bytes)?;
        self.dirty = true;
        Ok(())
    }

    pub fn sprite(
        &mut self,
        id: &str,
        track: Track,
        raster: Option<([i32; 2], &[u8])>,
        now: f64,
        sampled: Instant,
    ) -> Result<()> {
        let device = &self.device.composition.device;
        let sprite = match self.sprites.entry(id.to_owned()) {
            std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                ensure!(raster.is_some(), "sprite requires initial pixels");
                let visual = create_visual(device)?;
                add_visual(&self.scrolling, &visual)?;
                slot.insert(Sprite {
                    visual,
                    track: None,
                })
            }
        };
        if let Some((size, bytes)) = raster {
            let surface = create_surface(device, size)?;
            let rect = Rect {
                left: 0,
                top: 0,
                right: size[0],
                bottom: size[1],
            };
            update_surface(&self.device, &surface, rect, bytes)?;
            set_content(&sprite.visual, &surface)?;
            self.dirty = true;
        }
        if sprite.track != Some(track) {
            set_track(
                device,
                &sprite.visual,
                track,
                now + sampled.elapsed().as_secs_f64(),
            )?;
            sprite.track = Some(track);
            self.dirty = true;
        }
        Ok(())
    }

    pub fn commit(&mut self) -> Result<bool> {
        let device = &self.device.composition.device;
        let mut valid = 0;
        hr(
            unsafe {
                (device.vtable::<CompositionDeviceVtbl>().check_device_state)(device.0, &mut valid)
            },
            "CheckDeviceState",
        )?;
        ensure!(valid != 0, "DirectComposition device is no longer valid");
        if self.dirty {
            self.device.composition.commit()?;
            self.dirty = false;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::Track;

    #[test]
    fn animation_resumes_at_the_scheduler_position_and_clamps_at_the_end() {
        for velocity in [-100.0, 100.0, 0.0] {
            let track = Track {
                x: 700.0,
                y: 0.0,
                velocity,
                start: 7.0,
                duration: 5.0,
            };
            assert_eq!(
                track.segment(2.0),
                (700.0 + velocity * 2.0, 3.0, 700.0 + velocity * 5.0)
            );
            assert_eq!(track.segment(7.0).0, track.segment(7.0).2);
            assert_eq!(track.segment(-1.0).0, 700.0);
        }
    }
}
