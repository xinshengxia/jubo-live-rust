# Relay and playback lifecycle

This document describes the relay/playback contract owned by `src/relay.rs`,
`src/flv.rs`, `src/playback.rs`, `tests/relay_contract.rs` and
`tests/playback_contract.rs`. Everything here is verified offline; no test opens
a real player window, contacts a live platform or reads real user data.

## Relay (`relay::serve`)

- Binds a loopback address only, with a zero port by default.
- Writes exactly one stdout protocol line, `READY <actual_port>`, after binding
  and flushes it before any upstream resolution.
- Requires the URL-safe session token (`/live.flv?t=<token>`), compared in
  constant time. Wrong/missing/duplicate tokens get `403` before any upstream
  work; malformed requests get `400`; excess connections get `503`.
- Connection budget: `max_connections` in `1..=8` (default 8). The budget is
  checked after accepting and before spawning a worker, so a ninth socket is
  rejected immediately with `503 Service Unavailable` even while eight idle
  sockets are still waiting for their request line.
- Idle sockets are bounded by `request_timeout`; a standalone, unbound relay
  exits after `idle_timeout` with no live connections. A relay registered to a
  player instead remains available until that exact player process ends.
- Accepted sockets are forced to blocking mode. The listener is non-blocking so
  the accept loop can enforce the idle deadline, and Windows inherits that mode
  into accepted sockets (unlike Unix accept). Before this reset, an idle player
  socket failed its first read instantly, the connection budget was released
  early and the ninth socket was served instead of rejected.
- The relay forwards complete FLV tags only; a truncated upstream tag is dropped
  with that upstream response, never spliced into the next one.
  `flv::ReconnectTimeline` rebases upstream clocks that restart at zero and
  drops only the overlap seen after a reconnect (per audio/video track).
- In-response timeline discontinuities are rebased too. A live CDN can drop its
  own queued data and resume at the live edge inside one HTTP response (measured:
  +114.5 s audio+video jump with `upstream_attempts=1`, `reconnects=0`);
  forwarding that raw jump makes a player such as PotPlayer treat the stream as
  restarted and rebuild its demuxer cache. `ReconnectTimeline` maps the jump onto
  the already forwarded clock (+20 ms, shared by both tracks) so the skipped
  content stays lost but the clock does not break. Each rebase is logged as a
  relay WARN and counted as `timeline_jumps` in the status snapshot.
- Recovery uses a **consecutive unproductive attempt** budget (default 500),
  rather than a cumulative reconnect count over the viewing session. A segment
  clears earlier failures only after forwarding at least 24 non-codec media
  tags, 8 KiB of media payload and 1 s of timestamp advancement on an audio or
  video track. A 200 response, FLV/script/codec headers or static/tiny media do
  not count as recovery. Every reconnect still waits `retry_delay` (at least
  5 ms even when a fixture requests zero); the production default is 1 s for a
  failed attempt. A segment that ended after forwarding stable media is a normal
  CDN boundary instead of a failure, so it only waits
  `min(retry_delay, 150 ms)` before reconnecting; duplicate overlap is trimmed by
  the reconnect timeline either way.
  `max_reconnects=0` explicitly preserves one-attempt/no-reconnect behavior.
- Player writes use a 15 s write timeout (same budget as the legacy launcher).
  A live player may legitimately stop reading for seconds while it rebuilds its
  own cache; the previous 5 s timeout closed the connection in that case, which
  forced a fresh upstream request and a visible re-buffer.
- A resolver can return `ResolveWaiting { state, retry_after }` for a positively
  identified `offline`, `replay_unavailable` or `verification` state. The same
  player HTTP connection remains open while polling (each wait clamped to 1-60 s),
  without consuming or resetting the failure budget. A `verification` wait is
  returned after one bounded silent-refresh attempt failed, so a Douyin captcha
  cannot turn a long relay session into a room-page request storm; the shared
  refresh gate also applies its own single-flight/cooldown. A network/unknown
  error is not silently classified as offline. The player disconnect is checked
  during waits.
