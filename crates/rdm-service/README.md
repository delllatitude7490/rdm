# RDM per-user service (preview)

This Rust process owns managed downloads independently of the Iced window.
Closing or crashing the GUI only disconnects a client. It does not cancel work.
The service has no webview and exposes no HTTP listener.

## Build / run

Use current stable Rust and the root README's native build prerequisites.

```sh
cargo build -p rdm-service -p rdm-gui --release
./target/release/rdm-gui
```

The GUI connects to an existing service or starts `rdm-service` on demand. It
looks beside its own executable first, then on PATH. Install/build both binaries
from the same checkout and profile; `cargo run -p rdm-gui` alone does not build
the service executable. A manually started service is also supported:

```sh
./target/release/rdm-service
# From another terminal:
./target/release/rdm-service --status
./target/release/rdm-service --shutdown
```

No login autostart, systemd unit, tray, or installer is added. A service remains
running until explicitly shut down (or terminated). Its runtime may have worker
threads, but it has no progress polling timer while no transfer is active.
There are no RAM/CPU guarantees without target-machine benchmarks.

## Runtime ownership

Linux only. `XDG_RUNTIME_DIR` must be an absolute, user-owned directory with no
group/other permission bits, as normally provided by a Linux desktop session.
There is deliberately no insecure automatic `/tmp` fallback. Tests and manual
headless sessions can set it to a freshly created private directory.

- `$XDG_RUNTIME_DIR/rdm/` is mode 0700; `service.sock` is mode 0600.
- Client and server also check the peer UID. The boundary is the OS user, not
  a sandbox against other processes already running as that same user.
- An OS-held `flock` admits one service instance. The lock inode is never unlinked.
- A stale socket is removed only after acquiring the lifetime lock. Unexpected
  files and symlinks are not removed. Normal exit removes only the bound socket
  inode belonging to that instance.
- Duplicate startup cannot replace an active service or create a second worker.

## Commands and events

The `rdm-ipc` crate defines Add, AddCli, List, Subscribe, Pause, Cancel, Remove, Retry
and Shutdown. Traffic is length-prefixed JSON with internal protocol version 6
and a 1 MiB frame limit.
Add input has separate URL/destination length limits. Pause/Cancel/Remove/Retry/Shutdown include
the service instance ID so stale clients cannot target reused job IDs.

Each subscription starts with a complete snapshot, then receives coalesced
snapshots at most four times a second during active transfer plus immediate
command/terminal changes. There is no unbounded per-chunk event queue. Each
snapshot has an instance ID and monotonic revision. Slow clients are disconnected
and can reconnect to the current complete state. At most 32 IPC handlers are
admitted, and first requests and writes have deadlines.

Mutations are never automatically retried. If acknowledgement is lost, the GUI
reports an uncertain outcome: reconnect and inspect before retrying. Following
an established connection's loss, GUI retries are read-only. Explicitly choose
**Reconnect / start service** to start a stopped service again; an intentional
shutdown must not trigger an automatic restart loop.

**Stop service & cancel downloads** and `--shutdown` request graceful shutdown:
reject new Adds, cancel active work, and wait for the engine to release files
before exiting. The CLI reports acceptance, not proof that all cleanup already
finished. SIGINT/SIGTERM request the same drain. An outstanding engine network
operation can delay it; there is no unsafe forced-abort button.

## Deliberate limits

- One running **managed** worker at a time, shared by CLI, GUI and Firefox.
  Further validated Adds are held privately in FIFO order; queued work performs
  no network/filesystem operations until promoted. The actor starts the next
  worker only after joining the previous one, including cancellation cleanup.
  This prevents simultaneous managed writes even through aliases or server names.
- At most 64 total records, persisted in SQLite; oldest terminal records are evicted
  except held Paused/Interrupted work, which is never silently evicted.
  Unfinished records are never evicted. At 64 unfinished/held jobs, new Adds get Busy
  with queue-full feedback. Cancel makes queued work evictable retryable history;
  Remove immediately forgets the record and frees its slot.
- Cancel targets queued or running/held jobs and retains retry input. Remove forgets
  queued or any terminal job and its private retry input, committing before ACK
  or publication. It never opens/deletes output, partial or checkpoint files.
  Running/Pausing/Cancelling jobs are rejected, including promotion races: Cancel
  and wait for cleanup before removing them. Both commands are
  instance-qualified and never automatically retried. Shutdown cancels all
  queued work, retains private retry input, and drains the running worker.
  Reopening the GUI reconnects to these records, including after restart.
