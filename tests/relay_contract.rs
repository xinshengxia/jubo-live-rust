use anyhow::Result;
use live_hub_rust::{
    core::ResolvedStream,
    diag,
    flv::{self, FlvTag},
    relay::{RelayConfig, RelayServer, ResolveWaiting},
};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

const HEADER: &[u8] = b"FLV\x01\x05\0\0\0\x09\0\0\0\0";

fn fixture(tags: &[(u8, u32, &[u8])]) -> Vec<u8> {
    let mut output = HEADER.to_vec();
    for (kind, timestamp, payload) in tags {
        output.extend_from_slice(FlvTag::new(*kind, *timestamp, payload).unwrap().as_bytes());
    }
    output
}

fn upstream(responses: Vec<Vec<u8>>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/synthetic.flv", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        for body in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            let _ = stream.write_all(&body);
        }
    });
    (url, worker)
}

fn config() -> RelayConfig {
    RelayConfig {
        token: "synthetic-session".into(),
        idle_timeout: Duration::from_millis(100),
        retry_delay: Duration::from_millis(5),
        upstream_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_millis(200),
        write_timeout: Duration::from_millis(500),
        allow_loopback_fixture: true,
        ..Default::default()
    }
}

fn start(
    config: RelayConfig,
    url: String,
) -> (
    std::net::SocketAddr,
    thread::JoinHandle<Result<()>>,
    Arc<AtomicUsize>,
) {
    let count = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&count);
    let resolver = Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(ResolvedStream {
            url: url.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        })
    });
    let server = RelayServer::bind(config, resolver).unwrap();
    let address = server.local_addr().unwrap();
    (address, thread::spawn(move || server.run()), count)
}

fn player(address: std::net::SocketAddr, token: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        stream,
        "GET /live.flv?t={token} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    )
    .unwrap();
    stream
}

fn response(mut stream: TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    if let Err(error) = stream.read_to_end(&mut bytes) {
        assert!(
            error.kind() == std::io::ErrorKind::ConnectionReset && !bytes.is_empty(),
            "{error}"
        );
    }
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (
        String::from_utf8(bytes[..split].to_vec()).unwrap(),
        bytes[split..].to_vec(),
    )
}

#[test]
fn binds_zero_port_rejects_public_bind_and_connection_budget_above_eight() {
    let resolver = Arc::new(|| -> Result<ResolvedStream> { anyhow::bail!("synthetic offline") });
    let mut cfg = config();
    cfg.bind = "0.0.0.0:0".parse().unwrap();
    assert!(RelayServer::bind(cfg, resolver.clone()).is_err());
    let mut cfg = config();
    cfg.max_connections = 9;
    assert!(RelayServer::bind(cfg, resolver.clone()).is_err());
    let server = RelayServer::bind(config(), resolver).unwrap();
    assert!(server.local_addr().unwrap().ip().is_loopback());
    assert_ne!(server.local_addr().unwrap().port(), 0);
}

#[test]
fn token_is_required_before_resolving_any_upstream() {
    let (address, relay, count) = start(config(), "http://127.0.0.1:1/not-used".into());
    let (header, body) = response(player(address, "incorrect"));
    assert!(header.starts_with("HTTP/1.1 403"));
    assert!(body.is_empty());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    relay.join().unwrap().unwrap();
}

