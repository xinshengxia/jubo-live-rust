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
//! change and calls verdict() per message. Regex support is a deliberate
//! bounded subset of the .NET syntax (literals, ., *, +, ?, ^, $, character
//! classes and escapes); groups/alternation/backreferences are treated as
//! literal text and documented as unsupported.

use crate::danmaku::DanmakuMessage;

/// Legacy limit for one rule line.
pub const MAX_RULE_CHARS: usize = 80;
/// Maximum number of rules per list that will be compiled.
pub const MAX_RULES: usize = 200;
/// Maximum regex pattern length (bounded matcher, no catastrophic backtracking).
pub const MAX_REGEX_CHARS: usize = 256;
/// Maximum matched text length (chars) fed into the regex engine.
pub const MAX_REGEX_TEXT_CHARS: usize = 4096;
/// Hard backtracking step budget per is_match() call.
pub const MAX_REGEX_STEPS: u32 = 200_000;

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
            RuleErrorKind::InvalidUid => "uid: 后应为数字",
            RuleErrorKind::UnsupportedRegex => "正则语法不受支持（仅支持 . * + ? ^ $ [] 与转义）",
            RuleErrorKind::InvalidRegex => "正则语法错误",
        };
        write!(formatter, "{reason}：{}", self.rule)
    }
}

