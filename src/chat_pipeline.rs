//! Admission policy independent of drawing, networking and real user data.
use crate::{
    core::DanmakuBehavior,
    danmaku::{DanmakuMessage, DanmakuMode},
};
use std::collections::{BTreeMap, BTreeSet};

/// Normalises text for duplicate detection only: runs of whitespace collapse,
/// full-width ASCII becomes half-width and the result is lower-cased. The first
/// message keeps its original text; only the merge key is normalised.
pub fn normalize_for_merge(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        let character = if ('！'..='～').contains(&character) {
            char::from_u32(character as u32 - 0xFEE0).unwrap_or(character)
        } else if character == '　' {
            ' '
        } else {
            character
        };
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(character);
    }
    output.to_ascii_lowercase()
}
#[derive(Clone)]
pub struct PendingMessage {
    pub message: DanmakuMessage,
    pub release_at: f64,
}
struct Group {
    id: String,
    last: f64,
}

/// Token-bucket admission gate for the legacy smooth-admission options.
/// Messages that arrive faster than the configured rate may wait for a short,
/// bounded slot; beyond that they are dropped instead of growing a backlog.
#[derive(Debug, Clone)]
pub struct AdmissionGate {
    rate: f32,
    capacity: f32,
    tokens: f32,
    last: Option<f64>,
    queue_wait: f64,
}

impl AdmissionGate {
    pub fn new(per_second: u8, queue_wait_ms: u16, queue_capacity: u8) -> Self {
        Self::with_default_rate(per_second, 30.0, queue_wait_ms, queue_capacity)
    }

    /// Builds the gate with the legacy derived rate when the user did not pin
    /// one: rate = max(1, MaxOnScreen / max(2, crossing duration)) messages per
    /// second (danmaku-admission.cs Admitted()).
    pub fn with_default_rate(
        per_second: u8,
        default_rate: f32,
        queue_wait_ms: u16,
        queue_capacity: u8,
    ) -> Self {
        let fallback = if default_rate.is_finite() {
            default_rate.clamp(1.0, 120.0)
        } else {
            30.0
        };
        let rate = if per_second == 0 {
            fallback
        } else {
            f32::from(per_second)
        };
        Self {
            rate,
            capacity: f32::from(queue_capacity.max(1)),
            // One immediate token: bursts wait in the bounded short queue.
            tokens: 1.0,
            last: None,
            queue_wait: f64::from(queue_wait_ms) / 1000.0,
        }
    }

    pub fn rate_per_second(&self) -> f32 {
        self.rate
    }

    /// Returns None when the message should be dropped, otherwise the extra
    /// release delay in seconds (0 when a token was immediately available).
    pub fn admit(&mut self, now: f64) -> Option<f64> {
        if !now.is_finite() {
            return None;
        }
        if let Some(last) = self.last {
            let elapsed = (now - last).max(0.0) as f32;
            self.tokens = (self.tokens + elapsed * self.rate).min(1.0);
        }
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Some(0.0);
        }
        let delay = f64::from(1.0 - self.tokens) / f64::from(self.rate);
        // The wait budget and the short-queue length both bound the backlog.
        if delay > self.queue_wait || -self.tokens >= self.capacity {
            return None;
        }
        self.tokens -= 1.0;
        Some(delay)
    }
}

pub struct ChatPipeline {
    settings: DanmakuBehavior,
    groups: BTreeMap<String, Group>,
    counts: BTreeMap<String, u32>,
    sequence: u64,
    rules: crate::danmaku_rules::CompiledRules,
    gate: Option<AdmissionGate>,
}
impl ChatPipeline {
    pub fn new(settings: DanmakuBehavior) -> Self {
        Self::build(settings, 30.0)
    }

    /// Builds the admission gate with the legacy derived rate (see
    /// DanmakuBehavior::admission_per_second): MaxOnScreen / max(2, duration).
    pub fn new_for_display(
        settings: DanmakuBehavior,
        max_visible: u16,
        duration_secs: f64,
    ) -> Self {
        let duration = if duration_secs.is_finite() {
            duration_secs.max(2.0)
        } else {
            8.0
        };
        let derived = (f32::from(max_visible.max(1)) / duration as f32).clamp(1.0, 120.0);
        Self::build(settings, derived)
    }

