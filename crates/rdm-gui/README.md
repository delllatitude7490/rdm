# RDM native preview

A small Rust/Iced desktop frontend over the RDM service. No React, JavaScript,
webview, CLI-output parsing, or GUI-owned transfer workers.

## Build and run

Use current stable Rust plus the root README's native prerequisites. On
Debian/Ubuntu a useful starting point is:

```sh
sudo apt-get install build-essential cmake pkg-config libssl-dev \
    libxkbcommon-dev libxkbcommon-x11-0 libwayland-dev libx11-dev libxcursor1
cargo build -p rdm-service -p rdm-gui --release
./target/release/rdm-gui
```

Build/install **both** matching binaries. The GUI locates `rdm-service` beside
itself first, then on PATH. It starts the service on demand if none is reachable.
A valid private `XDG_RUNTIME_DIR` and an X11 or Wayland desktop are required.

Default rendering is tiny-skia software rendering. To compare GPU rendering:

```sh
cargo build -p rdm-service -p rdm-gui --release --features rdm-gui/gpu
./target/release/rdm-gui
```

Renderer choice is not a promise about CPU/RAM. Benchmark complete process-tree
RSS/PSS and idle/active CPU on the target machine. The existing CLI remains the
workspace default; normal CLI-only builds do not build Iced or the service.

## Current behaviour

- Direct HTTP/HTTPS URL, destination-folder text field, optional filename.
- One running managed download across all clients, not one per window.
- Further submissions enter the shared FIFO queue. Inputs remain editable while
  work runs; the button becomes **Add to queue**. Confirmed submission clears
  URL/filename; an unconfirmed or rejected submission does not.
- Separate **Downloading**, **Queued** (FIFO positions), **Needs attention** and **Finished** sections.
  Remove queued jobs without touching output; cancel running jobs safely.
  The next job starts only after the previous engine worker has fully cleaned up.
- Coalesced service progress, sampled speed, known/unknown totals and outcomes.
- **Closing the window leaves downloads running. Reopening reconnects to the
  same service and shows current records.** No transfer future/token lives here.
- Cancel targets a specific job in the current service instance.
- **Stop service & cancel downloads** explicitly cancels and drains active work.
- Connection loss shows last-known state, not a fabricated failure/completion.
  Reconnection to an existing service is read-only; click **Reconnect / start
  service** to start a stopped service. Unconfirmed commands are never retried.
- Existing completed files remain unchanged and show **Already exists**, not
  newly downloaded or verified. Valid partial state uses the existing engine's
  resume rules when re-adding the same URL and destination.
- The service owns configuration, network safety and errors. A running service's
  configuration/environment does not change when another GUI is launched.

Records are capped at 64 and persist privately in the service's SQLite database.
After restart, unfinished work appears as **Interrupted** under **Needs attention**;
no network activity resumes automatically. **Resume / retry** explicitly creates
a fresh attempt at the FIFO tail, using the saved destination and engine resume rules.
Failed/cancelled jobs also offer retry; completed files remain protected.
**Remove** releases held Interrupted work without touching partial files.
Saved progress may lag after a crash; it is not proof of a valid resumable offset.
See the service README for state location, permissions and plaintext-URL privacy.
Parallel downloads, provider/SFTP routing, graphical folder picker, pause, tray
and installer remain future milestones. Firefox explicit handoff is separate.

Direct HTTP/HTTPS CLI files now share this service. Provider/SFTP routes, listings,
sync and the persisted queue still do not: avoid concurrent writes to the same
destination from those legacy paths. See [service setup, security and limitations](../rdm-service/README.md).

## Tests and manual checks

```sh
cargo fmt -p rdm-gui --check
cargo check -p rdm-gui --all-targets --locked
cargo test -p rdm-gui --locked
cargo clippy -p rdm-gui --all-targets --locked -- -D warnings
```

GUI model tests cover close/detach, disconnection, revisions, generation changes,
stale subscription/command responses, unknown totals and safe Debug output.
The service crate retains the previous transfer adapter's tests and adds
process-level lifecycle tests.

On a Linux desktop, start a large download, close the GUI, reopen and verify the
same job progressed; cancel it and inspect retained partial state. Also test
explicit shutdown, missing service executable, permissions/version mismatch,
service crash/restart, existing output, invalid input and unknown-size transfers.
Test keyboard navigation, scaling, accessibility, X11/Wayland and renderer/resource
budgets on target machines rather than inferring them from successful compilation.
