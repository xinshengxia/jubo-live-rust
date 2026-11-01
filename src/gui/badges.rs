//! Room-live badge scheduling for favorites and recent rooms.
//!
//! Policy is pure and testable offline: bounded parallelism, a two-minute
//! freshness window per room, and a shorter retry after an unknown/failed query
//! so a transient platform limit heals quickly. The launcher only displays the
//! result for the exact room key; nothing here claims a room is live from a
//! cached favorite list.

use super::*;
use crate::room_state::RoomState;
use serde::{Deserialize, Serialize};

/// Maximum queries in flight at once. Kept low because Douyin/Bilibili rate
/// limits react to client bursts; four is enough to fill a 14-room list quickly.
pub const BADGE_MAX_IN_FLIGHT: usize = 4;
/// A confirmed room state is reused for this long before an automatic re-query.
pub const BADGE_FRESH_SECS: u64 = 120;
/// An unknown/failed query is retried after this short window instead of 2 min.
pub const BADGE_UNKNOWN_RETRY_SECS: u64 = 20;
/// Defensive bound for cached badge state (favorites + history are far smaller).
pub const BADGE_MAX_TRACKED: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Badge {
    Live,
    Offline,
    Replay,
    Unknown,
}

impl Badge {
    pub fn from_state(state: RoomState) -> Self {
        match state {
            RoomState::Live => Self::Live,
            RoomState::Offline => Self::Offline,
            RoomState::Replay => Self::Replay,
            RoomState::Unknown => Self::Unknown,
        }
    }

    /// Prefix shown in lists; an unknown result never pretends the room is off.
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "[播] ",
            Self::Offline => "[休] ",
            Self::Replay => "[重播] ",
            Self::Unknown => "",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Live => "直播中",
            Self::Offline => "未开播",
            Self::Replay => "重播中",
            Self::Unknown => "状态待确认",
        }
    }

    /// Favorites-page ordering rank, identical to the legacy launcher:
    /// live first, then replay, then offline, then unknown. Ties keep the
    /// caller's stable name ordering.
    pub fn favorite_rank(self) -> u8 {
        match self {
            Self::Live => 0,
            Self::Replay => 1,
            Self::Offline => 2,
            Self::Unknown => 3,
        }
    }
}

/// Stable key for one room; platform is part of the identity.
pub fn room_key(target: &Target) -> String {
    format!("{}:{}", target.platform, target.room)
}

/// Records a query time. An unknown result is backdated so the room becomes
/// eligible again after BADGE_UNKNOWN_RETRY_SECS, not after the full freshness
/// window (mirrors the C# BadgeScheduler policy).
pub fn record_result(times: &mut BTreeMap<String, u64>, key: &str, now: u64, unknown: bool) {
    if key.is_empty() {
        return;
    }
    let stamped = if unknown {
        now.saturating_sub(BADGE_FRESH_SECS)
            .saturating_add(BADGE_UNKNOWN_RETRY_SECS)
    } else {
        now
    };
    times.insert(key.to_owned(), stamped);
}

/// Picks the rooms that may be queried now: not already in flight, not checked
/// within the freshness window, deduplicated, and bounded by the in-flight cap.
/// Input order is preserved (favorites before recent rooms).
pub fn pick_badges(
    ordered_keys: &[String],
    pending: &BTreeSet<String>,
    times: &BTreeMap<String, u64>,
    now: u64,
    max_in_flight: usize,
) -> Vec<String> {
    let mut batch = Vec::new();
    if max_in_flight == 0 {
        return batch;
    }
    let mut in_flight = pending.len();
    for key in ordered_keys {
        if in_flight >= max_in_flight {
            break;
        }
        if key.is_empty() || pending.contains(key) || batch.contains(key) {
            continue;
        }
        if let Some(at) = times.get(key) {
            // A clock that moved backwards keeps the entry fresh instead of
            // creating a query storm.
            if now <= *at || now - *at < BADGE_FRESH_SECS {
                continue;
            }
        }
        batch.push(key.clone());
        in_flight += 1;
    }
    batch
}

impl Launcher {
    fn badge_target(&self, key: &str) -> Option<Target> {
        self.snapshot
            .favorites
            .iter()
            .map(|entry| &entry.target)
            .chain(self.snapshot.history.iter().map(|entry| &entry.target))
            .find(|target| room_key(target) == key)
            .cloned()
    }

    /// Fills the in-flight slots from favorites and recent rooms. Called from the
    /// render loop; badge_networking is false in headless/test constructors so no
    /// platform request is ever made by a test.
    pub(super) fn schedule_room_badges(&mut self) {
        if !self.badge_networking {
            return;
        }
        let mut ordered = Vec::new();
        for entry in &self.snapshot.favorites {
            ordered.push(room_key(&entry.target));
        }
        for entry in &self.snapshot.history {
            ordered.push(room_key(&entry.target));
        }
        let now = health_now();
        let batch = pick_badges(
            &ordered,
            &self.badge_pending,
            &self.badge_times,
            now,
            BADGE_MAX_IN_FLIGHT,
        );
        for key in batch {
            if let Some(target) = self.badge_target(&key) {
                self.spawn_badge_check(key, target);
            }
        }
    }

