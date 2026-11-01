# Architecture

- `core.rs`: validated domain values and transactional, explicitly located storage.
- `platforms.rs`, `search.rs`, `room_state.rs`: platform adapters, staged identity lookup and room observations.
- `auth.rs`, `login.rs`, `login/browser.rs`: platform-scoped credentials, DPAPI storage and opt-in login flows.
- `relay.rs`, `flv.rs`: loopback-only authenticated FLV relay, bounded framing and reconnect handling.
- `playback.rs`, `sessions.rs`: external-player argument plans, child-process ownership and independent sessions.
- `danmaku*.rs`, `chat_pipeline.rs`, `overlay.rs`: bounded protocol decoding, admission policy and transparent presentation.
- `avatars.rs`, `images.rs`: approved public image transport, bounded decoding and cache recovery.
- `gui/`: launcher views, explicit user actions and background worker events.

Default tests use synthetic fixtures and temporary state. Live accounts and real player windows require explicit manual acceptance. Unicode emoji atlas provenance and regeneration are recorded in `assets/emoji-source.json`.
