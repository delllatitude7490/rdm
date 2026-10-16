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

The `rdm-ipc` crate defines Add, List, Subscribe, Cancel and Shutdown. Traffic is
length-prefixed JSON with an explicit protocol version and a 1 MiB frame limit.
Add input has separate URL/destination length limits. Cancel/Shutdown include
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

- One active **managed** download at a time, shared by all GUI/IPC clients.
  Other Adds return Busy, not a hidden queue. This also prevents simultaneous
  managed writes when directory aliases or server-selected filenames coincide.
- At most 64 recent records, in memory only; oldest terminal records are evicted.
  Reopening the GUI reconnects to these records while the same service lives.
- A service restart creates a new instance and an empty record list. It does not
  invent restored history or automatically restart downloads. Existing engine
  partial/resume files remain on disk; re-add the same URL and destination to use
  the engine's supported resume behaviour.
- The legacy CLI and its persisted queue remain unchanged and do NOT yet use
  this service. Do not run a legacy CLI download against the same destination
  concurrently. Cross-interface coordination is a separate follow-up.
- Only direct HTTP/HTTPS files. Known provider shares, directory-shaped URLs,
  SFTP and userinfo credentials are still refused. Firefox explicit link handoff
  is available through the separate `rdm-native-host` preview; see
  [Firefox setup](../../extensions/firefox/README.md).
- No per-file pause, multi-download scheduler, durable history or queue controls.
- The service loads existing config once at startup, retaining parse errors and
  existing network/file-safety rules. Restart it to apply config/environment
  changes. For example `RDM_ALLOW_PRIVATE=1` must be set on the **service** process,
  not just a GUI connecting to an already running service.
- No raw error chains, notice bodies, signed URLs or credentials in public
  snapshots or service logs. Existing engine resume files retain their existing
  protection model. The service adds no separate URL/history persistence.

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
