//! Bounded asynchronous image loading. No cookies, disk cache or UI-thread I/O.
use crate::network;
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use url::Url;
pub const MAX_BODY: usize = 2 * 1024 * 1024;
pub const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
const FAMILIES: &[&str] = &[
    "hdslb.com",
    "biliimg.com",
    "huyaimg.com",
    "msstatic.com",
    "douyucdn.cn",
    "douyucdn.com",
    "douyinpic.com",
    "douyinstatic.com",
    "byteimg.com",
    "pstatp.com",
    "snssdk.com",
];
pub fn approved_url(address: &str) -> Result<Url> {
    network::approved_url(address, &[], FAMILIES)
}
#[derive(Clone)]
pub struct ImageData {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}
enum Entry {
    Pending,
    Ready(Arc<ImageData>),
    Failed(Instant),
}
struct Item {
    used: u64,
    entry: Entry,
}
#[derive(Default)]
struct State {
    clock: u64,
    items: BTreeMap<String, Item>,
}
#[derive(Clone, Default)]
pub struct ImageLoader {
    state: Arc<Mutex<State>>,
    workers: Arc<AtomicUsize>,
}
impl ImageLoader {
    pub fn new() -> Self {
        Self::default()
    }
    #[allow(deprecated)] // Preserve compatibility before Atomic::try_update's rename.
    pub fn request(
        &self,
        address: &str,
        notify: impl FnOnce() + Send + 'static,
    ) -> Option<Arc<ImageData>> {
        let url = network::approved_url(address, &[], FAMILIES).ok()?;
        let key = url.to_string();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let tick = state.clock;
        if let Some(item) = state.items.get_mut(&key) {
            item.used = tick;
            match &item.entry {
                Entry::Ready(image) => return Some(image.clone()),
                Entry::Pending => return None,
                Entry::Failed(at) if at.elapsed() < Duration::from_secs(30) => return None,
                _ => (),
            }
        }
        if self
            .workers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            return None;
        }
        state.items.insert(
            key.clone(),
            Item {
                used: tick,
                entry: Entry::Pending,
            },
        );
        drop(state);
        let shared = self.state.clone();
        let workers = self.workers.clone();
        let failed_key = key.clone();
        let failed_state = self.state.clone();
        let failed_workers = self.workers.clone();
        let spawned = thread::Builder::new()
            .name("public-image".into())
            .spawn(move || {
                let result = download(url);
                let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                state.clock = state.clock.wrapping_add(1);
                let tick = state.clock;
                state.items.insert(
                    key,
                    Item {
                        used: tick,
                        entry: match result {
                            Ok(image) => Entry::Ready(Arc::new(image)),
                            Err(_) => Entry::Failed(Instant::now()),
                        },
                    },
                );
                evict(&mut state);
                drop(state);
                workers.fetch_sub(1, Ordering::AcqRel);
                notify();
            });
        if spawned.is_err() {
            failed_workers.fetch_sub(1, Ordering::AcqRel);
            failed_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .items
                .remove(&failed_key);
        }
        None
    }
}
fn evict(state: &mut State) {
    loop {
        let bytes: usize = state
            .items
            .values()
            .map(|i| match &i.entry {
                Entry::Ready(image) => image.rgba.len(),
                _ => 0,
            })
            .sum();
        if bytes <= MAX_CACHE_BYTES && state.items.len() <= 200 {
            break;
        }
        let oldest = state
            .items
            .iter()
            .filter(|(_, item)| !matches!(item.entry, Entry::Pending))
            .min_by_key(|(_, i)| i.used)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            state.items.remove(&key);
        } else {
            break;
        }
    }
}
fn download(mut url: Url) -> Result<ImageData> {
    for _ in 0..=3 {
        let client = network::client(&url, &[], FAMILIES, Duration::from_secs(6))?;
        let response = client
            .get(url.clone())
            .send()
            .map_err(|_| anyhow!("图片请求失败"))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|h| h.to_str().ok())
                .ok_or_else(|| anyhow!("图片重定向无效"))?;
            url = network::approved_url(url.join(location)?.as_str(), &[], FAMILIES)?;
            continue;
        }
        ensure!(response.status().is_success(), "图片暂不可用");
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= MAX_BODY as u64),
            "图片超过限额"
        );
        let mut bytes = Vec::new();
        response.take(MAX_BODY as u64 + 1).read_to_end(&mut bytes)?;
        return decode(&bytes);
    }
    Err(anyhow!("图片重定向次数超限"))
}
pub fn decode(bytes: &[u8]) -> Result<ImageData> {
    ensure!(bytes.len() <= MAX_BODY, "图片超过限额");
    // Reject oversized containers before the decoder allocates pixels; the
    // decoder limits below stay as the second line of defence.
    ensure!(
        crate::danmaku::rich::image_within_limits(bytes),
        "图片尺寸无法识别或超过限额"
    );
    decode_bounded(bytes, 2048, 2048, 24 * 1024 * 1024, true)
}

