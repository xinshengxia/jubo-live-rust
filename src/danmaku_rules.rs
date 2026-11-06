//! Danmaku filtering / highlight / pin rules (legacy DanmakuRules parity).
//!
//! Legacy semantics (danmaku-features.cs DanmakuRules):
//! * every rule is a line from the settings lists; a line starting with re: is a
//!   regular expression (case-insensitive, bounded), otherwise the rule is a
//!   case-insensitive substring match on the message text;
//! * user rules support uid:<digits> (exact user id) and name:<nickname>
//!   (exact nickname); a line without a prefix is a substring match on the user
//!   name;
//! * invalid or unsupported regexes are reported and skipped (legacy disabled
//!   them temporarily and retried after a settings change);
//! * pinning wins over highlighting, and blocking wins over both.
//!
//! The module is pure: the overlay compiles the lists once after a settings
//! change and calls verdict() per message. Regex uses a bounded linear-time
//! engine: classes, Unicode, groups, alternatives and repetitions are supported;
//! backreferences/look-around are rejected rather than silently mis-matched.

use crate::danmaku::DanmakuMessage;

/// Legacy limit for one rule line.
pub const MAX_RULE_CHARS: usize = 80;
/// Maximum number of rules per list that will be compiled.
pub const MAX_RULES: usize = 200;
/// Maximum regex pattern length (bounded matcher, no catastrophic backtracking).
pub const MAX_REGEX_CHARS: usize = 256;
/// Maximum matched text length (chars) fed into the regex engine.
pub const MAX_REGEX_TEXT_CHARS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleErrorKind {
    Empty,
    TooLong,
    InvalidUid,
    UnsupportedRegex,
    InvalidRegex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    pub rule: String,
    pub kind: RuleErrorKind,
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.kind {
            RuleErrorKind::Empty => "规则为空",
            RuleErrorKind::TooLong => "规则过长",
            RuleErrorKind::InvalidUid => "uid: 后应为正整数（从 1 开始）",
            RuleErrorKind::UnsupportedRegex => "正则语法不受支持（不支持回溯引用或前后查找）",
            RuleErrorKind::InvalidRegex => "正则语法错误",
        };
        write!(formatter, "{reason}：{}", self.rule)
    }
}

impl std::error::Error for RuleError {}

/// Case-insensitive regular expressions with a linear-time engine and explicit
/// allocation/length budgets. Unsupported look-around/backreferences are
/// rejected rather than silently interpreted as literal characters.
#[derive(Debug, Clone)]
pub struct SimpleRegex {
    pattern: String,
    regex: regex::Regex,
}
impl PartialEq for SimpleRegex {
    fn eq(&self, other: &Self) -> bool { self.pattern == other.pattern }
}
impl Eq for SimpleRegex {}
impl SimpleRegex {
    pub fn compile(pattern: &str) -> Result<Self, RuleErrorKind> {
        if pattern.is_empty() { return Err(RuleErrorKind::Empty); }
        if pattern.chars().count() > MAX_REGEX_CHARS { return Err(RuleErrorKind::TooLong); }
        let regex = regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .size_limit(1 << 20)
            .dfa_size_limit(1 << 20)
            .build()
            .map_err(|error| regex_error_kind(&error))?;
        Ok(Self { pattern: pattern.to_owned(), regex })
    }
    pub fn is_match(&self, text: &str) -> bool {
        let end = text.char_indices().nth(MAX_REGEX_TEXT_CHARS).map(|(n, _)| n).unwrap_or(text.len());
        self.regex.is_match(&text[..end])
    }
}

