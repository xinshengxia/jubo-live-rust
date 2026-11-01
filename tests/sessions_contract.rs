//! Synthetic session workbench tests: no platform requests, windows or players.
use anyhow::Result;
use live_hub_rust::auth::CookieVault;
use live_hub_rust::core::{Player, Settings, Target};
use live_hub_rust::playback::{
    LaunchCancelled, LaunchOutcome, PlaybackControl, PlaybackEvent, PlaybackObserver,
};
use live_hub_rust::sessions::{
    MAX_ACTIVE_SESSIONS, SessionLauncher, SessionManager, SessionPhase, SessionRoomState,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Mode {
    ResolveUntilCancel,
    Play,
    RelayFails,
    Fail,
    StaleAfterCancel,
}
struct SyntheticLauncher {
    mode: Mode,
    calls: AtomicUsize,
}
impl SessionLauncher for SyntheticLauncher {
    fn launch(
        &self,
        _: &Target,
        _: Player,
        _: &Settings,
        _: Option<&CookieVault>,
        control: &PlaybackControl,
        observer: PlaybackObserver,
    ) -> Result<LaunchOutcome> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        observer(PlaybackEvent::CheckingPlayer);
        observer(PlaybackEvent::Resolving);
        if matches!(self.mode, Mode::Fail) {
            anyhow::bail!("synthetic platform failure");
        }
        if matches!(self.mode, Mode::ResolveUntilCancel | Mode::StaleAfterCancel) {
            while !control.is_start_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            if matches!(self.mode, Mode::StaleAfterCancel) {
                observer(PlaybackEvent::Playing {
                    title: "late result".into(),
                    player_pid: 999,
                });
            }
            return Err(LaunchCancelled.into());
        }
        observer(PlaybackEvent::WaitingRelay);
        observer(PlaybackEvent::StartingPlayer);
        observer(PlaybackEvent::RegisteringPlayer);
        observer(PlaybackEvent::Playing {
            title: format!("synthetic {index}"),
            player_pid: 1000 + index as u32,
        });
        let control = control.clone();
        let mode = self.mode;
        thread::spawn(move || {
            if matches!(mode, Mode::RelayFails) {
                thread::sleep(Duration::from_millis(10));
                observer(PlaybackEvent::RelayExited { exit_code: Some(7) });
            }
            while !control.is_stop_requested() {
                thread::sleep(Duration::from_millis(2));
            }
            observer(PlaybackEvent::Stopped);
        });
        Ok(LaunchOutcome {
            title: format!("synthetic {index}"),
            player_pid: 1000 + index as u32,
        })
    }
}

fn manager(mode: Mode) -> SessionManager {
    SessionManager::with_launcher(Arc::new(SyntheticLauncher {
        mode,
        calls: AtomicUsize::new(0),
    }))
}
fn settings() -> Settings {
    let mut settings = Settings::default();
    settings.playback.monitor_room_state = false;
    settings
}
fn target() -> Target {
    Target::parse("huya:synthetic").unwrap()
}
fn wait(manager: &SessionManager, id: &str, phase: SessionPhase) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if manager.snapshot(id).unwrap().phase == phase {
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("expected {phase:?}, got {:?}", manager.snapshot(id));
}

#[test]
fn startup_is_asynchronous_and_cancellation_does_not_promote_late_results() {
    let manager = manager(Mode::StaleAfterCancel);
    let start = Instant::now();
    let id = manager
        .start(target(), Player::Mpv, settings(), None)
        .unwrap();
    assert!(start.elapsed() < Duration::from_millis(500));
    wait(&manager, &id, SessionPhase::Resolving);
    manager.cancel_start(&id).unwrap();
    wait(&manager, &id, SessionPhase::Cancelled);
    thread::sleep(Duration::from_millis(30));
    let snapshot = manager.snapshot(&id).unwrap();
    assert_eq!(snapshot.phase, SessionPhase::Cancelled);
    assert_eq!(snapshot.player_pid, None);
    assert!(snapshot.ended_at.is_some());
    assert!(manager.stop(&id).is_err());
}

#[test]
fn stop_addresses_one_owned_uuid_and_leaves_other_sessions_playing() {
    let manager = manager(Mode::Play);
    let first = manager
        .start(target(), Player::Mpv, settings(), None)
        .unwrap();
    let second = manager
        .start(target(), Player::PotPlayer, settings(), None)
        .unwrap();
    assert_ne!(first, second);
    wait(&manager, &first, SessionPhase::Playing);
    wait(&manager, &second, SessionPhase::Playing);
    assert!(manager.cancel_start(&first).is_err());
    manager.stop(&first).unwrap();
    wait(&manager, &first, SessionPhase::Stopped);
    assert_eq!(
        manager.snapshot(&second).unwrap().phase,
        SessionPhase::Playing
    );
    assert_eq!(manager.clear_finished(), 1);
    assert!(manager.snapshot(&first).is_none());
    assert!(manager.snapshot(&second).is_some());
    manager.stop(&second).unwrap();
    wait(&manager, &second, SessionPhase::Stopped);
}