#[test]
fn reconnect_preserves_headers_drops_overlap_and_never_splices_partial_tags() {
    let mut first = fixture(&[(8, 2000, &[0xaf, 1, 1]), (9, 2000, &[0x27, 1, 2])]);
    let partial = FlvTag::new(9, 2001, &[0x27, 1, 99, 99, 99]).unwrap();
    first.extend_from_slice(&partial.as_bytes()[..13]);
    let second = fixture(&[
        (9, 0, &[0x17, 0, 3]),
        (8, 1990, &[0xaf, 1, 4]),
        (9, 1990, &[0x27, 1, 5]),
        (8, 2010, &[0xaf, 1, 6]),
        (9, 2010, &[0x27, 1, 7]),
    ]);
    let (url, upstream) = upstream(vec![first, second]);
    let mut cfg = config();
    cfg.max_reconnects = 1;
    let (address, relay, count) = start(cfg, url);
    let (header, body) = response(player(address, "synthetic-session"));
    assert!(header.starts_with("HTTP/1.1 200"));
    assert!(!header.to_ascii_lowercase().contains("content-length"));
    assert!(!header.to_ascii_lowercase().contains("transfer-encoding"));
    assert!(header.contains("Connection: close"));
    let mut cursor = std::io::Cursor::new(body);
    assert_eq!(flv::read_header(&mut cursor).unwrap(), HEADER);
    let mut tags = Vec::new();
    while let Some(tag) = flv::read_tag(&mut cursor).unwrap() {
        tags.push(tag);
    }
    assert_eq!(
        tags.iter()
            .map(|t| (t.kind(), t.timestamp()))
            .collect::<Vec<_>>(),
        vec![(8, 2000), (9, 2000), (9, 2000), (8, 2010), (9, 2010)]
    );
    assert!(tags[2].is_codec_header());
    assert_eq!(count.load(Ordering::SeqCst), 2);
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();
}

#[test]
fn connection_budget_rejects_excess_idle_player_sockets() {
    let mut cfg = config();
    cfg.max_connections = 1;
    cfg.request_timeout = Duration::from_millis(500);
    let (address, relay, count) = start(cfg, "http://127.0.0.1:1/not-used".into());
    let held = TcpStream::connect(address).unwrap();
    thread::sleep(Duration::from_millis(50));
    let (header, _) = response(player(address, "synthetic-session"));
    assert!(header.starts_with("HTTP/1.1 503"));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    drop(held);
    relay.join().unwrap().unwrap();
}

#[test]
fn disconnected_player_is_not_re_resolved_after_upstream_eof() {
    let (url, upstream) = upstream(vec![fixture(&[(9, 1000, &[0x27, 1, 7])])]);
    let mut cfg = config();
    cfg.retry_delay = Duration::from_millis(300);
    let (address, relay, count) = start(cfg, url);
    let mut stream = player(address, "synthetic-session");
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    let mut media = vec![
        0;
        HEADER.len()
            + FlvTag::new(9, 1000, &[0x27, 1, 7])
                .unwrap()
                .as_bytes()
                .len()
    ];
    stream.read_exact(&mut media).unwrap();
    stream.shutdown(Shutdown::Both).unwrap();
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !relay.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        relay.is_finished(),
        "relay should leave after player disconnect and idle deadline"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();
}

#[test]
fn healthy_continuous_body_outlives_each_read_timeout_without_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/continuous", listener.local_addr().unwrap());
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
            .unwrap();
        stream.write_all(HEADER).unwrap();
        for number in 0..20 {
            let tag = FlvTag::new(9, number * 40, &[0x27, 1, 7]).unwrap();
            stream.write_all(tag.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(40));
        }
    });
    let mut cfg = config();
    cfg.upstream_timeout = Duration::from_millis(200);
    cfg.max_reconnects = 0;
    let (address, relay, calls) = start(cfg, url);
    let start = Instant::now();
    let (_, body) = response(player(address, "synthetic-session"));
    assert!(start.elapsed() > Duration::from_millis(600));
    let mut cursor = std::io::Cursor::new(body);
    flv::read_header(&mut cursor).unwrap();
    let mut count = 0;
    while flv::read_tag(&mut cursor).unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 20, "blocking Read timeout must reset for every read");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();
}

#[test]
fn stalled_partial_tag_and_player_disconnect_release_the_connection_within_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/stalled", listener.local_addr().unwrap());
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
            .unwrap();
        stream.write_all(HEADER).unwrap();
        let tag = FlvTag::new(9, 10, &[0x27, 1, 7]).unwrap();
        stream.write_all(&tag.as_bytes()[..12]).unwrap();
        thread::sleep(Duration::from_millis(900));
    });
    let mut cfg = config();
    cfg.upstream_timeout = Duration::from_millis(200);
    let (address, relay, calls) = start(cfg, url);
    let mut stream = player(address, "synthetic-session");
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    let mut file_header = [0; 13];
    stream.read_exact(&mut file_header).unwrap();
    let start = Instant::now();
    stream.shutdown(Shutdown::Both).unwrap();
    drop(stream);
    let deadline = start + Duration::from_millis(750);
    while !relay.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        relay.is_finished(),
        "stalled reader must release after read timeout and idle deadline"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    relay.join().unwrap().unwrap();
    upstream.join().unwrap();
}