- A service restart creates a new instance but restores records and ID allocation.
  Every unfinished record is held as Interrupted, except durable pause intent
  recovers as Paused. Existing Paused records stay held, without HEAD/GET/output work.
  Explicit Retry revalidates current policy and creates a new ID at the FIFO
  tail, including at capacity; duplicate retry while active is rejected. Other
  held work remains idle. Remove forgets held Paused/Interrupted records; Cancel
  instead retains Cancelled retryable history. Removed IDs are never recycled;
  old handles cannot recreate jobs or affect later attempts. Attached waiters see
  absence as a nonzero, completion-unconfirmed outcome, not success. A lost remove
  reply is ambiguous: inspect the list before doing anything else, never resend
  automatically. Current instance-qualified handles remain mandatory.
  Existing engine partial/resume files are authoritative, never stored offsets.
  Resume/retry is not forced overwrite or a promise of byte-range continuation.
- Direct HTTP/HTTPS CLI files now use this service through AddCli, with absolute
  caller-resolved output, connection count and explicit private-network opt-in.
  CLI, GUI and Firefox share the one-active limit and records. Provider/SFTP,
  listings, sync and the persisted queue remain legacy paths outside coordination;
  do not use them concurrently against the same output. See the root README for
  CLI wait/detach, cancellation, existing-file semantics and upgrade instructions.
- Only direct HTTP/HTTPS files. Known provider shares, directory-shaped URLs,
  SFTP and userinfo credentials are still refused. Firefox explicit link handoff
  is available through the separate `rdm-native-host` preview; see
  [Firefox setup](../../extensions/firefox/README.md).
- No parallel managed downloads or automatic network replay. Pause/Resume applies
  only to this shared direct-file service, not provider/SFTP or legacy queue work.
  The existing persisted CLI queue remains separate; no automatic import or
  migration of provider/listing/sync workflows is performed.
- The service loads existing config once at startup, retaining parse errors and
  existing network/file-safety rules. Restart it to apply config/environment
  changes (CLI output/connections/private opt-in are explicit per-job exceptions).
  For example `RDM_ALLOW_PRIVATE=1` must be set on the **service** process,
  not just a GUI connecting to an already running service.
- No raw error chains, notice bodies, signed URLs or credentials in public
  snapshots or service logs. Existing engine resume files retain their existing
  protection model. Pending and retryable jobs retain URLs in private local SQLite (not encryption).

## Durable state

Storage: `$XDG_STATE_HOME/rdm/service/jobs.sqlite3`, default
`~/.local/state/rdm/service/jobs.sqlite3`. Private service/rdm directories (0700),
DB and lock (0600); symlinks, hardlinked/shared files and unsafe paths are refused.
A second lifetime lock in the durable directory prevents services using different
runtime directories from opening the same state. No legacy queue import occurs.

SQLite is bundled into the service only, with schema version 2 (validated schema-1 images migrate in place), DELETE journal
and synchronous FULL transactions. A bounded complete image is committed before
ACK, promotion/engine creation, cancellation publication or terminal publication.
SQLite/fsync work uses Tokio's blocking pool, not reactor workers. Progress is
coalesced in memory and saved on state transitions, not every tick. Recovery
never trusts a saved offset over the engine's resume validation.

Corrupt/unknown schemas or unsafe files cause startup failure without reset.
Mutation commit errors fail closed: no success ACK or further scheduling,
existing workers are cancelled/drained, and the last committed image is left for
conservative recovery. A committed Add with a lost reply remains ambiguous; do
not resubmit without inspection. A file may finish just before a crash without
its result being committed; explicit retry then protects existing output and
reports AlreadyPresent rather than inventing completion.

URLs (including signatures) are stored in plaintext locally for pending and
retryable jobs, never public snapshots/logs. Success drops URL input; bounded
terminal eviction drops old inputs. Do not share state files/backups. Permissions
are an OS-user boundary, not encryption or protection from same-user processes.
Retryable history remains bounded, not an indefinite archive of every attempt.
Stop the previous service using its matching binary BEFORE upgrading; rebuild
all installed Rust clients/service/native host together (IPC v6), and reload
Firefox extension 0.3.0. The extension's external protocol stays v1.

## Validation