#[test]
fn a_relay_exit_is_visible_and_does_not_falsely_report_the_player_ended() {
    let manager = manager(Mode::RelayFails);
    let id = manager
        .start(target(), Player::Mpv, settings(), None)
        .unwrap();
    wait(&manager, &id, SessionPhase::Interrupted);
    let snapshot = manager.snapshot(&id).unwrap();
    assert!(snapshot.error.unwrap().contains('7'));
    assert!(snapshot.player_pid.is_some());
    assert!(snapshot.ended_at.is_none());
    manager.stop(&id).unwrap();
    wait(&manager, &id, SessionPhase::Stopped);
}

#[test]
fn unknown_replay_and_offline_statuses_remain_distinct_from_playback_lifetime() {
    let manager = manager(Mode::Play);
    let id = manager
        .start(target(), Player::Mpv, settings(), None)
        .unwrap();
    wait(&manager, &id, SessionPhase::Playing);
    for state in [
        SessionRoomState::Offline,
        SessionRoomState::Replay,
        SessionRoomState::Unknown,
        SessionRoomState::Live,
    ] {
        manager.update_room_status(
            &id,
            state,
            Some("server title".into()),
            "synthetic observation".into(),
        );
        let snapshot = manager.snapshot(&id).unwrap();
        assert_eq!(snapshot.room_state, state);
        assert_eq!(snapshot.phase, SessionPhase::Playing);
        assert_eq!(snapshot.title, "server title");
        assert!(snapshot.room_observed_at.is_some());
    }
    manager.stop(&id).unwrap();
    wait(&manager, &id, SessionPhase::Stopped);
    manager.update_room_status(&id, SessionRoomState::Offline, None, "late response".into());
    assert_eq!(
        manager.snapshot(&id).unwrap().room_state,
        SessionRoomState::Live
    );
}

#[test]
fn failed_launch_keeps_an_actionable_row_and_never_a_fake_pid() {
    let manager = manager(Mode::Fail);
    let events = manager.subscribe();
    let id = manager
        .start(target(), Player::Mpv, settings(), None)
        .unwrap();
    wait(&manager, &id, SessionPhase::Failed);
    let snapshot = manager.snapshot(&id).unwrap();
    assert!(
        snapshot
            .error
            .unwrap()
            .contains("synthetic platform failure")
    );
    assert_eq!(snapshot.player_pid, None);
    assert!(snapshot.ended_at.is_some());
    assert!(
        events
            .try_iter()
            .any(|snapshot| snapshot.phase == SessionPhase::Failed)
    );
    assert_eq!(manager.clear_finished(), 1);
}

#[test]
fn active_startups_are_bounded_and_validation_happens_before_scheduling() {
    let manager = manager(Mode::ResolveUntilCancel);
    let mut invalid = target();
    invalid.room = "../private".into();
    assert!(
        manager
            .start(invalid, Player::Mpv, settings(), None)
            .is_err()
    );
    assert!(manager.snapshots().is_empty());
    let ids: Vec<_> = (0..MAX_ACTIVE_SESSIONS)
        .map(|_| {
            manager
                .start(target(), Player::Mpv, settings(), None)
                .unwrap()
        })
        .collect();
    assert!(
        manager
            .start(target(), Player::Mpv, settings(), None)
            .is_err()
    );
    assert_eq!(manager.snapshots().len(), MAX_ACTIVE_SESSIONS);
    for id in ids {
        manager.cancel_start(&id).unwrap();
    }
    assert_eq!(manager.clear_finished(), MAX_ACTIVE_SESSIONS);
}

#[test]
fn state_events_include_title_pid_and_ordered_utc_timestamps() {
    let manager = manager(Mode::Play);
    let events = manager.subscribe();
    let id = manager
        .start(target(), Player::Mpv, settings(), None)
        .unwrap();
    wait(&manager, &id, SessionPhase::Playing);
    let snapshot = manager.snapshot(&id).unwrap();
    assert_eq!(snapshot.id.len(), 32);
    assert_eq!(snapshot.player_pid, Some(1000));
    assert_eq!(snapshot.title, "synthetic 0");
    assert!(snapshot.updated_at >= snapshot.started_at);
    let phases: Vec<_> = events.try_iter().map(|snapshot| snapshot.phase).collect();
    assert_eq!(phases.first(), Some(&SessionPhase::Queued));
    assert_eq!(phases.last(), Some(&SessionPhase::Playing));
    manager.stop(&id).unwrap();
    wait(&manager, &id, SessionPhase::Stopped);
}
