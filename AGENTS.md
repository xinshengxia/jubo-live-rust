# Contributing

- Preserve applicable licenses, attribution and upstream source records.
- Never commit credentials, personal configuration, profiles, history, private logs or machine-specific paths.
- Tests use synthetic fixtures and isolated temporary directories; real accounts, players and live-platform tests require an explicit opt-in.
- Preserve existing user data during upgrades. Do not silently import another application's accounts or history.
- Keep UI, domain logic, platform adapters, relay, player integration and storage behind explicit module boundaries.
- Report offline verification and real-platform acceptance separately.