#[test]
fn production_policy_rejects_private_upstream_without_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/private", listener.local_addr().unwrap());
    let mut cfg = config();
    cfg.allow_loopback_fixture = false;
    cfg.max_reconnects = 0;
    let (address, relay, calls) = start(cfg, url);
    let (_, body) = response(player(address, "synthetic-session"));
    assert!(body.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    relay.join().unwrap().unwrap();
}

#[test]
fn literal_private_media_address_is_policy_rejected_with_redacted_statistics_and_log() {
    // The old code required a fixed CDN family and upgraded http; the new policy
    // accepts unknown public families (proved by the unit-level DNS tests) but a
    // literal private address must still be refused before any connection. The
    // recorded diagnostics name only rule/stage/host — never the signed URL.
    let directory = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.allow_loopback_fixture = false;
    cfg.max_reconnects = 0;
    cfg.idle_timeout = Duration::from_secs(1);
    cfg.log_dir = Some(directory.path().join("logs"));
    let signed = "https://127.0.0.1:9/synthetic.flv?token=SIGNED_SYNTHETIC_PLACEHOLDER";
    let (address, relay, calls) = start(cfg, signed.into());
    let (header, body) = response(player(address, "synthetic-session"));
    assert!(header.starts_with("HTTP/1.1 200"), "{header}");
    assert!(body.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let control = format!("http://{address}/live.flv?t=synthetic-session");
    let stats = live_hub_rust::relay::query_statistics(&control).unwrap();
    assert_eq!(stats.policy_rejections, 1);
    let reject = stats
        .last_policy_reject
        .as_ref()
        .expect("policy rejection recorded");
    assert_eq!(reject.rule, "media-host-literal-private");
    assert_eq!(reject.stage, "initial");
    assert_eq!(reject.host.as_deref(), Some("127.0.0.1"));
    relay.join().unwrap().unwrap();

    let path = diag::log_path(&directory.path().join("logs"), "relay");
    let text = std::fs::read_to_string(&path).expect("relay log");
    assert!(text.contains("rule=media-host-literal-private"), "{text}");
    assert!(text.contains("stage=initial"), "{text}");
    assert!(text.contains("host=127.0.0.1"), "{text}");
    assert!(
        !text.contains("SIGNED_SYNTHETIC_PLACEHOLDER"),
        "signed query leaked: {text}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("https://"),
        "media URL leaked: {text}"
    );
}

#[test]
fn upstream_redirect_to_loopback_is_never_followed_even_in_fixture_mode() {
    let destination = TcpListener::bind("127.0.0.1:0").unwrap();
    destination.set_nonblocking(true).unwrap();
    let location = format!(
        "http://{}/should-not-be-requested",
        destination.local_addr().unwrap()
    );
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/redirect", source.local_addr().unwrap());
    let upstream = thread::spawn(move || {
        let (mut stream, _) = source.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        write!(stream, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let mut cfg = config();
    cfg.max_reconnects = 0;
    let (address, relay, _) = start(cfg, url);
    let (_, body) = response(player(address, "synthetic-session"));
    assert!(body.is_empty());
    upstream.join().unwrap();
    assert_eq!(
        destination.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    relay.join().unwrap().unwrap();
}

#[test]
fn verification_wait_keeps_the_player_connection_without_consuming_the_retry_budget() {
    // The resolver first reports Douyin's post-refresh verification wait, then
    // succeeds. The relay must treat the wait as a known room state: keep the player
    // connection open, not count a failed media segment (max_reconnects = 0 would
    // close the session immediately), and resolve again only after the clamped
    // retry window (the 1 ms request is clamped up to 1 s).
    let (url, upstream) = upstream(vec![fixture(&[
        (9, 0, &[0x17, 0, 3]),
        (9, 1000, &[0x27, 1, 7]),
    ])]);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let resolver = Arc::new(move || -> Result<ResolvedStream> {
        let index = observed.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            return Err(ResolveWaiting {
                state: "verification".into(),
                retry_after: Duration::from_millis(1),
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
    cfg.max_reconnects = 0;
    cfg.retry_delay = Duration::from_millis(5);
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let started = Instant::now();
    let (header, body) = response(player(address, "synthetic-session"));
    let elapsed = started.elapsed();
    assert!(header.starts_with("HTTP/1.1 200"), "{header}");
    assert!(
        !body.is_empty(),
        "the verification wait must not close the player stream"
    );
    let mut cursor = std::io::Cursor::new(body);
    assert_eq!(flv::read_header(&mut cursor).unwrap(), HEADER);
    let tag = flv::read_tag(&mut cursor).unwrap().expect("forwarded tag");
    assert_eq!(tag.kind(), 9);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the relay must resolve once more after the wait"
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "retry_after must be clamped up to one second, waited only {elapsed:?}"
    );
    relay.join().unwrap().unwrap();
    upstream.join().unwrap();
}

#[test]
fn player_disconnect_during_a_blocked_resolve_closes_the_connection_without_waiting() {
    // Douyin's silent verification refresh can block resolver.resolve() for up to
    // ~25 s and has no cancellation handle. A player that closes during that window
    // must still release the relay connection promptly: the resolver runs on a
    // worker, the connection thread keeps polling the player socket, and the
    // worker's eventual result is discarded instead of streaming to a gone player.
    let destination = TcpListener::bind("127.0.0.1:0").unwrap();
    destination.set_nonblocking(true).unwrap();
    let resolved_url = format!(
        "http://{}/never-requested",
        destination.local_addr().unwrap()
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let resolver = Arc::new(move || -> Result<ResolvedStream> {
        observed.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_secs(3));
        Ok(ResolvedStream {
            url: resolved_url.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        })
    });
    let mut cfg = config();
    cfg.max_reconnects = 0;
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let mut stream = player(address, "synthetic-session");
    // The streaming response header is emitted before the first resolve call.
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    assert!(
        String::from_utf8(header).unwrap().starts_with("HTTP/1.1 200"),
        "relay did not start the streaming response"
    );
    thread::sleep(Duration::from_millis(100));
    let disconnected = Instant::now();
    stream.shutdown(Shutdown::Write).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut sink = [0u8; 64];
    loop {
        match stream.read(&mut sink) {
            Ok(0) => break,
            Ok(count) => panic!(
                "relay wrote {count} byte(s) to a disconnected player before closing"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(error) => panic!("player read failed: {error}"),
        }
    }
    assert!(
        disconnected.elapsed() < Duration::from_secs(1),
        "connection took {:?} to close; it waited for the blocked resolver",
        disconnected.elapsed()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the resolver must have been called once on its worker"
    );
    // The server keeps running until its idle deadline; it must stop cleanly while
    // the worker is still sleeping.
    let deadline = Instant::now() + Duration::from_secs(1);
    while !relay.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        relay.is_finished(),
        "relay server did not stop after the player disconnected"
    );
    relay.join().unwrap().unwrap();
    // The discarded resolver result must never open the upstream connection.
    assert_eq!(
        destination.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "a discarded resolve result must not dial the upstream"
    );
}

#[test]
fn relay_forwards_only_approved_headers_and_rejects_credentials_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/headers", listener.local_addr().unwrap());
    let (captured, receiver) = std::sync::mpsc::channel();
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        captured.send(String::from_utf8(header).unwrap()).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            HEADER.len()
        )
        .unwrap();
        stream.write_all(HEADER).unwrap();
    });
    let headers = BTreeMap::from([
        ("User-Agent".into(), "Synthetic-UA".into()),
        ("Referer".into(), "https://example.invalid/".into()),
        ("Origin".into(), "https://example.invalid".into()),
    ]);
    let resolver = Arc::new(move || {
        Ok(ResolvedStream {
            url: url.clone(),
            headers: headers.clone(),
            title: "Synthetic".into(),
        })
    });
    let mut cfg = config();
    cfg.max_reconnects = 0;
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let (_, body) = response(player(address, "synthetic-session"));
    assert_eq!(body, HEADER);
    let request = receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .to_ascii_lowercase();
    // The fixture URL is plain http and this listener answers it: if the relay
    // upgraded the initial address to https the capture would see a TLS
    // ClientHello instead of an HTTP request. User-Agent is always forwarded;
    // Referer survives on the initial host; Origin is never forwarded to a media
    // request; no credential header can appear.
    assert!(request.contains("user-agent: synthetic-ua"));
    assert!(request.contains("referer: https://example.invalid/"));
    assert!(!request.contains("origin:"));
    assert!(!request.contains("cookie:"));
    assert!(!request.contains("authorization:"));
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();

    for blocked in ["Cookie", "Authorization", "X-Api-Key"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/blocked", listener.local_addr().unwrap());
        let headers = BTreeMap::from([(blocked.to_owned(), "SYNTHETIC_PLACEHOLDER".into())]);
        let resolver = Arc::new(move || {
            Ok(ResolvedStream {
                url: url.clone(),
                headers: headers.clone(),
                title: "Synthetic".into(),
            })
        });
        let mut cfg = config();
        cfg.max_reconnects = 0;
        let server = RelayServer::bind(cfg, resolver).unwrap();
        let address = server.local_addr().unwrap();
        let relay = thread::spawn(move || server.run());
        let (_, body) = response(player(address, "synthetic-session"));
        assert!(body.is_empty());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        relay.join().unwrap().unwrap();
    }
}

#[test]
fn ninth_idle_socket_is_rejected_promptly_when_eight_are_held() {
    // The production relay caps live connections at eight; an excess player must
    // receive a fast HTTP rejection instead of waiting for an upstream timeout.
    let mut cfg = config();
    cfg.max_connections = 8;
    cfg.request_timeout = Duration::from_secs(2);
    cfg.idle_timeout = Duration::from_millis(100);
    let (address, relay, count) = start(cfg, "http://127.0.0.1:1/not-used".into());
    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(TcpStream::connect(address).unwrap());
    }
    // Give the accept loop time to register all eight idle sockets.
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let (header, body) = response(player(address, "synthetic-session"));
    assert!(header.starts_with("HTTP/1.1 503"), "{header}");
    assert!(body.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "excess socket waited {:?}",
        started.elapsed()
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "budget rejection must not resolve upstream"
    );
    drop(held);
    relay.join().unwrap().unwrap();
}

#[test]
fn idle_request_timeout_releases_the_connection_budget() {
    // A player socket that never sends a request must release its slot after
    // request_timeout so a later player is not permanently rejected.
    let mut cfg = config();
    cfg.max_connections = 1;
    cfg.request_timeout = Duration::from_millis(150);
    cfg.idle_timeout = Duration::from_millis(800);
    let (address, relay, count) = start(cfg, "http://127.0.0.1:1/not-used".into());
    let held = TcpStream::connect(address).unwrap();
    // The idle socket is rejected with 400 when request_timeout expires; by the
    // time the next player arrives its budget slot must be free again.
    thread::sleep(Duration::from_millis(300));
    let (header, body) = response(player(address, "wrong-token"));
    assert!(header.starts_with("HTTP/1.1 403"), "{header}");
    assert!(body.is_empty());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    drop(held);
    relay.join().unwrap().unwrap();
}

/// Verifies the parent/child protocol: the relay binary prints the actual bound
/// port on stdout and enforces the session token before any upstream work.
#[test]
fn relay_binary_reports_ready_port_and_rejects_wrong_tokens() {
    use std::process::{Child, Command, Stdio};
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let initial = ResolvedStream {
        url: "http://127.0.0.1:1/not-used.flv".into(),
        headers: BTreeMap::new(),
        title: "Synthetic".into(),
    };
    let mut child = Kill(
        Command::new(env!("CARGO_BIN_EXE_live-hub-rust"))
            .args([
                "relay",
                "douyu:123456",
                "--token",
                "synthetic-token",
                "--port",
                "0",
            ])
            .env(
                "LIVE_HUB_INITIAL_STREAM",
                serde_json::to_string(&initial).unwrap(),
            )
            .env("LIVE_HUB_RELAY_SETTINGS", "{}")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(stdout).read_line(&mut line);
        let _ = sender.send(line);
    });
    let line = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("relay never printed READY");
    let port: u16 = line
        .trim()
        .strip_prefix("READY ")
        .expect(&format!("unexpected relay stdout: {line:?}"))
        .parse()
        .unwrap();
    assert!(port > 0);
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let (header, body) = response(player(address, "wrong-token"));
    assert!(header.starts_with("HTTP/1.1 403"), "{header}");
    assert!(body.is_empty());
    // A valid token gets the streaming response header; the loopback upstream is
    // then rejected by the production policy, so the body stays empty.
    let (header, body) = response(player(address, "synthetic-token"));
    assert!(header.starts_with("HTTP/1.1 200"), "{header}");
    assert!(header.contains("Content-Type: video/x-flv"));
    assert!(body.is_empty());
}

#[test]
fn sustained_tag_rate_is_not_limited_by_per_tag_socket_polling() {
    // Regression: client_gone() used a blocking 1 ms peek before every relayed
    // tag. Windows rounds short socket timeouts up to the timer granularity
    // (~15.6 ms), which capped forwarding near 15 tags/second and made the relay
    // fall permanently behind a live FLV stream. 120 tags at a slow 5 ms interval
    // (200 tags/second produced) must be relayed without a multi-second backlog.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/sustained", listener.local_addr().unwrap());
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
            .unwrap();
        stream.write_all(HEADER).unwrap();
        for number in 0..120u32 {
            let tag = FlvTag::new(9, number * 10, &[0x27, 1, 7]).unwrap();
            stream.write_all(tag.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(5));
        }
    });
    let mut cfg = config();
    cfg.upstream_timeout = Duration::from_millis(500);
    cfg.max_reconnects = 0;
    let (address, relay, calls) = start(cfg, url);
    let started = Instant::now();
    let (_, body) = response(player(address, "synthetic-session"));
    let elapsed = started.elapsed();
    let mut cursor = std::io::Cursor::new(body);
    flv::read_header(&mut cursor).unwrap();
    let mut count = 0;
    while flv::read_tag(&mut cursor).unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 120);
    assert!(
        elapsed < Duration::from_millis(2500),
        "relay needed {elapsed:?} for 120 tags; per-tag polling throttled it"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    upstream.join().unwrap();
    relay.join().unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// Performance measurements
//
// Ignored by default: they assert only liveness (every tag/byte forwarded) and
// print one JSON line per workload. The supported invocation is
//   cargo test --release --no-default-features --test relay_contract -- --ignored --nocapture
// scripts/Bench-Perf.ps1 builds the binary, runs these in order and collects
// the JSON lines into a table. They are not part of the correctness gate.
// ---------------------------------------------------------------------------

fn median_of(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// End-to-end loopback forward rate: synthetic upstream -> relay -> player.
/// Two workloads: many tiny tags (per-tag overhead) and live-sized video tags
/// (bytes per second). First-byte latency is measured from the player request.
#[test]
#[ignore = "performance measurement; run scripts/Bench-Perf.ps1"]
fn perf_relay_forwarding_throughput() {
    for (label, tag_count, payload_bytes) in [
        ("small-256b", 20_000_usize, 256_usize),
        ("video-32k", 1_024, 32 * 1024),
    ] {
        let mut payload = vec![0x27_u8, 1, 7];
        payload.resize(payload_bytes, 0);
        let mut body = Vec::with_capacity(HEADER.len() + tag_count * (payload_bytes + 15));
        body.extend_from_slice(HEADER);
        for index in 0..tag_count {
            let tag = FlvTag::new(9, (index as u32).wrapping_mul(10), &payload).unwrap();
            body.extend_from_slice(tag.as_bytes());
        }
        let body_len = body.len();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/throughput", listener.local_addr().unwrap());
        let upstream = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream.write_all(&body).unwrap();
            let _ = stream.shutdown(Shutdown::Write);
            thread::sleep(Duration::from_millis(300));
        });
        let mut cfg = config();
        cfg.upstream_timeout = Duration::from_secs(10);
        cfg.write_timeout = Duration::from_secs(10);
        cfg.max_reconnects = 0;
        let (address, relay, _calls) = start(cfg, url);
        let started = Instant::now();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(
            stream,
            "GET /live.flv?t=synthetic-session HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        let mut received = 0_usize;
        let mut first_byte = Duration::ZERO;
        let mut buffer = vec![0_u8; 128 * 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if received == 0 {
                        first_byte = started.elapsed();
                    }
                    received += count;
                }
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(error) => panic!("player read failed: {error}"),
            }
        }
        let elapsed = started.elapsed();
        assert_eq!(received, body_len, "relay must forward every byte");
        let seconds = elapsed.as_secs_f64();
        println!(
            "{}",
            serde_json::json!({
                "metric": "relay_forwarding",
                "workload": label,
                "tags": tag_count,
                "bytes": body_len,
                "elapsed_ms": (seconds * 1000.0 * 1000.0).round() / 1000.0,
                "tags_per_s": (tag_count as f64 / seconds).round(),
                "mb_per_s": ((body_len as f64 / (1024.0 * 1024.0)) / seconds * 1000.0).round() / 1000.0,
                "first_byte_ms": (first_byte.as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0,
            })
        );
        upstream.join().unwrap();
        relay.join().unwrap().unwrap();
    }
}

