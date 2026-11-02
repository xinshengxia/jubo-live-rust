//! Opt-in public Huya image acceptance; no account, room, player or UI involved.
use live_hub_rust::{danmaku_huya, images::ImageLoader};
use std::{sync::mpsc, time::Duration};

#[test]
#[ignore = "public CDN network test; opt in with LIVE_HUB_LIVE_ACCEPTANCE=1"]
fn every_huya_emote_downloads_and_decodes_through_the_production_loader() {
    if std::env::var("LIVE_HUB_LIVE_ACCEPTANCE").as_deref() != Ok("1") {
        panic!("set LIVE_HUB_LIVE_ACCEPTANCE=1 to enable public CDN requests");
    }
    let dictionary = danmaku_huya::emoticon_dictionary();
    let loader = ImageLoader::new();
    for (code, url) in dictionary.iter() {
        let (sender, receiver) = mpsc::channel();
        let image = loader.request(url, move || {
            let _ = sender.send(());
        });
        let image = image
            .or_else(|| {
                receiver.recv_timeout(Duration::from_secs(20)).expect(code);
                loader.request(url, || {})
            })
            .unwrap_or_else(|| panic!("{code}: production download/decode failed"));
        assert!(image.width > 0 && image.height > 0, "{code}");
        assert_eq!(image.rgba.len(), image.width * image.height * 4, "{code}");
        assert!(
            image.rgba.chunks_exact(4).any(|pixel| pixel[3] != 0),
            "{code}"
        );
        println!("{code}: {}x{} RGBA OK", image.width, image.height);
    }
    println!("verified {} Huya emotes", dictionary.len());
}
