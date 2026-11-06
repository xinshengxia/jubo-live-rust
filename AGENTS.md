# Project development rules

- Keep original user data, sessions, browser profiles, and the legacy application intact. Use a separate data directory by default. Import only through an explicit user operation.
- Do not include real credentials, personal paths, host identifiers, private logs, or user data in source or release archives. Audit actual files and pending history before external publication.
- Preserve applicable third-party licenses and attribution for every ported algorithm, protocol implementation, library, and asset. Record upstream sources and local usage in THIRD_PARTY_NOTICES.md and licenses/SOURCES.json. Do not change the project license without authorization.
- Keep domain logic, platform adapters, relay, player integration, storage, and UI behind explicit module boundaries. Do not silently route Rust functionality to a legacy implementation.
- Report implemented, partially implemented, and unimplemented features honestly. An offline test does not establish successful live-platform playback or login.
- Tests must use synthetic fixtures and isolated temporary data. Do not open windows, start real players, read personal credentials, or contact live platforms in the default test suite.
- Do not modify existing shortcuts or install over the legacy application as part of development builds.
