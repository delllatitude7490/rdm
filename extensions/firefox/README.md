# Firefox → RDM (Linux development preview)

Plain JavaScript/HTML/CSS, no React, bundler, content scripts, localhost server,
background polling or permanent native connection. Firefox MV3 uses a suspendable
background event page. Requires desktop Firefox **140+** and matching Rust
binaries. Not a signed/AMO release; Android, Chromium, Flatpak/Snap integration
and automatic download interception are outside this milestone.

## Build and register

From the repository root, in a normal Linux desktop session:

```sh
cargo build -p rdm-service -p rdm-gui -p rdm-native-host --release --locked
./target/release/rdm-native-host --install
```

The per-user registration is `~/.mozilla/native-messaging-hosts/rdm_firefox.json`.
No sudo. It points to the current native-host binary and permits only add-on ID
`rdm@delllatitude7490`. Both native binaries must remain beside each other at
that location. Registration refuses to replace an existing file or symlink; to
change it, inspect/remove that specific manifest and run `--install` again.
Do not register a debug binary then delete its build directory.

Existing `rdm-service` must be a matching build. Shut down an older service with
its `--shutdown` command before trying a new version. `XDG_RUNTIME_DIR` must be
private/user-owned; there is no insecure automatic fallback.

## Load in Firefox

1. Open `about:debugging#/runtime/this-firefox`.
2. Choose **Load Temporary Add-on…**, then this directory's `manifest.json`.
3. Open a normal (not private) browser window and pin the RDM toolbar action if
   desired. Choose **Check connection**: it reports status without starting RDM.
4. Right-click a direct public file link → **Download with RDM**, or open the
   popup, paste a URL, and choose **Download with RDM**. **Use current tab** only
   fills the field; review and explicitly send it. A web page URL is not
   necessarily a downloadable file.
5. Open `./target/release/rdm-gui` to view progress/cancel. Closing Firefox,
   the popup or the GUI leaves the service-owned transfer running.

Temporary add-ons disappear on Firefox restart. A distributable persistent
installation needs Mozilla signing/review and platform selection; this PR does
not publish or sign anything. A source zip made with `web-ext build` is unsigned,
not an AMO-ready signed XPI.

## Feedback and deliberate limits

- Toolbar badge: ✓ for accepted/queued/connected, ! for errors. Hover/popup shows the
  generic last result (action title, not stored URLs). Popup uses textContent,
  never HTML interpolation of untrusted data.
- One running managed transfer globally. Further links join the service-owned
  FIFO. **Queued in RDM** means it has not started; **Accepted** is not completion.
  Open the GUI for queue position, queued removal or active cancellation.
  At 64 unfinished jobs the bounded queue refuses new submissions explicitly.
  Accepted means the request reached RDM; it does **not** mean completion.
- Default save folder and automatic filename come from RDM, never the webpage.
  Change service config/restart it to change the folder.
- Only direct HTTP/HTTPS file links; no embedded userinfo, directory URLs, blob,
  data, browser-internal or file URLs. Known provider shares remain unsupported.
- No cookies, Authorization/Referer headers or browser session are forwarded.
  Login-protected downloads may fail; copying their page URL does not transfer
  authentication. Private browsing is explicitly disabled.
- No automatic interception, cancellation of Firefox downloads, media scraping,
  history scanning, page-wide access, shutdown or remote filesystem control.
- If the bridge/reply disappears, the transfer may already have been accepted.
  **Inspect the GUI before retrying.** No automatic mutation retry is performed.
- Service queue/history is persisted privately; unfinished recovery waits for
  explicit Resume / retry in the GUI or CLI.
  Direct HTTP/HTTPS CLI files share this service. Provider/SFTP, listings, sync
  and persisted queue routes remain outside coordination: do not use those
  against the same destination concurrently. Parallel transfers remain future work.

## Permissions and privacy

Only `nativeMessaging`, `menus`, `activeTab`; no `<all_urls>`, host permissions,
`tabs`, cookies, history, downloads, clipboard or storage permission. A URL is
sent to the local Rust bridge only after explicit user action. Signed query
parameters are preserved; they may contain sensitive tokens. RDM then contacts
the selected server under its existing network and resume-file safety model.
No analytics, telemetry, remote scripts or extension URL/history persistence.
RDM's pre-existing resume files retain their existing protection model.

The manifest declares `browsingActivity` transmission rather than `none` because
selected URLs are passed outside the add-on to a native application. The local
Setup & privacy page explains this distinction. Review Mozilla policy and
platform/signing requirements again before any AMO publication.

## Development checks

No npm install is needed for the JavaScript tests (Node 20+):

```sh
node --test extensions/firefox/tests/*.test.js
cargo fmt -p rdm-native-host --check
cargo clippy -p rdm-native-host --all-targets --locked -- -D warnings
cargo test -p rdm-native-host --locked
cargo build -p rdm-native-host -p rdm-service --locked
python3 extensions/firefox/tests/native_smoke.py target/debug
```

The smoke test uses an isolated private runtime/config/HOME and per-child
`RDM_ALLOW_PRIVATE=1` for its local HTTP fixture. It exercises real native stdio,
registration, auto-start, queued feedback/removal, exact downloaded bytes, host-exit independence,
output protection, command restrictions, shutdown, and read-only status. It does
not install an extension or claim live Firefox API coverage. Node tests mock the
browser API for menu/permission/no-retry boundaries.

Optional Mozilla tooling:

```sh
npx --yes web-ext@10.8.0 lint --source-dir extensions/firefox --ignore-files 'tests' 'tests/**' package.json README.md
npx --yes web-ext@10.8.0 build --source-dir extensions/firefox --ignore-files 'tests' 'tests/**' package.json README.md --artifacts-dir /tmp/rdm-firefox-artifacts
```

A linter warning about Android 140/data-consent support may occur: this extension
has no `gecko_android` entry and is desktop-only. It must not be published for
Android. Treat any other warning/error as a review issue.

## Remove development registration

Remove the temporary add-on in `about:debugging`. Inspect then delete only:

```sh
rm -- "$HOME/.mozilla/native-messaging-hosts/rdm_firefox.json"
```

This does not stop an already running service or remove downloads/configuration.
Use the GUI's explicit service stop, or `rdm-service --shutdown`, if desired.

Queue upgrade: stop an old service with its matching binary before rebuilding.
Build matching Rust binaries (internal IPC v4) and reload this extension (0.3.0).
The external Native Messaging framing/protocol remains v1. Queue and history
survive service restart; unfinished work is held Interrupted until explicit retry.
The local Rust service stores retry URLs (including query tokens) in owner-only
plaintext SQLite under XDG_STATE_HOME/rdm/service, not extension storage.
Successful records drop their URLs; bounded terminal eviction removes old input.
