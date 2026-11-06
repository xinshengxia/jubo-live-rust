//! Offline contract tests for danmaku_rules (filter / highlight / pin).

use live_hub_rust::danmaku::{DanmakuMessage, DanmakuMode};
use live_hub_rust::danmaku_rules::{Matcher, RuleErrorKind, SimpleRegex, compile, parse_rule};

fn msg(text: &str, user: &str, user_id: u64) -> DanmakuMessage {
    DanmakuMessage {
        image_url: None,
        id: "m".into(),
        user: user.into(),
        text: text.into(),
        color: 0xff_ffff,
        mode: DanmakuMode::Scroll,
        font_size: 25,
        user_id,
        user_level: 0,
        guard_level: 0,
        medal: None,
        image_width: 0,
        image_height: 0,
        image_large: false,
    }
}

#[test]
fn blocked_keywords_are_case_insensitive_contains() {
    let rules = compile(&["广告".into(), "SPAM".into()], &[], &[]);
    assert!(rules.verdict(&msg("这里有广告", "u", 1)).blocked);
    assert!(rules.verdict(&msg("buy spam now", "u", 1)).blocked);
    assert!(!rules.verdict(&msg("正常弹幕", "u", 1)).blocked);
    assert!(rules.errors.is_empty());
}

#[test]
fn user_rules_accept_uid_and_name_prefixes() {
    let rules = compile(&["uid:12345".into(), "name:老观众".into()], &[], &[]);
    assert!(rules.verdict(&msg("hi", "任何人", 12345)).blocked);
    assert!(rules.verdict(&msg("hi", "老观众", 1)).blocked);
    assert!(!rules.verdict(&msg("hi", "老观众2", 1)).blocked);
    // name: is exact; a plain rule matches the message text (legacy keyword list).
    let rules = compile(&["老观众".into()], &[], &[]);
    assert!(rules.verdict(&msg("老观众来了", "观众乙", 1)).blocked);
    assert!(!rules.verdict(&msg("hi", "老观众", 1)).blocked);
}

#[test]
fn highlight_and_pin_verdict_precedence() {
    let rules = compile(&["屏蔽".into()], &["666".into()], &["置顶".into()]);
    let blocked = rules.verdict(&msg("666 置顶 屏蔽", "u", 1));
    assert!(blocked.blocked && !blocked.highlighted && !blocked.pinned);
    let both = rules.verdict(&msg("666 置顶", "u", 1));
    assert!(!both.blocked && both.highlighted && both.pinned);
    let neutral = rules.verdict(&msg("普通", "u", 1));
    assert!(!neutral.blocked && !neutral.highlighted && !neutral.pinned);
}

#[test]
fn regex_rules_use_the_linear_engine_and_report_bad_patterns() {
    // The `regex` crate replaced the old hand-written subset: groups,
    // alternation, classes, repetitions and \d-style escapes now behave like
    // the legacy .NET patterns instead of being rejected or read as literals.
    let rules = compile(
        &[
            "re:^a.c*$".into(),
            "re:[0-9]+".into(),
            "re:(foo|bar)!".into(),
        ],
        &[],
        &[],
    );
    assert!(rules.errors.is_empty());
    assert!(rules.verdict(&msg("abc", "u", 1)).blocked);
    assert!(rules.verdict(&msg("房间123", "u", 1)).blocked);
    assert!(rules.verdict(&msg("say foo!", "u", 1)).blocked);
    assert!(!rules.verdict(&msg("xyz", "u", 1)).blocked);

    // Malformed syntax and backtracking-only constructs are reported instead of
    // being silently interpreted as literal text.
    let broken = compile(
        &["re:[abc".into(), "re:".into(), "re:(?=a)".into()],
        &[],
        &[],
    );
    assert_eq!(broken.errors.len(), 3);
    assert_eq!(broken.errors[0].kind, RuleErrorKind::InvalidRegex);
    assert_eq!(broken.errors[2].kind, RuleErrorKind::UnsupportedRegex);
    assert!(broken.blocked_keywords.is_empty());

    // `\d` is a digit class now; the old subset matched a literal "d".
    let digits = compile(&["re:^\\d+$".into()], &[], &[]);
    assert!(digits.verdict(&msg("123", "u", 1)).blocked);
    assert!(!digits.verdict(&msg("ddd", "u", 1)).blocked);
}

#[test]
fn rule_parser_and_matcher_are_directly_usable() {
    let matcher = parse_rule("re:666+", false).unwrap();
    assert!(matches!(matcher, Matcher::Regex(_)));
    let regex = SimpleRegex::compile("^[a-c]+[0-9]?$").unwrap();
    assert!(regex.is_match("abc7"));
    assert!(!regex.is_match("xyz7"));

    let error = parse_rule("uid:abc", true).unwrap_err();
    assert_eq!(error.kind, RuleErrorKind::InvalidUid);
    assert!(parse_rule("", true).is_err());
}

#[test]
fn pathological_patterns_are_bounded() {
    let regex = SimpleRegex::compile("a*a*a*a*a*a*b").unwrap();
    assert!(!regex.is_match(&"a".repeat(4096)));
}

#[test]
fn rule_lists_are_capped() {
    let many: Vec<String> = (0..250).map(|index| format!("blocked-{index}")).collect();
    let rules = compile(&many, &[], &[]);
    assert_eq!(rules.blocked_keywords.len(), 200);
}