- Production media redirects are followed for at most five hops. Every target
  must independently pass the syntax/literal-address policy, the bounded DNS
  lookup, the public/TUN-address filter and pinned address selection with normal
  TLS hostname verification. The initial address keeps its platform protocol
  (http is no longer upgraded, because not every HTTP CDN endpoint has an
  equivalent HTTPS service, matching the legacy launcher); an https→http
  downgrade on a redirect is rejected, while http→https and same-scheme hops
  are allowed. Credentials, local names, private literals and redirect loops are
  rejected. A host outside the documented CDN families is accepted only when its
  DNS answers are public/TUN addresses; fixed family membership is a fast path,
  not the boundary. Loopback fixture mode never follows a redirect, preserving
  the offline safety boundary. Only User-Agent/Referer/Origin can be forwarded;
  Cookie, Authorization and arbitrary API-key headers remain rejected before any
  upstream connection. Across a host change Origin is dropped and Referer is
  kept only for the initial host or a documented CDN family.
- `GET /__livehub/status?t=<token>` returns `RelayStatistics` as a bounded JSON
  snapshot. `query_statistics(relay_url)` is the loopback-only client helper.
  It includes attempts, reconnects, stable segments, actual media tags/bytes,
  failed attempts, budget exhaustions, in-response timeline jumps
  (`timeline_jumps`) and at most eight active/eight recent connection records
  with controlled state/error values. It never includes
  media URLs, request headers, room IDs or the session token. Status polling
  shares the connection budget and can return 503 when all eight slots are busy.

## Playback (`playback::launch`)

`launch` = find player executable -> resolve media stream (`platforms::resolve`)
-> (FLV by default, including Bili; explicit Bili direct/HLS excluded) start relay child -> wait for `READY` -> spawn player.

- `LaunchOutcome` keeps exactly `title` and `player_pid`; the GUI depends on it.
- The relay child's stdout is parsed for up to 10 seconds. Distinct actionable
  errors are returned for: spawn failure, stdout EOF before READY, invalid
  READY output, READY timeout, and a relay that exits immediately after READY.
  Every startup failure kills and reaps the relay child before returning, and
  the player is never spawned into a dead relay URL.
- On player spawn/plan failure the relay child is killed and reaped before the
  error is returned.
- After spawning the player, the launcher registers its PID through a session-
  token-authenticated POST to the relay's existing loopback server. The relay
  opens a Windows `SYNCHRONIZE` process handle before acknowledging registration.
  This handle identifies the exact player process even after its numeric PID
  could be recycled. Launch succeeds only after the acknowledgement.
- The relay checks its own player handle in its accept loop and exits promptly
  when the player ends. This remains active after the GUI closes or the one-shot
  `play` CLI exits. The relay never gains termination or input-focus rights to the
  player. An owned relay that receives no registration exits after 15 seconds.
- While the launcher remains alive, its detached supervisor still owns both child
  handles and reaps exited children. It is an additional cleanup path, rather than
  the owner of the relay's independent lifetime contract.
- If the supervisor thread itself cannot be created, both children are killed
  and the launch reports an error instead of leaking them.
- If registration fails, both the spawned player and relay are terminated and
  reaped before returning an error. Standalone `relay` commands without the
  internal `--watch-player` flag retain their ordinary idle-timeout behavior.

## Known limitations / TODO

- Post-launch relay failure cannot be surfaced through `LaunchOutcome` (the GUI
  contract has no status channel); the supervisor only reaps the children. A
  user-visible "relay died while playing" indicator would need an API change.
- The immediate `READY` check is best effort, followed by the acknowledged
  registration round trip. Failure before that acknowledgement fails launch and
  cleans up both children. A relay failure after a successful registration still
  needs a separate user-visible status channel.
- mpv title/relay OSD: the original launcher's Lua helpers
  (`assets/lua/live-title.lua`, `live-resync.lua`) are loaded when the packaged
  assets exist next to the executable; base/media title hints and relay state
  (ONLINE / RECONNECTING / OFFLINE / REPLAY-UNAVAILABLE) are written per session.
  Room-state hints for direct Bili playback, bitrate and anchor hints are not
  wired yet.
- Real mpv/PotPlayer launches and live platforms are outside this offline
  document; see docs/validation.md for the recorded production acceptance.
- Independent process-handle supervision is currently implemented for Windows;
  other systems reject registration explicitly instead of pretending to provide
  an equivalent PID-only watch.

## Verification