impl std::error::Error for RuleError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quantifier {
    One,
    Optional,
    Any,
    OneOrMore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AtomKind {
    Literal(char),
    AnyChar,
    Class { negated: bool, ranges: Vec<(char, char)> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Atom {
    kind: AtomKind,
    quantifier: Quantifier,
}

impl Atom {
    fn matches(&self, ch: char) -> bool {
        match &self.kind {
            AtomKind::Literal(literal) => *literal == ch,
            AtomKind::AnyChar => true,
            AtomKind::Class { negated, ranges } => {
                let inside = ranges.iter().any(|(start, end)| ch >= *start && ch <= *end);
                inside != *negated
            }
        }
    }
}

/// Bounded regular expression used for re: rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleRegex {
    atoms: Vec<Atom>,
    anchored_start: bool,
    anchored_end: bool,
}

impl SimpleRegex {
    /// Compiles the supported subset. Unsupported constructs are rejected so a
    /// settings page can show a precise error instead of silently mis-matching.
    pub fn compile(pattern: &str) -> Result<Self, RuleErrorKind> {
        if pattern.is_empty() {
            return Err(RuleErrorKind::Empty);
        }
        if pattern.chars().count() > MAX_REGEX_CHARS {
            return Err(RuleErrorKind::TooLong);
        }
        let chars: Vec<char> = pattern.to_lowercase().chars().collect();
        let mut atoms = Vec::new();
        let mut index = 0;
        let mut anchored_start = false;
        let mut anchored_end = false;
        while index < chars.len() {
            let ch = chars[index];
            if ch == '^' && index == 0 {
                anchored_start = true;
                index += 1;
                continue;
            }
            if ch == '$' && index + 1 == chars.len() {
                anchored_end = true;
                index += 1;
                continue;
            }
            let kind = match ch {
                '.' => {
                    index += 1;
                    AtomKind::AnyChar
                }
                '\\' => {
                    let Some(next) = chars.get(index + 1).copied() else {
                        return Err(RuleErrorKind::InvalidRegex);
                    };
                    if matches!(next, '(' | ')' | '|' | '{' | '}' | '[' | ']') {
                        return Err(RuleErrorKind::UnsupportedRegex);
                    }
                    index += 2;
                    AtomKind::Literal(next)
                }
                '[' => {
                    index += 1;
                    let mut negated = false;
                    if chars.get(index) == Some(&'^') {
                        negated = true;
                        index += 1;
                    }
                    let mut ranges = Vec::new();
                    let mut closed = false;
                    while index < chars.len() {
                        let current = chars[index];
                        if current == ']' {
                            closed = true;
                            index += 1;
                            break;
                        }
                        let start = if current == '\\' {
                            let Some(next) = chars.get(index + 1).copied() else {
                                return Err(RuleErrorKind::InvalidRegex);
                            };
                            index += 2;
                            next
                        } else {
                            index += 1;
                            current
                        };
                        if chars.get(index) == Some(&'-')
                            && chars.get(index + 1).is_some()
                            && chars.get(index + 1) != Some(&']')
                        {
                            let Some(end) = chars.get(index + 1).copied() else {
                                return Err(RuleErrorKind::InvalidRegex);
                            };
                            if end < start {
                                return Err(RuleErrorKind::InvalidRegex);
                            }
                            ranges.push((start, end));
                            index += 2;
                        } else {
                            ranges.push((start, start));
                        }
                    }
                    if !closed || ranges.is_empty() {
                        return Err(RuleErrorKind::InvalidRegex);
                    }
                    AtomKind::Class { negated, ranges }
                }
                '(' | ')' | '|' | '{' | '}' | '*' | '+' | '?' => {
                    return Err(RuleErrorKind::UnsupportedRegex);
                }
                other => {
                    index += 1;
                    AtomKind::Literal(other)
                }
            };
            let quantifier = match chars.get(index) {
                Some('?') => {
                    index += 1;
                    Quantifier::Optional
                }
                Some('*') => {
                    index += 1;
                    Quantifier::Any
                }
                Some('+') => {
                    index += 1;
                    Quantifier::OneOrMore
                }
                _ => Quantifier::One,
            };
            atoms.push(Atom { kind, quantifier });
        }
        if atoms.is_empty() {
            return Err(RuleErrorKind::InvalidRegex);
        }
        Ok(Self {
            atoms,
            anchored_start,
            anchored_end,
        })
    }

    /// Case-insensitive match against the (length-capped) message text.
    pub fn is_match(&self, text: &str) -> bool {
        let lowered: String = text.chars().take(MAX_REGEX_TEXT_CHARS).flat_map(char::to_lowercase).collect();
        let chars: Vec<char> = lowered.chars().collect();
        let mut budget = MAX_REGEX_STEPS;
        if self.anchored_start {
            return self.match_from(&chars, 0, 0, &mut budget);
        }
        for start in 0..=chars.len() {
            if self.match_from(&chars, start, 0, &mut budget) {
                return true;
            }
            if budget == 0 {
                return false;
            }
        }
        false
    }

    fn match_from(&self, text: &[char], position: usize, atom_index: usize, budget: &mut u32) -> bool {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        if atom_index == self.atoms.len() {
            return !self.anchored_end || position == text.len();
        }
        let atom = &self.atoms[atom_index];
        match atom.quantifier {
            Quantifier::One => {
                if position < text.len() && atom.matches(text[position]) {
                    self.match_from(text, position + 1, atom_index + 1, budget)
                } else {
                    false
                }
            }
            Quantifier::Optional => {
                self.match_from(text, position, atom_index + 1, budget)
                    || (position < text.len()
                        && atom.matches(text[position])
                        && self.match_from(text, position + 1, atom_index + 1, budget))
            }
            Quantifier::Any => {
                let mut cursor = position;
                loop {
                    if self.match_from(text, cursor, atom_index + 1, budget) {
                        return true;
                    }
                    if *budget == 0 || cursor >= text.len() || !atom.matches(text[cursor]) {
                        return false;
                    }
                    cursor += 1;
                }
            }
            Quantifier::OneOrMore => {
                if position >= text.len() || !atom.matches(text[position]) {
                    return false;
                }
                let mut cursor = position + 1;
                loop {
                    if self.match_from(text, cursor, atom_index + 1, budget) {
                        return true;
                    }
                    if *budget == 0 || cursor >= text.len() || !atom.matches(text[cursor]) {
                        return false;
                    }
                    cursor += 1;
                }
            }
        }
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
}

impl Matcher {
    fn matches(&self, message: &DanmakuMessage) -> bool {
        match self {
            Self::TextContains(needle) => contains_ignore_case(&message.text, needle),
            Self::NameContains(needle) => contains_ignore_case(&message.user, needle),
            Self::NameExact(name) => message.user == *name,
            Self::UserId(id) => message.user_id == *id,
            Self::Regex(regex) => regex.is_match(&message.text),
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
        return Ok(Matcher::Regex(regex));
    }
    if user_prefixes {
        if let Some(rest) = lower.strip_prefix("uid:") {
            let uid = rest.trim().parse::<u64>().map_err(|_| RuleError {
                rule: trimmed.to_owned(),
                kind: RuleErrorKind::InvalidUid,
            })?;
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
    Ok(Matcher::TextContains(trimmed.to_owned()))
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
        match parse_rule(rule, true) {
            Ok(matcher) => match matcher {
                Matcher::UserId(_) | Matcher::NameExact(_) | Matcher::NameContains(_) => {
                    compiled.blocked_users.push(matcher)
                }
                other => compiled.blocked_keywords.push(other),
            },
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

/// Per-message rule verdict. Blocking wins over pinning, pinning over highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuleVerdict {
    pub blocked: bool,
    pub pinned: bool,
    pub highlighted: bool,
}

impl CompiledRules {
    pub fn is_blocked(&self, message: &DanmakuMessage) -> bool {
        self.blocked_users.iter().any(|matcher| matcher.matches(message))
            || self
                .blocked_keywords
                .iter()
                .any(|matcher| matcher.matches(message))
    }

    pub fn is_highlighted(&self, message: &DanmakuMessage) -> bool {
        self.highlighted.iter().any(|matcher| matcher.matches(message))
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
    use crate::danmaku::{DanmakuMode, DanmakuMessage};

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
        let rules = compile(&["re:(a|b)".into(), "re:[abc".into()], &[], &[]);
        assert_eq!(rules.errors.len(), 2);
        assert!(rules.blocked_keywords.is_empty());
    }

    #[test]
    fn highlight_and_pin_lists_work_and_blocking_wins() {
        let rules = compile(
            &["屏蔽".into()],
            &["666".into()],
            &["置顶".into()],
        );
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
}