/// Bounded FLV framing throughput on synthetic tags. read_tag is measured in
/// every build so the number stays comparable across the allocation change.
#[test]
#[ignore = "performance measurement; run scripts/Bench-Perf.ps1"]
fn perf_flv_tag_parse_throughput() {
    const TAG_COUNT: usize = 60_000;
    const PAYLOAD_BYTES: usize = 1024;
    let mut payload = vec![0x27_u8, 1, 7];
    payload.resize(PAYLOAD_BYTES, 0);
    let mut bytes = Vec::with_capacity(HEADER.len() + TAG_COUNT * (PAYLOAD_BYTES + 15));
    bytes.extend_from_slice(HEADER);
    for index in 0..TAG_COUNT {
        bytes.extend_from_slice(FlvTag::new(9, index as u32, &payload).unwrap().as_bytes());
    }
    let framed = bytes.len() - HEADER.len();
    let mut samples = Vec::new();
    for _ in 0..4 {
        let started = Instant::now();
        let mut cursor = std::io::Cursor::new(&bytes);
        flv::read_header(&mut cursor).unwrap();
        let mut count = 0_usize;
        while flv::read_tag(&mut cursor).unwrap().is_some() {
            count += 1;
        }
        let elapsed = started.elapsed();
        assert_eq!(count, TAG_COUNT);
        samples.push(elapsed);
    }
    let elapsed = median_of(&mut samples);
    report_flv_parse("read_tag", TAG_COUNT, framed, elapsed);

    // The relay uses the buffer-reusing reader; measure it separately so the
    // allocation change is visible next to the unchanged read_tag number.
    let mut buffer = Vec::new();
    let mut reuse_samples = Vec::new();
    for _ in 0..4 {
        let started = Instant::now();
        let mut cursor = std::io::Cursor::new(&bytes);
        flv::read_header(&mut cursor).unwrap();
        let mut count = 0_usize;
        while let Some(tag) = flv::read_tag_reusing(&mut cursor, &mut buffer).unwrap() {
            count += 1;
            if tag.as_bytes().len() <= 1024 * 1024 {
                buffer = tag.into_bytes();
            }
        }
        let elapsed = started.elapsed();
        assert_eq!(count, TAG_COUNT);
        reuse_samples.push(elapsed);
    }
    let elapsed = median_of(&mut reuse_samples);
    report_flv_parse("read_tag_reusing", TAG_COUNT, framed, elapsed);
}

