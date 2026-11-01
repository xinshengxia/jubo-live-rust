//! Production reconnect-budget regression fixtures. No live platform or player.
use anyhow::Result;
use live_hub_rust::{
    core::ResolvedStream,
    flv::{self, FlvTag},
    relay::{self, RelayConfig, RelayConnectionState, RelayFailure, RelayServer},
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const HEADER: &[u8] = b"FLV\x01\x05\0\0\0\x09\0\0\0\0";
const TOKEN: &str = "synthetic-recovery-session";

fn config() -> RelayConfig {
    RelayConfig {
        token: TOKEN.into(),
        allow_loopback_fixture: true,
        idle_timeout: Duration::from_millis(300),
        upstream_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(3),
        retry_delay: Duration::ZERO,
        ..Default::default()
    }
}

fn connect(address: SocketAddr) -> TcpStream {
    let mut player = TcpStream::connect(address).unwrap();
    player
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write!(
        player,
        "GET /live.flv?t={TOKEN} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        player.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    assert!(header.starts_with(b"HTTP/1.1 200 OK\r\n"));
    player
}

fn upstream_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
}

fn productive_segment(index: u32) -> Vec<u8> {
    let mut body = HEADER.to_vec();
    body.extend_from_slice(FlvTag::new(9, 0, &[0x17, 0, 0, 0, 0]).unwrap().as_bytes());
    let mut payload = vec![0x5a; 512];
    payload[0] = 0x27;
    payload[1] = 1;
    for tag in 0..relay::STABLE_MEDIA_TAGS as u32 {
        body.extend_from_slice(
            FlvTag::new(9, index * 2_000 + tag * 50, &payload)
                .unwrap()
                .as_bytes(),
        );
    }
    body
}

#[test]
fn production_budget_survives_more_than_five_hundred_productive_reconnects() {
    let started = Instant::now();
    // Keep the production budget and progress thresholds. Only wall-clock retry
    // delay is accelerated: a 500-second sleep would add no boundary coverage.
    let cfg = config();
    assert_eq!(cfg.max_reconnects, 500);
    let count = cfg.max_reconnects + 3;
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/synthetic.flv", source.local_addr().unwrap());
    let (stop, stopped) = mpsc::sync_channel(1);
    let upstream = thread::spawn(move || {
        for index in 0..count {
            let (mut stream, _) = source.accept().unwrap();
            upstream_request(&mut stream);
            let body = productive_segment(index as u32);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
        // Keep the listener alive while the downstream inspects statistics and
        // closes. Its next connect may queue, but cannot burn a failure budget.
        let _ = stopped.recv_timeout(Duration::from_secs(10));
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let resolver = Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(ResolvedStream {
            url: url.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        })
    });
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let control_url = format!("http://{address}/live.flv?t={TOKEN}");
    let relay = thread::spawn(move || server.run());
    let mut player = connect(address);
    flv::read_header(&mut player).unwrap();
    let wanted = count * relay::STABLE_MEDIA_TAGS as usize;
    let mut media = 0;
    while media < wanted {
        let tag = flv::read_tag(&mut player)
            .unwrap()
            .expect("relay ended at a cumulative reconnect limit");
        if matches!(tag.kind(), 8 | 9) && !tag.is_codec_header() {
            media += 1;
        }
    }
    let stats = relay::query_statistics(&control_url).unwrap();
    assert!(stats.stable_segments >= count as u64, "{stats:?}");
    assert!(stats.reconnects > 500, "{stats:?}");
    assert_eq!(stats.budget_exhaustions, 0, "{stats:?}");
    assert!(calls.load(Ordering::SeqCst) >= count);
    println!(
        "production_recovery_snapshot={}",
        serde_json::json!({
            "production_budget":500,"productive_segments_requested":count,
            "elapsed_ms":started.elapsed().as_millis(),"statistics":stats,
        })
    );
    player.shutdown(Shutdown::Both).unwrap();
    drop(player);
    let _ = stop.send(());
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();
}

#[test]
fn consecutive_resolution_failures_exhaust_the_configured_budget_and_are_observable() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let resolver = Arc::new(move || -> Result<ResolvedStream> {
        observed.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("synthetic resolver failure")
    });
    let mut cfg = config();
    cfg.max_reconnects = 2;
    cfg.idle_timeout = Duration::from_secs(1);
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let mut player = connect(address);
    let mut body = Vec::new();
    player.read_to_end(&mut body).unwrap();
    assert!(body.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let stats = relay::query_statistics(&format!("http://{address}/live.flv?t={TOKEN}")).unwrap();
    assert_eq!(stats.budget_exhaustions, 1);
    assert_eq!(stats.failures, 3);
    assert_eq!(stats.connections[0].state, RelayConnectionState::Exhausted);
    assert_eq!(stats.connections[0].consecutive_failures, 3);
    assert_eq!(
        stats.connections[0].last_error,
        Some(RelayFailure::RetryBudgetExhausted)
    );
    relay.join().unwrap().unwrap();
}

#[test]
fn headers_codec_and_tiny_static_media_never_clear_a_failure_budget() {
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/tiny", source.local_addr().unwrap());
    let upstream = thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _) = source.accept().unwrap();
            upstream_request(&mut stream);
            let mut body = HEADER.to_vec();
            for (kind, payload) in [
                (18, vec![7; 16 * 1024]),
                (9, vec![0x17, 0, 1]),
                (9, vec![0x27, 1, 1]),
            ] {
                body.extend_from_slice(FlvTag::new(kind, 0, &payload).unwrap().as_bytes());
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    let resolver = Arc::new(move || {
        Ok(ResolvedStream {
            url: url.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        })
    });
    let mut cfg = config();
    cfg.max_reconnects = 2;
    cfg.idle_timeout = Duration::from_secs(1);
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let started = Instant::now();
    let mut player = connect(address);
    let mut body = Vec::new();
    player.read_to_end(&mut body).unwrap();
    let stats = relay::query_statistics(&format!("http://{address}/live.flv?t={TOKEN}")).unwrap();
    assert_eq!(stats.stable_segments, 0);
    assert_eq!(stats.upstream_attempts, 3);
    assert_eq!(stats.budget_exhaustions, 1);
    assert!(
        started.elapsed() >= Duration::from_millis(10),
        "zero configured delay must still yield between retries"
    );
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();
}

#[test]
fn stable_media_resets_earlier_failures_before_later_outages() {
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/recovered", source.local_addr().unwrap());
    let upstream = thread::spawn(move || {
        let (mut stream, _) = source.accept().unwrap();
        upstream_request(&mut stream);
        let body = productive_segment(1);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let resolver = Arc::new(move || -> Result<ResolvedStream> {
        let index = observed.fetch_add(1, Ordering::SeqCst);
        if index != 2 {
            anyhow::bail!("synthetic outage");
        }
        Ok(ResolvedStream {
            url: url.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        })
    });
    let mut cfg = config();
    cfg.max_reconnects = 2;
    cfg.idle_timeout = Duration::from_secs(1);
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let mut player = connect(address);
    let mut body = Vec::new();
    player.read_to_end(&mut body).unwrap();
    let stats = relay::query_statistics(&format!("http://{address}/live.flv?t={TOKEN}")).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert_eq!(stats.stable_segments, 1);
    assert_eq!(stats.failures, 5);
    assert_eq!(stats.budget_exhaustions, 1);
    assert_eq!(stats.connections[0].consecutive_failures, 3);
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();
}

#[test]
fn statistics_require_the_token_and_never_resolve_or_echo_sensitive_values() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let resolver = Arc::new(move || -> Result<ResolvedStream> {
        observed.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("must never resolve")
    });
    let mut cfg = config();
    cfg.idle_timeout = Duration::from_millis(300);
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    assert_eq!(server.statistics().total_streams, 0);
    let relay = thread::spawn(move || server.run());
    assert!(relay::query_statistics(&format!("http://{address}/live.flv?t=wrong")).is_err());
    let stats = relay::query_statistics(&format!("http://{address}/live.flv?t={TOKEN}")).unwrap();
    let json = serde_json::to_string(&stats).unwrap();
    assert!(!json.contains(TOKEN));
    assert!(!json.contains("Cookie"));
    assert!(!json.contains("http"));
    assert_eq!(stats.total_streams, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for bad in [
        "https://127.0.0.1:80/live.flv?t=x",
        "http://localhost:80/live.flv?t=x",
        "http://8.8.8.8:80/live.flv?t=x",
        "http://127.0.0.1:80/live.flv?t=x&t=y",
    ] {
        assert!(relay::query_statistics(bad).is_err());
    }
    relay.join().unwrap().unwrap();
}

#[test]
fn positively_offline_and_replay_states_keep_the_same_player_without_spending_failure_budget() {
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/back-live", source.local_addr().unwrap());
    let (stop, stopped) = mpsc::sync_channel(1);
    let upstream = thread::spawn(move || {
        let (mut stream, _) = source.accept().unwrap();
        upstream_request(&mut stream);
        let body = productive_segment(1);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
        let _ = stopped.recv_timeout(Duration::from_secs(5));
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let resolver = Arc::new(move || -> Result<ResolvedStream> {
        let index = observed.fetch_add(1, Ordering::SeqCst);
        if index < 4 {
            return Err(relay::ResolveWaiting {
                state: if index % 2 == 0 {
                    "offline"
                } else {
                    "replay_unavailable"
                }
                .into(),
                retry_after: Duration::from_secs(1),
            }
            .into());
        }
        Ok(ResolvedStream {
            url: url.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        })
    });
    let mut cfg = config();
    cfg.max_reconnects = 1;
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let mut player = connect(address);
    let control = format!("http://{address}/live.flv?t={TOKEN}");
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        let stats = relay::query_statistics(&control).unwrap();
        if stats
            .connections
            .first()
            .is_some_and(|c| c.state == RelayConnectionState::Waiting)
        {
            assert_eq!(
                stats.connections[0].waiting_state.as_deref(),
                Some("offline")
            );
            assert_eq!(stats.failures, 0);
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    flv::read_header(&mut player).unwrap();
    let mut media = 0;
    while media < relay::STABLE_MEDIA_TAGS {
        let tag = flv::read_tag(&mut player).unwrap().unwrap();
        if !tag.is_codec_header() {
            media += 1;
        }
    }
    let stats = relay::query_statistics(&control).unwrap();
    assert!(stats.stable_segments >= 1);
    assert_eq!(stats.budget_exhaustions, 0);
    assert_eq!(stats.failures, 0);
    assert!(calls.load(Ordering::SeqCst) >= 5);
    player.shutdown(Shutdown::Both).unwrap();
    drop(player);
    let _ = stop.send(());
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();
}

#[test]
fn unknown_waiting_state_does_not_disguise_a_network_failure_as_offline() {
    let resolver = Arc::new(|| -> Result<ResolvedStream> {
        Err(relay::ResolveWaiting {
            state: "network_error".into(),
            retry_after: Duration::from_secs(60),
        }
        .into())
    });
    let mut cfg = config();
    cfg.max_reconnects = 0;
    cfg.idle_timeout = Duration::from_secs(1);
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let mut player = connect(address);
    let mut body = Vec::new();
    player.read_to_end(&mut body).unwrap();
    let stats = relay::query_statistics(&format!("http://{address}/live.flv?t={TOKEN}")).unwrap();
    assert_eq!(stats.failures, 1);
    assert_eq!(stats.connections[0].waiting_state, None);
    assert_eq!(
        stats.connections[0].last_error,
        Some(RelayFailure::ResolveFailed)
    );
    relay.join().unwrap().unwrap();
}

fn amf_number(key: &str, value: f64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(key.len() as u16).to_be_bytes());
    bytes.extend_from_slice(key.as_bytes());
    bytes.push(0x00);
    bytes.extend_from_slice(&value.to_be_bytes());
    bytes
}

fn metadata_segment(kbps: f64) -> Vec<u8> {
    let mut payload = vec![0x02];
    let name = b"onMetaData";
    payload.extend_from_slice(&(name.len() as u16).to_be_bytes());
    payload.extend_from_slice(name);
    payload.push(0x08);
    payload.extend_from_slice(&5u32.to_be_bytes());
    payload.extend_from_slice(&amf_number("width", 1920.0));
    payload.extend_from_slice(&amf_number("height", 1080.0));
    payload.extend_from_slice(&amf_number("framerate", 60.0));
    payload.extend_from_slice(&amf_number("videodatarate", kbps));
    payload.extend_from_slice(&amf_number("videocodecid", 7.0));
    payload.extend_from_slice(&[0, 0, 9]);
    let mut body = HEADER.to_vec();
    body.extend_from_slice(FlvTag::new(18, 0, &payload).unwrap().as_bytes());
    body.extend_from_slice(&productive_segment(1)[HEADER.len()..]);
    body
}

#[test]
fn flv_on_metadata_reports_the_stream_summary_for_title_hints() {
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/metadata", source.local_addr().unwrap());
    let upstream = thread::spawn(move || {
        let (mut stream, _) = source.accept().unwrap();
        upstream_request(&mut stream);
        let body = metadata_segment(2500.0);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    let resolver = Arc::new(move || {
        Ok(ResolvedStream {
            url: url.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        })
    });
    let mut cfg = config();
    cfg.max_reconnects = 0;
    cfg.idle_timeout = Duration::from_secs(1);
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let mut player = connect(address);
    flv::read_header(&mut player).unwrap();
    let mut media = 0;
    while media < relay::STABLE_MEDIA_TAGS {
        let tag = flv::read_tag(&mut player).unwrap().unwrap();
        if matches!(tag.kind(), 8 | 9) && !tag.is_codec_header() {
            media += 1;
        }
    }
    let stats = relay::query_statistics(&format!("http://{address}/live.flv?t={TOKEN}")).unwrap();
    let info = stats.stream_info.clone().expect("onMetaData summary");
    assert_eq!(info.width, 1920);
    assert_eq!(info.height, 1080);
    assert_eq!(info.fps, 60.0);
    assert_eq!(info.video_kbps, 2500);
    assert_eq!(info.codec.as_deref(), Some("h264"));
    // The launcher telemetry exposes only the bounded bitrate for the title hint.
    let telemetry = live_hub_rust::playback::RelayTelemetry::from_statistics(&stats);
    assert_eq!(telemetry.stream_bitrate_kbps, Some(2500));
    player.shutdown(Shutdown::Both).unwrap();
    drop(player);
    relay.join().unwrap().unwrap();
    upstream.join().unwrap();
}