    fn build(settings: DanmakuBehavior, derived_rate: f32) -> Self {
        let rules = crate::danmaku_rules::compile_with_users(
            &settings.blocked_keywords, &settings.blocked_users, &[], &[]);
        let gate = if settings.smooth_admission {
            Some(AdmissionGate::with_default_rate(
                settings.admission_per_second,
                derived_rate,
                settings.queue_wait_ms,
                settings.queue_capacity,
            ))
        } else {
            None
        };
        Self {
            settings,
            groups: BTreeMap::new(),
            counts: BTreeMap::new(),
            sequence: 0,
            rules,
            gate,
        }
    }
    /// Applies blocking, visibility, duplicate merging and the admission gate.
    ///
    /// With duplicate merging on, the returned message id is rewritten to
    /// `local-merge-N` (the merge group id). Callers that keep per-message state
    /// keyed by id — e.g. the overlay's highlight colour set — must key it off
    /// the *returned* id, never the wire id, or merged messages lose that state.
    pub fn admit(&mut self, mut message: DanmakuMessage, now: f64) -> Option<PendingMessage> {
        if self.rules.is_blocked(&message) { return None; }
        if !self.mode_visible(message.mode) {
            return None;
        }
        if !self.settings.show_colored
            && message.color != 0
            && (message.color & 0xffffff) != 0xffffff
        {
            return None;
        }
        if message.user_level > 0 && message.user_level < u16::from(self.settings.min_user_level) {
            return None;
        }
        let medal_level = message.medal.as_ref().map(|medal| medal.level).unwrap_or(0);
        if medal_level < u16::from(self.settings.min_medal_level) {
            return None;
        }
        if self.settings.merge_duplicates {
            let key = format!(
                "{:?}:{}:{}",
                message.mode,
                message.color,
                normalize_for_merge(&message.text)
            );
            if let Some(group) = self.groups.get_mut(&key) {
                if now - group.last <= f64::from(self.settings.merge_window_ms) / 1000.0 {
                    if let Some(count) = self.counts.get_mut(&group.id) {
                        *count = count.saturating_add(1).min(9999);
                        group.last = now;
                        return None;
                    }
                }
            }
            self.sequence = self.sequence.wrapping_add(1);
            message.id = format!("local-merge-{}", self.sequence);
            self.counts.insert(message.id.clone(), 1);
            self.groups.insert(
                key,
                Group {
                    id: message.id.clone(),
                    last: now,
                },
            );
            if self.groups.len() > 1024 {
                if let Some(key) = self
                    .groups
                    .iter()
                    .min_by(|a, b| a.1.last.total_cmp(&b.1.last))
                    .map(|(k, _)| k.clone())
                {
                    self.groups.remove(&key);
                }
            }
        }
        let mut queued = 0.0;
        if let Some(gate) = self.gate.as_mut() {
            match gate.admit(now) {
                Some(delay) => queued = delay,
                None => return None,
            }
        }
        Some(PendingMessage {
            message,
            release_at: now + f64::from(self.settings.delay_ms) / 1000.0 + queued,
        })
    }
    /// Whether the configured display mode includes this message position.
    /// Updates the arrival delay without touching merge/gate state (used by the
    /// legacy delay hotkeys while the overlay is running).
    pub fn set_delay_ms(&mut self, delay_ms: u16) {
        self.settings.delay_ms = delay_ms.min(15_000);
    }

