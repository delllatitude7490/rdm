# RDM native preview

A small Rust/Iced desktop frontend over the RDM service. No React, JavaScript,
webview, CLI-output parsing, or GUI-owned transfer workers.

## Build and run

Use current stable Rust plus the root README's native prerequisites. On
Debian/Ubuntu a useful starting point is:

```sh
sudo apt-get install build-essential cmake pkg-config libssl-dev \
    libxkbcommon-dev libxkbcommon-x11-0 libwayland-dev libx11-dev libxcursor1 xdg-utils
cargo build -p rdm-service -p rdm-gui --release
./target/release/rdm-gui
```

Build/install matching Rust CLI, service, GUI and native host from one checkout
when installed (internal IPC 7). Stop the old service explicitly before upgrading.
SQLite stays schema 2; do not reset it. The extension remains 0.3.0 / external v1.
Build/install **both** matching GUI/service binaries. The GUI locates `rdm-service` beside
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
- Separate **Downloading**, **Queued** (FIFO positions), **Paused**, **Needs attention** and **Finished** sections.
  Pause running/queued work, or remove queued jobs without touching output; cancel running jobs safely.
  Pausing safely is distinct from confirmed Paused. Resume creates a fresh attempt at the FIFO tail.
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
deliberately paused jobs stay **Paused** in their own section. No network activity
resumes automatically. **Resume** / **Resume / retry** explicitly creates
a fresh attempt at the FIFO tail, using the saved destination and engine resume rules.
Failed/cancelled jobs also offer retry; completed files remain protected.
**Remove** is separate from Cancel: it forgets queued or any terminal card and its
saved retry input, including Paused/Interrupted, failed/cancelled and Finished
history. It appears on every terminal card and never deletes output/partial files.
No optimistic hiding: only a confirmed service snapshot removes the card.
A running/draining job must finish cancellation before it can be removed.
Saved progress may lag after a crash; it is not proof of a valid resumable offset.
See the service README for state location, permissions and plaintext-URL privacy.
Parallel downloads, provider/SFTP routing, graphical folder picker, tray
and installer remain future milestones. Firefox explicit handoff is separate.

Direct HTTP/HTTPS CLI files now share this service. Provider/SFTP routes, listings,
sync and the persisted queue still do not: avoid concurrent writes to the same
destination from those legacy paths. See [service setup, security and limitations](../rdm-service/README.md).

## Explicit desktop and history actions

Successful **Completed** / **Already exists** cards offer **Open folder** and
**Open file**. Folder requests open the containing directory, not select a file.
Open file first asks for confirmation; cancel does nothing. No completion,
reconnection, restart or history restore launches anything automatically.

The GUI hands one percent-escaped local `file:` URI directly to `xdg-open`,
without a shell. Install `xdg-utils` and configure default desktop applications.
Missing helpers, inaccessible/moved paths, launcher failures and a bounded
10-second timeout are reported without automatic retry. A successful helper exit
only confirms that opening was requested, not that an application window opened.
A timeout is uncertain; inspect your desktop before trying again.

Open file uses a read-only, nonblocking, no-follow descriptor check and requires
a regular, user-owned, single-link file. It refuses executable permission bits,
known script/launcher extensions and common executable/script signatures; use
Open folder to inspect such downloads instead. These checks are **not a malware
scanner or sandbox** and do not identify every executable format. The external
default handler reopens a path: another same-user process could replace it after
validation. Open only files you trust. Already exists does not verify file content.

**Clear finished** appears only for the Finished section. Its confirmation freezes
that section's exact IDs and service instance. Newly completed jobs while the
prompt is open are not included. Active/queued, Paused, Interrupted and retryable
Failed/Cancelled jobs stay available; retired nonretryable attempts can be cleared.
Individual Remove is still available when deliberately discarding retryable work.
Clearing changes history only, never downloads, partial files or checkpoints.
The service validates the entire batch and commits once before ACK/events; the
GUI never hides records optimistically. A disconnect/restart invalidates the
prompt; an uncertain command must be inspected rather than resent automatically.

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

The GUI model tests also cover frozen clear selections, confirmation cancellation,
changed/removed paths, restart/disconnect invalidation and explicit-only opening.
Desktop unit tests use a fake opener to check literal URI arguments, exit failures,
missing helpers, timeouts, unusual filenames and unsafe filesystem types. They do
not verify every desktop's real default handler. Manually check both open actions,
cancel/confirm, missing files, absent xdg-utils, and clear/restart with files intact.