/// Offline four-platform target parsing. Rust exposes a parse CLI; the C#
/// reference program has no offline parse CLI, so this row is Rust-only.
#[test]
#[ignore = "performance measurement; run scripts/Bench-Perf.ps1"]
fn perf_target_parse_throughput() {
    use live_hub_rust::core::Target;
    let inputs = [
        "https://live.bilibili.com/7734200",
        "huya:333003",
        "douyu:9999",
        "douyin:123456",
    ];
    for input in inputs {
        Target::parse(input).unwrap();
    }
    const ROUNDS: usize = 5;
    const ITERATIONS: usize = 20_000;
    let mut samples = Vec::new();
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            for input in inputs {
                let target = Target::parse(input).unwrap();
                assert!(!target.room.is_empty());
            }
        }
        samples.push(started.elapsed());
    }
    let elapsed = median_of(&mut samples);
    let seconds = elapsed.as_secs_f64();
    let parses = (ITERATIONS * inputs.len()) as f64;
    println!(
        "{}",
        serde_json::json!({
            "metric": "target_parse",
            "workload": "four-platform",
            "iterations": ITERATIONS,
            "parses": parses as u64,
            "elapsed_ms": (seconds * 1000.0 * 1000.0).round() / 1000.0,
            "parses_per_s": (parses / seconds).round(),
            "us_per_parse": (seconds * 1_000_000.0 / parses * 1000.0).round() / 1000.0,
        })
    );
}

