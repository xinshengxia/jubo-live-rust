# Playback sessions and cancellation

The workbench owns an in-memory `SessionManager`. It starts the same native
playback path as the CLI, tracks each launch by a random UUID and reports its
startup phases, process identity, room observation and terminal outcome.

```rust,ignore
let sessions = SessionManager::new();
let updates = sessions.subscribe();
let id = sessions.start(target, player, settings, Some(vault))?;
// GUI frames drain `updates` and/or read `sessions.snapshots()`.
// Exactly one user-selected row is addressed by each action:
sessions.cancel_start(&id)?; // only while starting/waiting for live
sessions.stop(&id)?;         // only while playing/interrupted
```

## Startup and ownership

The native launcher reports checking the player, resolving the stream, waiting
for live, waiting for the relay's READY acknowledgement, starting the player,
registering the player's process handle, and playing. Only after registration
succeeds does startup atomically transition to Running. A cancellation wins or
loses that boundary; it cannot accidentally become a stop of a running session.

Blocking platform work runs on bounded worker threads. Cancelling the launch
returns promptly while the underlying network request finishes its existing
timeout. A shared budget of eight requests bounds those remaining workers.
Their late result cannot start a player. READY and registration waits are also
cancellable; the owned player/relay are terminated and reaped on failure.

The session manager retains at most 16 active rows and 128 rows overall. Clearing
finished rows never stops or removes a live row. A slow UI subscriber does not
block playback: notifications have a bounded queue and snapshots are authoritative.

## After startup

The supervisor owns its exact `Child` handles; stopping a row never searches all
system PIDs or closes another application instance. Player exit and unexpected
relay exit are reported separately. A relay failure leaves the player window
available and marks its row Interrupted, rather than silently showing Playing.

Closing the launcher does not terminate a running session. The relay's own
registered process handle continues player-lifetime supervision independently.
No persistent session file is imported from another application.

## Room states and reopen

Initial stream resolution enters automatic waiting only after a room observation
explicitly confirms Offline and `settings.playback.wait_for_live` is enabled.
Replay and Unknown are distinct; a verification failure or network problem is
never presented as a confirmed offline room. Waiting is cancellable.

With `monitor_room_state`, a session checks its platform approximately every 20
seconds on its worker thread. Status errors become Unknown while the existing
playback continues. A late observation cannot revive a completed row. Playback
process state and room state are separate fields: Offline is not process exit.
Keeping a player's stream open across an offline/reopen period is the relay
resolver's responsibility; this module does not pretend that a UI label alone
implements media reconnection or replay support.

## Validation

The default contracts use injected launch backends, temporary values and synthetic
child processes. They do not open real player windows, read user credentials or
contact platforms. Contracts cover parallel sessions, UUID addressing, cancel and
stop separation, late results, interrupted relays, room-state distinction, bounded
rows, notifications and cleanup. Playback unit tests also exercise actual
synthetic READY-wait cancellation and owned-child cleanup.

Real player end-to-end playback, account login and long-duration streams require
the separately documented acceptance procedure; passing these contracts alone
does not establish those outcomes.