/// Classifies a regex build failure. The bounded engine is linear-time, so the
/// only constructs it cannot express are the backtracking-only .NET features
/// (look-around and backreferences, which the engine reports explicitly); those
/// surface as "unsupported" instead of being mistaken for a typo. Anything else
/// is a genuine syntax error.
fn regex_error_kind(error: &regex::Error) -> RuleErrorKind {
    match error {
        regex::Error::Syntax(message)
            if message.contains("look-around") || message.contains("backreference") =>
        {
            RuleErrorKind::UnsupportedRegex
        }
        _ => RuleErrorKind::InvalidRegex,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Case-insensitive substring on the message text.
    TextContains(String),
    /// Case-insensitive substring on the user name.
    NameContains(String),
    /// Exact user name (name: prefix).
    NameExact(String),
    /// Exact user id (uid: prefix).
    UserId(u64),
    /// re: bounded regex on the message text.
    Regex(SimpleRegex),
    /// re: regex evaluated against the user name, not message content.
    UserRegex(SimpleRegex),
}

impl Matcher {
    fn matches(&self, message: &DanmakuMessage) -> bool {
        match self {
            Self::TextContains(needle) => contains_ignore_case(&message.text, needle),
            Self::NameContains(needle) => contains_ignore_case(&message.user, needle),
            Self::NameExact(name) => message.user.to_lowercase() == name.to_lowercase(),
            Self::UserId(id) => message.user_id == *id,
            Self::Regex(regex) => regex.is_match(&message.text),
            Self::UserRegex(regex) => regex.is_match(&message.user),
        }
    }
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Parses one rule line. user_prefixes accepts uid:/name: prefixes (block user
/// lists); keyword lists only accept plain text / re: prefixes like the legacy
/// Any() helper.
pub fn parse_rule(rule: &str, user_prefixes: bool) -> Result<Matcher, RuleError> {
    let trimmed = rule.trim();
    if trimmed.is_empty() {
        return Err(RuleError {
            rule: rule.to_owned(),
            kind: RuleErrorKind::Empty,
        });
    }
    if trimmed.chars().count() > MAX_RULE_CHARS {
        return Err(RuleError {
            rule: trimmed.to_owned(),
            kind: RuleErrorKind::TooLong,
        });
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("re:") {
        // Keep the original text: the matcher lowercases pattern and text for
        // case-insensitive matching, so only the prefix is case-insensitive.
        let pattern = &trimmed[3..];
        let regex = SimpleRegex::compile(pattern).map_err(|kind| RuleError {
            rule: trimmed.to_owned(),
            kind,
        })?;
        return Ok(if user_prefixes { Matcher::UserRegex(regex) } else { Matcher::Regex(regex) });
    }
    if user_prefixes {
        if let Some(rest) = lower.strip_prefix("uid:") {
            // The legacy parser used NumberStyles.None: ASCII digits only, no
            // sign and no surrounding text. uid 0 is not a real user, so it must
            // not block anonymous messages either.
            let digits = rest.trim();
            let valid = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
            let uid = digits.parse::<u64>().map_err(|_| RuleError {
                rule: trimmed.to_owned(),
                kind: RuleErrorKind::InvalidUid,
            })?;
            if !valid || uid == 0 {
                return Err(RuleError {
                    rule: trimmed.to_owned(),
                    kind: RuleErrorKind::InvalidUid,
                });
            }
            return Ok(Matcher::UserId(uid));
        }
        if let Some(rest) = trimmed.get(5..).filter(|_| lower.starts_with("name:")) {
            let name = rest.trim();
            if name.is_empty() {
                return Err(RuleError {
                    rule: trimmed.to_owned(),
                    kind: RuleErrorKind::Empty,
                });
            }
            return Ok(Matcher::NameExact(name.to_owned()));
        }
    }
    Ok(if user_prefixes { Matcher::NameContains(trimmed.to_owned()) } else { Matcher::TextContains(trimmed.to_owned()) })
}

#[derive(Debug, Clone, Default)]
pub struct CompiledRules {
    pub blocked_keywords: Vec<Matcher>,
    pub blocked_users: Vec<Matcher>,
    pub highlighted: Vec<Matcher>,
    pub pinned: Vec<Matcher>,
    pub errors: Vec<RuleError>,
}

/// Compiles the legacy settings lists. Invalid rules are skipped and reported;
/// the rest keep working (legacy behaviour). Lists are capped at MAX_RULES.
pub fn compile(blocked: &[String], highlight: &[String], pin: &[String]) -> CompiledRules {
    let mut compiled = CompiledRules::default();
    for rule in blocked.iter().take(MAX_RULES) {
        let explicit_user = rule.trim().to_ascii_lowercase().starts_with("uid:")
            || rule.trim().to_ascii_lowercase().starts_with("name:");
        match parse_rule(rule, explicit_user) {
            Ok(matcher) if explicit_user => compiled.blocked_users.push(matcher),
            Ok(matcher) => compiled.blocked_keywords.push(matcher),
            Err(error) => compiled.errors.push(error),
        }
    }
    for rule in highlight.iter().take(MAX_RULES) {
        match parse_rule(rule, false) {
            Ok(matcher) => compiled.highlighted.push(matcher),
            Err(error) => compiled.errors.push(error),
        }
    }
    for rule in pin.iter().take(MAX_RULES) {
        match parse_rule(rule, false) {
            Ok(matcher) => compiled.pinned.push(matcher),
            Err(error) => compiled.errors.push(error),
        }
    }
    compiled
}

/// Compile separately scoped lists; ordinary user rules match names, not text.
pub fn compile_with_users(blocked: &[String], users: &[String], highlight: &[String], pin: &[String]) -> CompiledRules {
    let mut compiled = compile(blocked, highlight, pin);
    for rule in users.iter().take(MAX_RULES) {
        match parse_rule(rule, true) {
            Ok(matcher) => compiled.blocked_users.push(matcher),
            Err(error) => compiled.errors.push(error),
        }
    }
    compiled
}

/// Per-message rule verdict. Blocking wins over pinning, pinning over highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuleVerdict {
    pub blocked: bool,
    pub pinned: bool,
    pub highlighted: bool,
}

impl CompiledRules {
    pub fn is_blocked(&self, message: &DanmakuMessage) -> bool {
        self.blocked_users
            .iter()
            .any(|matcher| matcher.matches(message))
            || self
                .blocked_keywords
                .iter()
                .any(|matcher| matcher.matches(message))
    }

    pub fn is_highlighted(&self, message: &DanmakuMessage) -> bool {
        self.highlighted
            .iter()
            .any(|matcher| matcher.matches(message))
    }

    pub fn is_pinned(&self, message: &DanmakuMessage) -> bool {
        self.pinned.iter().any(|matcher| matcher.matches(message))
    }

    pub fn verdict(&self, message: &DanmakuMessage) -> RuleVerdict {
        let blocked = self.is_blocked(message);
        if blocked {
            return RuleVerdict {
                blocked,
                pinned: false,
                highlighted: false,
            };
        }
        RuleVerdict {
            blocked,
            pinned: self.is_pinned(message),
            highlighted: self.is_highlighted(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::danmaku::{DanmakuMessage, DanmakuMode};

    fn message(text: &str, user: &str, user_id: u64) -> DanmakuMessage {
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
    fn plain_rules_are_case_insensitive_substrings() {
        let rules = compile(&["广告".into(), "SPAM".into()], &[], &[]);
        assert!(rules.is_blocked(&message("这是广告", "u", 1)));
        assert!(rules.is_blocked(&message("buy spam now", "u", 1)));
        assert!(!rules.is_blocked(&message("正常弹幕", "u", 1)));
        assert!(rules.errors.is_empty());
    }

    #[test]
    fn uid_and_name_user_rules_are_exact() {
        let rules = compile(&["uid:12345".into(), "name:老观众".into()], &[], &[]);
        assert!(rules.is_blocked(&message("hi", "任何人", 12345)));
        assert!(rules.is_blocked(&message("hi", "老观众", 1)));
        assert!(!rules.is_blocked(&message("hi", "老观众2", 12345 + 1)));
        assert!(!rules.is_blocked(&message("hi", "别的", 999)));
    }

    #[test]
    fn regex_rules_support_bounded_subset() {
        let regex = SimpleRegex::compile("^a.c*$").unwrap();
        assert!(regex.is_match("abc"));
        assert!(regex.is_match("abcc"));
        assert!(!regex.is_match("xabc"));
        let class = SimpleRegex::compile("[a-c]+[0-9]?").unwrap();
        assert!(class.is_match("abc7"));
        assert!(!class.is_match("xyz"));
    }

    #[test]
    fn unsupported_regex_constructs_are_reported_and_skipped() {
        let rules = compile(&["re:(?=a)".into(), "re:[abc".into()], &[], &[]);
        assert_eq!(rules.errors.len(), 2);
        assert!(rules.blocked_keywords.is_empty());
    }

    #[test]
    fn highlight_and_pin_lists_work_and_blocking_wins() {
        let rules = compile(&["屏蔽".into()], &["666".into()], &["置顶".into()]);
        let verdict = rules.verdict(&message("666 置顶 屏蔽", "u", 1));
        assert!(verdict.blocked);
        assert!(!verdict.pinned && !verdict.highlighted);

        let verdict = rules.verdict(&message("666 置顶", "u", 1));
        assert!(!verdict.blocked);
        assert!(verdict.pinned);
        assert!(verdict.highlighted);
    }

    #[test]
    fn pathological_regex_is_bounded() {
        let regex = SimpleRegex::compile("a*a*a*a*a*a*b").unwrap();
        let text = "a".repeat(MAX_REGEX_TEXT_CHARS);
        assert!(!regex.is_match(&text));
    }
    #[test]
    fn user_lists_and_common_regex_syntax_keep_their_scope() {
        let rules = compile_with_users(&[], &["uid:42".into(), "name:Alice".into(), "re:^Bob\\d+$".into(), "观众".into()], &[], &[]);
        assert!(rules.is_blocked(&message("hello", "x", 42)));
        assert!(rules.is_blocked(&message("hello", "alice", 1)));
        assert!(rules.is_blocked(&message("hello", "Bob123", 1)));
        assert!(rules.is_blocked(&message("hello", "老观众", 1)));
        assert!(!rules.is_blocked(&message("Bob123 老观众", "normal", 1)));
        let digits = SimpleRegex::compile(r"^\d+$").unwrap();
        assert!(digits.is_match("123"));
        assert!(!digits.is_match("ddd"));
        assert!(SimpleRegex::compile(r"^(foo|bar){1,2}\s+\w+$").unwrap().is_match("FOO x"));
    }

    #[test]
    fn regex_build_failures_are_classified_for_the_settings_page() {
        // Backtracking-only .NET constructs are reported as unsupported, typos
        // as invalid syntax: the settings page must not save either silently.
        assert_eq!(
            SimpleRegex::compile("(?=a)").unwrap_err(),
            RuleErrorKind::UnsupportedRegex
        );
        assert_eq!(
            SimpleRegex::compile(r"(a)\1").unwrap_err(),
            RuleErrorKind::UnsupportedRegex
        );
        assert_eq!(
            SimpleRegex::compile("[abc").unwrap_err(),
            RuleErrorKind::InvalidRegex
        );
    }

    #[test]
    fn user_regex_matches_the_user_name_not_the_message_text() {
        let rules = compile_with_users(&[], &["re:^Bob\\d+$".into()], &[], &[]);
        // The message text matches the pattern but the user does not.
        assert!(!rules.is_blocked(&message("Bob123", "alice", 1)));
        assert!(rules.is_blocked(&message("hello", "bob7", 1)));
    }

    #[test]
    fn uid_zero_and_malformed_uid_are_rejected() {
        let rules = compile_with_users(&[], &["uid:0".into(), "uid:abc".into()], &[], &[]);
        assert_eq!(rules.errors.len(), 2);
        assert!(rules.blocked_users.is_empty());
        assert_eq!(rules.errors[0].kind, RuleErrorKind::InvalidUid);
    }

    #[test]
    fn re_prefix_is_case_insensitive_like_the_legacy_client() {
        let rules = compile(&["RE:^\\d+$".into()], &[], &[]);
        assert!(rules.errors.is_empty());
        assert!(rules.is_blocked(&message("123", "u", 1)));
        assert!(!rules.is_blocked(&message("x123", "u", 1)));
    }

}
