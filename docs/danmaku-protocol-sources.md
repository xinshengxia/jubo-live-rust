# Danmaku implementation scope and references

The Rust implementation reads protocol descriptions and the legacy application's
wire-format observations as references. It does not load the legacy executable,
configuration, credentials, assets, or user data.

- Bilibili packet header, zlib packet nesting, `DANMU_MSG` and super-chat field
  meanings: protocol/model reference xfgryujk/blivedm, revision
  `3bf17fe862b8c2fc6bc04c81c3f0c5db95de93d6`, MIT, Copyright (c) 2018 xfgryujk.
  Preserve `licenses/blivedm-LICENSE.txt`. No Python implementation or runtime is
  bundled. WBI/HTTP discovery belongs to the platform adapter and its notices.
- Douyu framing/STT escaping: a new implementation of the wire layout and field
  observations documented by the legacy application. No third-party color or
  super-chat palette tables were copied. Chat currently uses white text.
- Douyin protobuf field layout and the pure `frontierSign`/`X-Bogus` signature
  algorithm: see `danmaku-douyin.md` and its licensing notes. The receiver keeps
  bounded packet/signature code under offline synthetic tests; live signing,
  ACK/heartbeat acceptance and login state are not validated by the offline
  suite. Anonymous real-link acceptance (2026-10-29) is recorded in
  `docs/danmaku-douyin.md`.
- Huya Tars/JCE: the anonymous WebSocket client, Tars subset codec, register and
  heartbeat frames and chat decoding are implemented from the legacy application's
  recorded wire behaviour and from public protocol research references
  (hwenjie/huya_danmu, BacooTang/huya-danmu, biliup-start/VideoPlayer; protocol
  references only). See `danmaku-huya.md`. No third-party code was copied, so no
  extra licence text is required; the acknowledgements live in
  THIRD_PARTY_NOTICES.md. Tars is a Tencent format (Tencent/Tars, BSD-3-Clause);
  this project only implements the observed wire subset.
- Zlib/gzip decoding uses flate2. WebSocket transport uses tungstenite. Cargo.lock
  fixes dependency versions; their applicable licenses must accompany releases.

All four receiver modules have synthetic offline protocol tests (Bilibili, Douyu,
Douyin, Huya). This does not establish successful real-platform authentication or
message reception. The simple scheduler is independent of GUI frameworks. Explicit Bili emote images and Bili/Douyu gift/enter events are now modeled with bounded decoding and image loading. Full emote maps, other-platform rich events, advanced paths and all legacy settings remain incomplete.