/// Decodes a local colour-emoji sprite sheet: larger container limits and no
/// thumbnail step, because cell UVs are computed from the original geometry.
pub fn decode_sheet(bytes: &[u8]) -> Result<ImageData> {
    ensure!(bytes.len() <= MAX_SHEET_BYTES, "表情精灵图超过限额");
    decode_bounded(bytes, 8192, 8192, 96 * 1024 * 1024, false)
}

pub const MAX_SHEET_BYTES: usize = 32 * 1024 * 1024;

fn decode_bounded(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
    thumbnail: bool,
) -> Result<ImageData> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_width);
    limits.max_image_height = Some(max_height);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    let decoded = reader.decode()?;
    let decoded = if thumbnail && (decoded.width() > 256 || decoded.height() > 256) {
        decoded.thumbnail(256, 256)
    } else {
        decoded
    };
    let decoded = decoded.into_rgba8();
    let (width, height) = decoded.dimensions();
    Ok(ImageData {
        width: width as usize,
        height: height as usize,
        rgba: decoded.into_raw(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    #[test]
    fn png_and_webp_decode_without_a_network_or_ui() {
        for webp in [false, true] {
            let mut bytes = Vec::new();
            let pixels = [255, 0, 0, 255, 0, 255, 0, 255];
            if webp {
                image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
                    .write_image(&pixels, 2, 1, image::ExtendedColorType::Rgba8)
                    .unwrap()
            } else {
                image::codecs::png::PngEncoder::new(&mut bytes)
                    .write_image(&pixels, 2, 1, image::ExtendedColorType::Rgba8)
                    .unwrap()
            }
            let decoded = decode(&bytes).unwrap();
            assert_eq!((decoded.width, decoded.height), (2, 1));
            assert_eq!(decoded.rgba, pixels);
        }
        assert!(decode(b"<html>not an image</html>").is_err());
    }
    #[test]
    fn sheet_decoding_keeps_the_full_image_and_limits() {
        let mut bytes = Vec::new();
        let pixels = vec![7u8; 40 * 40 * 4];
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&pixels, 40, 40, image::ExtendedColorType::Rgba8)
            .unwrap();
        let sheet = decode_sheet(&bytes).unwrap();
        assert_eq!((sheet.width, sheet.height), (40, 40));
        assert_eq!(sheet.rgba.len(), 40 * 40 * 4);
        assert!(decode_sheet(b"<html>").is_err());
    }

    #[test]
    fn large_dimensions_are_rejected_before_resizing() {
        let mut bytes = Vec::new();
        let pixels = vec![0u8; 4096 * 4];
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&pixels, 4096, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert!(decode(&bytes).is_err());
    }
    #[test]
    fn lru_budget_releases_cache_but_keeps_borrowers_alive() {
        let mut state = State::default();
        let image = Arc::new(ImageData {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        });
        for n in 0..250 {
            state.items.insert(
                n.to_string(),
                Item {
                    used: n,
                    entry: Entry::Ready(image.clone()),
                },
            );
        }
        evict(&mut state);
        assert_eq!(state.items.len(), 200);
        assert_eq!(image.rgba, [1, 2, 3, 255]);
    }
}