    pub fn mode_visible(&self, mode: DanmakuMode) -> bool {
        match mode {
            DanmakuMode::Scroll => self.settings.show_scrolling,
            DanmakuMode::Top => self.settings.show_top,
            DanmakuMode::Bottom => self.settings.show_bottom,
            DanmakuMode::Reverse => self.settings.show_reverse,
        }
    }
    pub fn allows(&self, user: &str, text: &str) -> bool {
        !self.rules.is_blocked(&DanmakuMessage {
            image_url: None, id: String::new(), user: user.into(), text: text.into(),
            mode: DanmakuMode::Scroll, color: 0xffffff, font_size: 25,
            user_id: 0, user_level: 0, guard_level: 0, medal: None,
            image_width: 0, image_height: 0, image_large: false,
        })
    }
    pub fn count(&self, id: &str) -> u32 {
        self.counts.get(id).copied().unwrap_or(1)
    }
    pub fn retain<'a>(&mut self, ids: impl Iterator<Item = &'a str>) {
        let keep: BTreeSet<_> = ids.map(str::to_owned).collect();
        self.counts.retain(|id, _| keep.contains(id));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::danmaku::DanmakuMode;
    fn message() -> DanmakuMessage {
        DanmakuMessage {
            image_url: None,
            id: "server-id".into(),
            user: "synthetic".into(),
            text: "hello world".into(),
            mode: DanmakuMode::Scroll,
            color: 0xffffff,
            font_size: 27,
            user_id: 0,
            user_level: 0,
            guard_level: 0,
            medal: None,
            image_width: 0,
            image_height: 0,
            image_large: false,
        }
    }
    #[test]
    fn blocked_users_and_words_do_not_enter_delay_queue() {
        let settings = DanmakuBehavior {
            blocked_keywords: vec!["WORLD".into()],
            ..Default::default()
        };
        assert!(ChatPipeline::new(settings).admit(message(), 0.0).is_none());
        let settings = DanmakuBehavior {
            blocked_users: vec!["SYNTHETIC".into()],
            ..Default::default()
        };
        assert!(ChatPipeline::new(settings).admit(message(), 0.0).is_none());
    }
    #[test]
    fn duplicates_merge_only_while_the_group_is_still_queued_or_visible() {
        let mut pipe = ChatPipeline::new(DanmakuBehavior {
            delay_ms: 500,
            ..Default::default()
        });
        let first = pipe.admit(message(), 1.0).unwrap();
        assert_eq!(first.release_at, 1.5);
        assert!(pipe.admit(message(), 1.1).is_none());
        assert_eq!(pipe.count(&first.message.id), 2);
        pipe.retain(std::iter::empty());
        let next = pipe.admit(message(), 1.2).unwrap();
        assert_ne!(first.message.id, next.message.id);
    }
    #[test]
    fn duplicate_merge_normalises_only_the_key_not_the_displayed_text() {
        assert_eq!(normalize_for_merge("  Hello   WORLD "), "hello world");
        assert_eq!(normalize_for_merge("ＡＢＣ　１２３"), "abc 123");
        assert_eq!(normalize_for_merge(""), "");
        let mut pipe = ChatPipeline::new(Default::default());
        let first = pipe
            .admit(
                DanmakuMessage {
                    text: "Hello  world".into(),
                    ..message()
                },
                0.0,
            )
            .unwrap();
        assert_eq!(first.message.text, "Hello  world");
        assert!(
            pipe.admit(
                DanmakuMessage {
                    text: "  hello WORLD ".into(),
                    ..message()
                },
                0.1
            )
            .is_none()
        );
        assert_eq!(pipe.count(&first.message.id), 2);
    }

    #[test]
    fn mode_colour_and_level_filters_are_applied_at_admission() {
        let settings = DanmakuBehavior {
            show_top: false,
            show_bottom: false,
            show_colored: false,
            min_user_level: 10,
            min_medal_level: 5,
            ..Default::default()
        };
        let mut pipe = ChatPipeline::new(settings.clone());
        assert!(
            pipe.admit(
                DanmakuMessage {
                    mode: DanmakuMode::Top,
                    ..message()
                },
                0.0
            )
            .is_none()
        );
        assert!(
            pipe.admit(
                DanmakuMessage {
                    mode: DanmakuMode::Bottom,
                    ..message()
                },
                0.0
            )
            .is_none()
        );
        let allowed = || DanmakuMessage {
            user_level: 30,
            medal: Some(crate::danmaku::Medal {
                name: "合成".into(),
                level: 5,
                color: 0,
            }),
            ..message()
        };
        assert!(pipe.admit(allowed(), 0.0).is_some());
        assert!(
            pipe.admit(
                DanmakuMessage {
                    color: 0x123456,
                    ..allowed()
                },
                0.0
            )
            .is_none()
        );
        assert!(
            pipe.admit(
                DanmakuMessage {
                    user_level: 3,
                    ..allowed()
                },
                0.0
            )
            .is_none()
        );
        // A message without a medal cannot satisfy a minimum medal level.
        assert!(
            pipe.admit(
                DanmakuMessage {
                    medal: None,
                    ..allowed()
                },
                0.0
            )
            .is_none()
        );
        assert!(
            pipe.admit(
                DanmakuMessage {
                    medal: Some(crate::danmaku::Medal {
                        name: "合成".into(),
                        level: 4,
                        color: 0
                    }),
                    ..allowed()
                },
                0.0
            )
            .is_none()
        );
        assert!(!pipe.mode_visible(DanmakuMode::Top));
        assert!(pipe.mode_visible(DanmakuMode::Scroll));
    }

    #[test]
    fn admission_gate_limits_the_rate_with_a_bounded_wait() {
        let mut gate = AdmissionGate::new(10, 150, 4);
        assert_eq!(gate.admit(0.0), Some(0.0));
        // Immediately after, the bucket is empty: the wait is charged instead of
        // dropping until the bounded wait budget is exhausted.
        let waited = gate.admit(0.0).expect("within wait budget");
        assert!((waited - 0.1).abs() < 1e-4, "{waited}");
        // Deep backlog beyond the wait budget is dropped.
        let mut gate = AdmissionGate::new(1, 1000, 2);
        assert_eq!(gate.admit(0.0), Some(0.0));
        assert_eq!(gate.admit(0.0), Some(1.0));
        assert!(gate.admit(0.0).is_none());
        assert!(gate.admit(f64::NAN).is_none());
        // After enough time the bucket refills and admits again.
        let mut gate = AdmissionGate::new(1, 0, 4);
        assert_eq!(gate.admit(0.0), Some(0.0));
        assert!(gate.admit(0.1).is_none());
        assert!(gate.admit(1.0).is_some());
        assert!((AdmissionGate::new(0, 0, 1).rate_per_second() - 30.0).abs() < 1e-4);
    }

    #[test]
    fn smooth_admission_queues_a_second_message_instead_of_dropping_it() {
        let mut pipe = ChatPipeline::new(DanmakuBehavior {
            admission_per_second: 5,
            queue_wait_ms: 500,
            queue_capacity: 8,
            ..Default::default()
        });
        let first = pipe.admit(message(), 0.0).expect("first");
        assert_eq!(first.release_at, 0.0);
        // A distinct message (different colour) is queued, not duplicate-merged.
        let second = pipe
            .admit(
                DanmakuMessage {
                    color: 0xff0000,
                    ..message()
                },
                0.0,
            )
            .expect("queued");
        assert!((second.release_at - 0.2).abs() < 1e-4);
        // Disabling smooth admission bypasses the gate entirely.
        let mut plain = ChatPipeline::new(DanmakuBehavior {
            smooth_admission: false,
            merge_duplicates: false,
            ..Default::default()
        });
        for _ in 0..50 {
            assert!(plain.admit(message(), 0.0).is_some());
        }
    }

    #[test]
    fn reverse_messages_follow_the_reverse_display_setting() {
        let mut pipe = ChatPipeline::new(DanmakuBehavior::default());
        assert!(pipe.mode_visible(DanmakuMode::Reverse));
        pipe = ChatPipeline::new(DanmakuBehavior {
            show_reverse: false,
            ..Default::default()
        });
        assert!(!pipe.mode_visible(DanmakuMode::Reverse));
        assert!(
            pipe.admit(
                DanmakuMessage {
                    mode: DanmakuMode::Reverse,
                    ..message()
                },
                0.0
            )
            .is_none()
        );
    }

    #[test]
    fn merged_counts_cannot_change_an_unrelated_message() {
        let mut pipe = ChatPipeline::new(Default::default());
        let first = pipe.admit(message(), 0.0).unwrap();
        let mut other = message();
        other.color = 0xff0000;
        let other = pipe.admit(other, 0.0).unwrap();
        pipe.admit(message(), 0.2);
        assert_eq!(pipe.count(&first.message.id), 2);
        assert_eq!(pipe.count(&other.message.id), 1);
    }

    #[test]
    fn user_rules_from_settings_support_uid_name_regex_and_contains() {
        let settings = DanmakuBehavior {
            blocked_users: vec![
                "uid:42".into(),
                "name:Alice".into(),
                "re:^Bob\\d+$".into(),
                "观众".into(),
            ],
            ..Default::default()
        };
        let mut pipe = ChatPipeline::new(settings);
        let blocked = |pipe: &mut ChatPipeline, user: &str, user_id: u64| {
            pipe.admit(
                DanmakuMessage {
                    user: user.into(),
                    user_id,
                    ..message()
                },
                0.0,
            )
            .is_none()
        };
        assert!(blocked(&mut pipe, "anyone", 42));
        assert!(blocked(&mut pipe, "alice", 1));
        assert!(blocked(&mut pipe, "Bob123", 1));
        assert!(blocked(&mut pipe, "老观众", 1));
        assert!(!blocked(&mut pipe, "normal", 7));
        // The live-event filter uses the same compiled rules.
        assert!(!pipe.allows("Alice", "普通消息"));
        assert!(pipe.allows("normal", "普通消息"));
    }

    #[test]
    fn an_invalid_user_rule_is_skipped_instead_of_blocking_everything() {
        let mut pipe = ChatPipeline::new(DanmakuBehavior {
            blocked_users: vec!["re:[abc".into()],
            ..Default::default()
        });
        // The overlay logs the compile error; the pipeline itself keeps working
        // and must not interpret the malformed rule as a match-everything rule.
        assert!(pipe.admit(message(), 0.0).is_some());
    }
}
