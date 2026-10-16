# RDM native preview

A small Iced desktop frontend over the existing RDM library. No React,
JavaScript, webview, or CLI-output parsing in the desktop application.

## Build and run

Use a current stable Rust toolchain, plus the dependencies listed in the root
README (including OpenSSL/LibreSSL, zlib, libssh2, pkg-config, and a C compiler).
The existing RDM dependencies may require a newer compiler than Iced's minimum.
On Debian/Ubuntu a useful starting point is:

```sh
sudo apt-get install build-essential cmake pkg-config libssl-dev \
    libxkbcommon-dev libxkbcommon-x11-0 libwayland-dev libx11-dev libxcursor1
cargo run -p rdm-gui --release
```

Run in a Linux desktop session with X11 or Wayland. Default rendering uses
`tiny-skia` (software); there is no bundled browser runtime. For a GPU comparison:

```sh
cargo run -p rdm-gui --release --features gpu
```

The GPU feature adds Iced's wgpu renderer; keep the selected renderer and process
memory in your benchmark notes. Software rendering is not a promise of lower
CPU or memory on every machine. A working GPU driver is required for wgpu.

The root package is the workspace default. Existing `cargo build`, `cargo test`,
and CLI-only installs do not build the GUI. To build both explicitly:

```sh
cargo build --workspace
```

## Supported in this milestone

- One direct HTTP/HTTPS file at a time, with the configured connection count.
- Destination folder and optional filename; `~` / `~/` are expanded for the folder.
- Byte progress, known totals, sampled speed, and a static unknown-size state.
- Cancellation propagated to the existing engine. Closing the window requests
  cancellation and waits for the engine future to finish before exiting.
- Engine-supported retry/resume state remains managed by the engine. Retry an
  interrupted download using the same URL and destination.
- Existing completed files are left unchanged and reported as **Already exists**,
  not verified or reported as newly downloaded. There is no overwrite prompt.
- Configuration loads once from the existing RDM configuration file. An invalid
  configuration blocks new downloads instead of silently resetting settings.
- Existing private-address, redirect, proxy, safe-file and resume policies are
  retained. `RDM_ALLOW_PRIVATE=1` remains an explicit opt-in for local networks.
- No raw engine error chains or notice bodies in the GUI, since they can contain
  signed links or credentials. Errors are deliberately categorised.

The folder is a text field, not a graphical folder picker yet. A URL that returns
an HTML page rather than a downloadable file may download that page: this preview
is not a content-discovery or browser-authentication tool.

## Deliberate limitations

No provider-share routing (MEGA, Dropbox, OneDrive, Drive, pixeldrain), SFTP,
directory scraping, queue management, history persistence, per-file pause, browser
extension, tray, installer, or background service yet. Use the CLI for those
existing workflows. Known provider URLs and directory-shaped URLs are refused
early rather than silently routed through the generic file engine.

Closing does **not** leave downloads running in the background. Cancellation can
wait for an outstanding engine network operation; there is no unsafe force-abort
button. A background service is the next architecture milestone, not part of this
preview. The GUI does not coordinate an active download with a separate CLI
process targeting the same file; avoid concurrent writes to the same destination.

The UI uses a dark theme for now. It has no permanent animation or polling timer
when idle; while active, one coalesced progress snapshot is sampled every 250 ms.
Terminal outcomes arrive through the download task separately from snapshots.
Do not infer a performance budget from this architecture alone.

## Validation

```sh
cargo fmt -p rdm-gui --check
cargo check -p rdm-gui --all-targets --locked
cargo test -p rdm-gui --locked
cargo clippy -p rdm-gui --all-targets --locked -- -D warnings
cargo test -p rdm --locked
```

Tests cover URL and filename validation, secret-safe messages, coalesced progress,
unknown totals, stale terminal events, close-request cancellation, and real local
HTTP transfers, including cancellation and existing-file protection. Local HTTP
tests explicitly opt in to loopback access without changing global environment
variables or weakening production defaults.

For a manual release smoke test, verify known/unknown size files, an empty file,
network errors, existing output, cancellation mid-transfer, retrying partial state,
and closing during a transfer. On target machines compare idle and active CPU,
RSS/PSS across the process tree, responsiveness, and both X11 and Wayland. Test
keyboard navigation, text input, scaling, and assistive technology separately;
those behaviours are not established by a successful build.
