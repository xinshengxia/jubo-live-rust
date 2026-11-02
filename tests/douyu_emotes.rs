use live_hub_rust::{
    danmaku::rich::{TextSegment, split_text_segments},
    danmaku_douyu_emoticons, images,
};

#[test]
fn douyu_fixed_package_reaches_image_segments_and_preserves_unknown_text() {
    let dictionary = danmaku_douyu_emoticons::dictionary();
    assert_eq!(dictionary.len(), 250);
    assert!(
        dictionary
            .get("[顽皮]")
            .unwrap()
            .ends_with("wanpi_20e1a62.png")
    );
    for (code, url) in dictionary.iter() {
        assert!(images::approved_url(url).is_ok(), "{code}");
        assert_eq!(
            split_text_segments(code, Some(&dictionary), None),
            vec![TextSegment::Emoticon {
                code: code.into(),
                url: url.into()
            },]
        );
    }
    let segments = split_text_segments("开始[顽皮][顽皮]结束[合成未知]", Some(&dictionary), None);
    assert_eq!(segments.len(), 4);
    assert_eq!(segments[0], TextSegment::Text("开始".into()));
    assert_eq!(segments[3], TextSegment::Text("结束[合成未知]".into()));
}