    fn spawn_badge_check(&mut self, key: String, target: Target) {
        if self.badge_pending.len() >= BADGE_MAX_IN_FLIGHT * 2 {
            return;
        }
        self.badge_pending.insert(key.clone());
        let sender = self.event_sender.clone();
        let root = self.snapshot.root.clone();
        thread::spawn(move || {
            let vault = CookieVault::open(root.join("credentials")).ok();
            let result = crate::room_state::check(&target, vault.as_ref())
                .map(|observation| observation.state)
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Event::Badge { key, result });
        });
    }

    /// Explicit request from a user selection; bypasses the freshness window but
    /// still respects single-flight per room.
    pub(super) fn request_room_badge(&mut self, target: &Target) {
        let key = room_key(target);
        if self.badge_pending.contains(&key) {
            return;
        }
        self.spawn_badge_check(key, target.clone());
    }

    pub(super) fn apply_badge_result(
        &mut self,
        key: String,
        result: std::result::Result<RoomState, String>,
    ) {
        self.badge_pending.remove(&key);
        let now = health_now();
        let (badge, unknown) = match result {
            Ok(state) => (Badge::from_state(state), state == RoomState::Unknown),
            Err(_) => (Badge::Unknown, true),
        };
        record_result(&mut self.badge_times, &key, now, unknown);
        self.badges.insert(key, badge);
        if self.badges.len() > BADGE_MAX_TRACKED {
            self.badges.clear();
            self.badge_times.clear();
        }
    }

    /// Badge prefix for one room, if any result is known.
    pub(super) fn badge_label(&self, target: &Target) -> Option<&'static str> {
        self.badges
            .get(&room_key(target))
            .map(|badge| badge.label())
            .filter(|label| !label.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(platform: Platform, room: &str) -> String {
        room_key(&Target::new(platform, room).unwrap())
    }

    #[test]
    fn picking_respects_freshness_dedupe_and_the_in_flight_cap() {
        let keys = vec![
            key(Platform::Bili, "1"),
            key(Platform::Huya, "2"),
            key(Platform::Bili, "1"),
            key(Platform::Douyu, "3"),
            key(Platform::Douyin, "4"),
            key(Platform::Bili, "5"),
        ];
        let mut times = BTreeMap::new();
        let now = 10_000;
        // One room is still fresh, one is eligible again.
        times.insert(keys[0].clone(), now - 10);
        times.insert(keys[1].clone(), now - BADGE_FRESH_SECS);
        let pending = BTreeSet::from([keys[3].clone()]);
        let batch = pick_badges(&keys, &pending, &times, now, 4);
        // Bili:1 fresh and Douyu:3 pending are skipped; duplicates deduplicated;
        // total in flight (pending + batch) stays at the cap.
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0], keys[1]);
        assert_eq!(batch[1], keys[4]);
        assert_eq!(batch[2], keys[5]);
        assert!(!batch.contains(&keys[0]));
        assert!(!batch.contains(&keys[3]));
        assert!(pick_badges(&keys, &pending, &times, now, 0).is_empty());
    }

    #[test]
    fn unknown_results_retry_far_sooner_than_confirmed_states() {
        let mut times = BTreeMap::new();
        let room = key(Platform::Bili, "1");
        record_result(&mut times, &room, 10_000, true);
        assert_eq!(
            times[&room],
            10_000 - BADGE_FRESH_SECS + BADGE_UNKNOWN_RETRY_SECS
        );
        // Allowed again after the short retry window.
        assert!(
            pick_badges(
                std::slice::from_ref(&room),
                &BTreeSet::new(),
                &times,
                10_000 + BADGE_UNKNOWN_RETRY_SECS + 1,
                4
            )
            .len()
                == 1
        );
        record_result(&mut times, &room, 20_000, false);
        assert!(
            pick_badges(
                std::slice::from_ref(&room),
                &BTreeSet::new(),
                &times,
                20_000 + BADGE_FRESH_SECS - 1,
                4
            )
            .is_empty()
        );
    }

    #[test]
    fn badges_distinguish_live_offline_and_replay_without_guessing() {
        assert_eq!(Badge::from_state(RoomState::Live).label(), "[播] ");
        assert_eq!(Badge::from_state(RoomState::Offline).label(), "[休] ");
        assert_eq!(Badge::from_state(RoomState::Replay).label(), "[重播] ");
        assert_eq!(Badge::from_state(RoomState::Live).description(), "直播中");
        assert_eq!(Badge::from_state(RoomState::Replay).description(), "重播中");
        assert_eq!(Badge::from_state(RoomState::Unknown).label(), "");
        assert_eq!(
            Badge::from_state(RoomState::Unknown).description(),
            "状态待确认"
        );
    }
}
    #[test]
    fn favorite_rank_puts_live_first_then_replay_offline_unknown() {
        assert_eq!(Badge::Live.favorite_rank(), 0);
        assert_eq!(Badge::Replay.favorite_rank(), 1);
        assert_eq!(Badge::Offline.favorite_rank(), 2);
        assert_eq!(Badge::Unknown.favorite_rank(), 3);
        let mut badges = vec![Badge::Offline, Badge::Live, Badge::Unknown, Badge::Replay];
        badges.sort_by_key(|badge| badge.favorite_rank());
        assert_eq!(
            badges,
            vec![Badge::Live, Badge::Replay, Badge::Offline, Badge::Unknown]
        );
    }
