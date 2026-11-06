//! Platform resolution for a long-lived relay. Confirmed offline/replay states
//! wait without consuming failure budget; transport errors remain errors.
use crate::{
    auth::CookieVault,
    core::{Platform, ResolvedStream, Settings, Target},
    platforms,
    relay::{ResolveWaiting, StreamResolver},
    room_state::{self, RoomObservation, RoomState},
};
use anyhow::Result;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// A verification failure after one failed silent refresh is a known room state,
/// not a media retry failure: the relay keeps the player connection waiting and
/// does not consume the reconnect budget while the shared gate cools down. The
/// state string is recognised by the relay pump alongside offline/replay.
fn verification_wait(_original: anyhow::Error) -> anyhow::Error {
    ResolveWaiting {
        state: "verification".into(),
        retry_after: Duration::from_secs(60),
    }
    .into()
}

pub trait BroadcastBackend: Send + Sync {
    fn resolve(&self) -> Result<ResolvedStream>;
    fn observe(&self) -> Result<RoomObservation>;
}
struct OfficialBackend {
    target: Target,
    settings: Settings,
    vault: CookieVault,
}
impl BroadcastBackend for OfficialBackend {
    fn resolve(&self) -> Result<ResolvedStream> {
        let first = match platforms::resolve_authenticated(&self.target, &self.settings, &self.vault)
        {
            Ok(stream) => return Ok(stream),
            Err(error) => error,
        };
        if self.target.platform != Platform::Douyin || !platforms::is_verification_required(&first) {
            return Err(first);
        }
        // A long-lived relay may reconnect into Douyin's verification page: allow
        // one silent refresh of this application's own session, then retry the
        // current room once. The shared single-flight/cooldown gate keeps a
        // reconnect loop from launching browsers repeatedly.
        crate::diag::log(
            "relay",
            crate::diag::Level::Warn,
            "douyin relay resolve reported a verification requirement; attempting one silent refresh",
        );
        let cancelled = || false;
        let refreshed = crate::douyin_guard::try_silent_refresh(
            &self.vault,
            &self.target.room,
            &cancelled,
        );
        match refreshed {
            Ok(true) => platforms::resolve_authenticated(&self.target, &self.settings, &self.vault),
            Ok(false) => Err(verification_wait(first)),
            Err(error) => {
                crate::diag::log(
                    "relay",
                    crate::diag::Level::Warn,
                    &format!("douyin relay silent refresh failed: {error:#}"),
                );
                Err(verification_wait(first))
            }
        }
    }
    fn observe(&self) -> Result<RoomObservation> {
        room_state::check(&self.target, Some(&self.vault))
    }
}
pub struct BroadcastResolver {
    backend: Arc<dyn BroadcastBackend>,
    cache: Mutex<Option<(ResolvedStream, Instant)>>,
    reuse: Duration,
    wait: bool,
}
impl BroadcastResolver {
    pub fn new(
        target: Target,
        settings: Settings,
        vault: CookieVault,
        initial: Option<ResolvedStream>,
    ) -> Self {
        let reuse = if matches!(target.platform, Platform::Huya | Platform::Douyin) {
            Duration::from_secs(20)
        } else {
            Duration::ZERO
        };
        let wait = settings.playback.wait_for_live;
        Self::with_backend(
            Arc::new(OfficialBackend {
                target,
                settings,
                vault,
            }),
            initial,
            reuse,
            wait,
        )
    }
    pub fn with_backend(
        backend: Arc<dyn BroadcastBackend>,
        initial: Option<ResolvedStream>,
        reuse: Duration,
        wait: bool,
    ) -> Self {
        Self {
            backend,
            cache: Mutex::new(initial.map(|stream| (stream, Instant::now()))),
            reuse,
            wait,
        }
    }
}
impl StreamResolver for BroadcastResolver {
    fn resolve(&self) -> Result<ResolvedStream> {
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if self.reuse.is_zero() {
                if let Some((stream, _)) = cache.take() {
                    return Ok(stream);
                }
            } else if let Some((stream, at)) = cache.as_ref() {
                if at.elapsed() < self.reuse {
                    return Ok(stream.clone());
                }
                *cache = None;
            }
        }
        match self.backend.resolve() {
            Ok(stream) => {
                if !self.reuse.is_zero() {
                    *self.cache.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((stream.clone(), Instant::now()));
                }
                Ok(stream)
            }
            Err(error) => {
                // A backend that already classified the wait (offline, replay
                // unavailable, verification) must not trigger a second room probe.
                if error.downcast_ref::<ResolveWaiting>().is_some() {
                    return Err(error);
                }
                if self.wait {
                    if let Ok(observation) = self.backend.observe() {
                        let state = match observation.state {
                            RoomState::Offline => Some("offline"),
                            RoomState::Replay => Some("replay_unavailable"),
                            _ => None,
                        };
                        if let Some(state) = state {
                            return Err(ResolveWaiting {
                                state: state.into(),
                                retry_after: Duration::from_secs(20),
                            }
                            .into());
                        }
                    }
                }
                Err(error)
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Backend {
        calls: AtomicUsize,
        state: RoomState,
        works: bool,
    }
    impl BroadcastBackend for Backend {
        fn resolve(&self) -> Result<ResolvedStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.works {
                Ok(media())
            } else {
                anyhow::bail!("synthetic unavailable")
            }
        }
        fn observe(&self) -> Result<RoomObservation> {
            Ok(RoomObservation {
                state: self.state,
                title: None,
                note: String::new(),
            })
        }
    }
    fn media() -> ResolvedStream {
        ResolvedStream {
            url: "https://cdn.example.test/synthetic.flv".into(),
            title: "synthetic".into(),
            headers: Default::default(),
        }
    }
    #[test]
    fn known_offline_waits_but_unknown_keeps_error() {
        for state in [RoomState::Offline, RoomState::Replay, RoomState::Unknown] {
            let resolver = BroadcastResolver::with_backend(
                Arc::new(Backend {
                    calls: AtomicUsize::new(0),
                    state,
                    works: false,
                }),
                None,
                Duration::ZERO,
                true,
            );
            let error = resolver.resolve().unwrap_err();
            assert_eq!(
                error.downcast_ref::<ResolveWaiting>().is_some(),
                state != RoomState::Unknown
            );
        }
    }
    #[test]
    fn one_shot_direct_link_and_short_reuse_are_distinct() {
        for reuse in [Duration::ZERO, Duration::from_secs(20)] {
            let backend = Arc::new(Backend {
                calls: AtomicUsize::new(0),
                state: RoomState::Live,
                works: true,
            });
            let resolver =
                BroadcastResolver::with_backend(backend.clone(), Some(media()), reuse, true);
            resolver.resolve().unwrap();
            resolver.resolve().unwrap();
            assert_eq!(
                backend.calls.load(Ordering::SeqCst),
                if reuse.is_zero() { 1 } else { 0 }
            );
        }
    }
}
