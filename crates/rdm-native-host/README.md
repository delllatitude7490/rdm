# RDM Firefox Native Messaging host (Linux preview)

A small, short-lived Rust adapter between Firefox Native Messaging stdin/stdout
and RDM's private Unix socket. No webview, HTTP listener, persistent browser port,
framework or GUI dependency. The extension uses `runtime.sendNativeMessage`, so
one native host process handles one command and exits; downloads belong to the
separate `rdm-service` and survive the host/browser exiting.

## Setup

See [the Firefox README](../../extensions/firefox/README.md) for build, temporary
extension loading, permissions, privacy and end-to-end testing.

```sh
cargo build -p rdm-service -p rdm-gui -p rdm-native-host --release --locked
./target/release/rdm-native-host --install
```

`--install` registers the absolute canonical path to this binary in
`~/.mozilla/native-messaging-hosts/rdm_firefox.json`. It requires an executable
sibling `rdm-service`, writes only a new mode-0600 manifest, refuses existing
files/symlinks and never changes service configuration. It does not copy binaries,
install/start Firefox, or use sudo. Keep matching binaries together at that path.
To move/update an existing registration, inspect it, remove that specific manifest
and rerun `--install`; no blind replacement is performed.

## Restricted protocol

Firefox frames: u32 **native-endian** length followed by UTF-8 JSON. This is NOT
the service socket's big-endian framing. Input/output are additionally capped at
64 KiB before input allocation. Exactly one response is written; stdout contains
no ordinary text. Malformed/unknown fields/actions fail closed with generic
errors. Browser-origin requests cannot register/unregister the host.

```json
{"version":1,"command":{"action":"add","url":"https://example.com/file.zip"}}
```

```json
{"version":1,"command":{"action":"status"}}
```

- Add starts/connects to the service, reads its configured default directory and
  sends one Add with automatic filename. It cannot accept a browser-supplied path,
  filename, headers, cookies, network opt-in, Cancel or Shutdown. The service and
  engine retain authority over URL/provider/DNS/redirect/file policy.
- Status reads List without starting the service; only ready/active/stopping
  status is returned, never URLs, paths, filenames or other clients' records.
- Reply: `{version, ok, code, message, job_id?}`. Acceptance is not completion.
  `accepted` means admitted to the running slot; `queued` means accepted into
  the shared FIFO but not yet started. Neither confirms completion. A full
  bounded queue returns `busy`; the host does not queue locally or retry.
  External protocol stays version 1; use extension 0.3.0 with matching Rust IPC
  version-4 binaries so queued replies are recognized.
- Uncertain command outcomes are reported, never automatically retried. An IPC
  rejection's arbitrary message is not forwarded into the browser.
- Firefox's passed add-on ID must match `rdm@delllatitude7490`. Registration lists
  only that ID. This supplements browser enforcement; it is not authentication
  against a process already running as the same OS user.

The service is not spawned on extension installation, popup opening or a
connection check—only when explicitly sending a URL. Configuration/environment
are those of the service. Sandboxed Flatpak/Snap Firefox integration is not
configured. No arbitrary `/tmp` runtime fallback or safety bypass is added.