```sh
cargo fmt -p rdm-ipc -p rdm-service -p rdm-gui --check
cargo check -p rdm-ipc -p rdm-service -p rdm-gui --all-targets --locked
cargo clippy -p rdm-ipc -p rdm-service -p rdm-gui --all-targets --locked -- -D warnings
cargo test -p rdm-ipc -p rdm-service -p rdm-gui --locked
cargo test -p rdm --all-targets --locked
```

Process-level tests launch real service executables, use real Unix sockets, and
serve local HTTP payloads. They exercise disconnect/reconnect, completed bytes,
cancel/shutdown drain, duplicate ownership, stale socket/crash recovery,
generation mismatch, permissions, bad frames/versions, bounded subscriptions,
existing output protection, SIGTERM drain, 64-record eviction, and secret-safe validation. Test-only loopback access
is set per child process, never through global environment mutation.

CLI process-level coverage (matching binaries required):

```sh
cargo build -p rdm -p rdm-service -p rdm-native-host --locked
python3 tests/cli_service_smoke.py target/debug
python3 tests/cli_protocol_smoke.py target/debug
python3 tests/service_queue_smoke.py target/debug
python3 tests/service_persistence_smoke.py target/debug
python3 tests/service_pause_smoke.py target/debug
python3 tests/service_remove_smoke.py target/debug
```

Crash-resume regression: segmented range counters previously included data still
in BufWriter. Range checkpoints now publish only after flush + sync_data, batched
at most twice per second plus terminal cleanup; failed flush/sync cannot advance
them. On range failure/cancellation, sibling ranges and progress/autosave helpers
are cancelled/drained before returning, and a final validated checkpoint is saved.
This prevents an old autosaver racing a retry. It prevents new bad checkpoints;
it does not repair partial files already corrupted by older versions.

## Durable Pause / Resume

Pause persists intent before signalling the existing engine token; it does not
freeze tasks in memory. Running work remains Pausing/running until the worker and
all checkpoint helpers are joined, then becomes Paused, and only then the next
FIFO entry starts. Queued work becomes Paused without starting an engine. No idle
hold heartbeat/timer is introduced. Held pauses are protected at the 64-job bound.
The pause intent survives a crash during drain. Successful completion or a genuine
failure racing pause remains its real result; only a drained Cancelled engine
outcome becomes Paused. Explicit Cancel overrides Pausing; Shutdown preserves it.

Resume uses Retry's existing distinct-attempt transaction and current policy
revalidation. The original paused record loses input/retryability and links to the
fresh tail attempt; old waiters/cancel handles stay safe. Range-capable servers
reuse valid engine partials, no-range servers safely restart, final output is
protected. A CLI waiter exits nonzero on Paused, not success, and never follows a
fresh attempt automatically. Paused input has the same plaintext-URL privacy as
other retryable records. Schema 2 gates older readers; downgrade is unsupported.

`tests/service_pause_smoke.py` exercises actual CLI pause/resume and local HTTP,
FIFO advancement, no queued/held network work, drained metadata tasks, crash
recovery, exact bytes with Range and without ranges, and existing-output safety.

Pause cancellation uses a typed engine marker, not merely a set cancellation
token: failed flush/sync or final metadata writes remain Failed. Sibling range
errors encountered while draining also take priority over cancellation. Streaming
partials flush and sync before confirming intentional cancellation.

Non-overwrite `Reuse` now protects an existing final path even beside a retained
partial/manifest. The same protection is carried through ranged, streaming and
empty-file publication so a file arriving during HEAD/inspection cannot become
implicit overwrite consent. Explicit legacy interactive/overwrite policies are
otherwise unchanged; valid partials without a final file still resume normally.

## True Remove (IPC 6)

The old RemoveQueued operation incorrectly kept retryable Cancelled history.
Remove now deletes the record from the live SQLite image and all snapshots. This
also lets users dismiss old Cancelled records produced by the previous behavior.
SQLite stays schema 2; no state reset/migration is required. Rebuild matching CLI,
GUI, service and native host and stop the old service before upgrading. Firefox's
external v1 protocol/permissions are unchanged. No promise of secure erasure of
backups, filesystem snapshots or storage-device history is made.

`tests/service_remove_smoke.py` checks real CLI/Unix-socket removal, attached
waiter failure (not false success), held and terminal kinds, exact unchanged
partial/checkpoint/final bytes, URL-input purge from the live database, restart
without resurrection/replay and monotonically allocated IDs. Actor/lifecycle
coverage also injects failed removal commits and rejects running/draining races.