Offline targeted runs (isolated build directory to avoid cross-worktree cargo
artifact reuse):

    cargo test --locked --offline --no-default-features --test relay_contract
    cargo test --locked --offline --no-default-features --lib playback::tests
    cargo test --locked --offline --no-default-features --test playback_contract
    cargo test --locked --offline --no-default-features --test relay_lifecycle

Full suite, default features:

    pwsh -NoProfile -File scripts/Test.ps1 -ToolchainRoot '<isolated-rust-root>' -TargetDirectory '<private-target>'

The connection-budget tests are repeated ten times in the task report to prove
they are deterministic.

`relay_lifecycle` uses a real relay process plus synthetic launcher/player test
processes. It verifies that the launcher can exit first, the relay stays alive
while the synthetic player lives, and the relay then exits when that player
ends. It also checks unauthenticated/rebinding rejection and the registration
deadline without making an upstream request or opening a player window.

## Stress and throughput validation (round 4, 2026-10-29)

Long-duration and concurrent pressure testing found a real throughput defect that
offline contract fixtures never exposed:

- The relay used to probe player disconnect with a 1 ms blocking `peek` before
  every tag. Windows rounds socket timeouts up to the system timer granularity
  (about 15.6 ms), which capped a single player connection at roughly
  **15-16 tags/s**. Real FLV streams carry well over 50 tags/s (interleaved video
  and audio), so long sessions fell permanently behind. The probe now flips the
  socket to non-blocking mode (FIONBIO) for a zero-wait `peek`, reaching about
  **95-96 tags/s** in the same synthetic scenario.
- A connection-budget 503 could be preceded by a TCP reset because the socket was
  closed while unread request bytes remained. The rejection path now writes and
  flushes the status line, half-closes the write side, and drains arrived request
  bytes for a bounded 50 ms before closing.

Measured results (see the local task evidence for the full JSON/CSV):

- 25-minute synthetic soak: 8 concurrent players, upstream reconnect storm every
  5-10 s, 1594 upstream sessions, 141,507-142,511 tags per player at about
  95 tags/s, 390-410 rebased codec headers, zero parse/stall/timestamp errors;
  RSS 16.43 MB baseline -> 17.71 MB peak (+1.28 MB), handles constant at 181,
  threads 18-22; the synthetic launcher exited first while the relay kept
  serving, and the relay exited within 15 s of the player being killed.
- 8-minute real huya:660000 soak with 3 readers: 601 MB in 480 s, zero errors,
  RSS 19 -> 30 MB, handles 261 +/- 2, threads 9-13, about 31% of one CPU core.

Reproduction: `scripts/Stress-Relay.ps1` (`synthetic-smoke`, `synthetic-soak`,
`synthetic-all`, `real-soak`). The new `tests/relay_stress.rs` cases are all
`#[ignore]`; the default suite stays offline and fast. A fast regression for the
throughput fix lives in `tests/relay_contract.rs`.

The round-4 cumulative `max_reconnects` limit is resolved in round 6
(2026-10-30): the same default 500 is now a consecutive-unproductive-retry
budget, reset by validated stable media. `tests/reconnect_policy.rs` runs 503
productive upstream segments through the real pump with the production budget
and thresholds, exceeding 500 reconnects without exhausting the budget. Only
wall-clock retry delay is accelerated for this boundary fixture. Separate tests
prove consecutive resolver failures and header-only/static segments still stop,
recovered media clears earlier failures, known offline/replay waits preserve the
player, and unknown failures are not misclassified.

The stress harness now also keeps the production default budget rather than
raising it to 100000. Its synthetic audio/video payloads are large enough to
exercise the production stable-media threshold. Historical round-4 resource
figures above remain historical measurements, not new 24-hour validation.

Round-6 targeted offline checks include:

    cargo test --locked --offline --no-default-features --test reconnect_policy
    cargo test --locked --offline --no-default-features --test relay_contract
    cargo test --locked --offline --no-default-features --lib relay::tests

## PotPlayer command-line compatibility

PotPlayer parses its raw Windows command line. Quote an option value after the equals sign, such as /title=quoted-value, rather than quoting the entire option. Otherwise a title containing spaces can become part of the media URL. The launcher passes the escaped line directly to CreateProcess, never to a shell. A synthetic HTTP listener and real production playback established this regression and its fix; strict relay method/path/token checks remain unchanged.