fn report_flv_parse(parser: &str, tags: usize, framed: usize, elapsed: Duration) {
    let seconds = elapsed.as_secs_f64();
    println!(
        "{}",
        serde_json::json!({
            "metric": "flv_tag_parse",
            "parser": parser,
            "tags": tags,
            "bytes": framed,
            "elapsed_ms": (seconds * 1000.0 * 1000.0).round() / 1000.0,
            "tags_per_s": (tags as f64 / seconds).round(),
            "mb_per_s": ((framed as f64 / (1024.0 * 1024.0)) / seconds * 1000.0).round() / 1000.0,
        })
    );
}

/// Bilibili danmaku frame decoding: zlib-compressed batch (real wire form).
#[test]
#[ignore = "performance measurement; run scripts/Bench-Perf.ps1"]
fn perf_danmaku_decode_throughput() {
    use flate2::{Compression, write::ZlibEncoder};
    use live_hub_rust::danmaku::{bili_packet, decode_bili};
    use std::io::Write as _;

    let mut encoded = Vec::new();
    let mut writer = ZlibEncoder::new(&mut encoded, Compression::fast());
    for index in 0..20_u32 {
        let body = serde_json::to_vec(&serde_json::json!({
            "cmd": "DANMU_MSG:4:0:2:2:2:0",
            "info": [[0, 1, 25, 16711680, 0, index], "合成弹幕消息内容", [7, "Synthetic User"]],
        }))
        .unwrap();
        writer
            .write_all(&bili_packet(5, 0, &body).unwrap())
            .unwrap();
    }
    writer.finish().unwrap();
    let batch = bili_packet(5, 2, &encoded).unwrap();
    assert_eq!(decode_bili(&batch).unwrap().len(), 20);

    const ROUNDS: usize = 5;
    const ITERATIONS: usize = 4_000;
    for _ in 0..2 {
        let _ = decode_bili(&batch).unwrap();
    }
    let mut samples = Vec::new();
    for _ in 0..ROUNDS {
        let started = Instant::now();
        let mut messages = 0_usize;
        for _ in 0..ITERATIONS {
            messages += decode_bili(&batch).unwrap().len();
        }
        let elapsed = started.elapsed();
        assert_eq!(messages, ITERATIONS * 20);
        samples.push(elapsed);
    }
    let elapsed = median_of(&mut samples);
    let seconds = elapsed.as_secs_f64();
    println!(
        "{}",
        serde_json::json!({
            "metric": "danmaku_decode",
            "workload": "bili-zlib-batch-20",
            "iterations": ITERATIONS,
            "messages": ITERATIONS * 20,
            "elapsed_ms": (seconds * 1000.0 * 1000.0).round() / 1000.0,
            "messages_per_s": ((ITERATIONS * 20) as f64 / seconds).round(),
            "frames_per_s": (ITERATIONS as f64 / seconds).round(),
        })
    );
}
