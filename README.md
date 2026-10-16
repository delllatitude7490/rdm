# Rust Download Manager
- A "usable" download manager. Emphasis on usable.
- HTTP/HTTPS/[SFTP](/extraInfo/sftp.md)
- Resume support on most connections like other download managers.
- Efficient as Fuck.
- Written in rust so it must be cool. Emphasis on must.
- Linux only.

## Native GUI preview (Iced + Rust service)

The optional Rust/Iced app now connects to a per-user Rust download service.
The CLI remains the default build and does not depend on Iced. Direct file
downloads now require the matching `rdm-service` binary alongside it or on PATH.

```sh
cargo build -p rdm-service -p rdm-gui --release
./target/release/rdm-gui
```

The GUI starts the matching service on demand. **Closing the window leaves
managed downloads running; reopening reconnects.** Stop the service explicitly
from the GUI or with `rdm-service --shutdown` to cancel and drain work.

This preview supports one direct HTTP/HTTPS managed download at a time. Records
are bounded and in-memory, not durable history. Direct HTTP/HTTPS CLI files,
the GUI and Firefox handoffs share admission and job state. Provider/SFTP routes,
HTTP listings, sync and the persisted queue remain outside the service: never
target the same output concurrently from those legacy paths. Concurrent managed
queues and pause remain future milestones.

See [GUI setup](crates/rdm-gui/README.md) and
[service architecture, runtime requirements and limitations](crates/rdm-service/README.md).

# HoW To uSe
```
RDM — Rust Download Manager

Usage: rdm [OPTIONS] [URL]
       rdm <COMMAND>

Commands:
  download  Download a single URL [aliases: d]
  sync      Mirror a remote directory listing into a local directory
  queue     Manage the download queue [aliases: q]
  jobs      Inspect downloads managed by the shared service (does not start it)
  config    Show the effective configuration
  help      Print this message or the help of the given subcommand(s)

Arguments:
  [URL]  URL to download (shorthand for `rdm download <URL>`)

Options:
  -o, --output <PATH>     Output file or directory [default: download_dir from config]
  -c, --connections <N>   Connections per file [default: connections from config]
      --allow-private     Allow scanning private, loopback and link-local addresses [aliases: --ap]
  -q, --quiet             Suppress progress output
  -p, --parallel <N>      Files to download concurrently if <URL> is a directory listing [default: queue_parallel from config]
      --detach            Return after service acceptance (direct files only)
  -h, --help              Print help
  -V, --version           Print version

Defaults for -c/-p and the download directory come from config.toml.
Run `rdm config` to see the values currently in effect.

-p applies only when <URL> is a directory listing, which is expanded into
the queue and downloaded concurrently.

sync and queue have options of their own — see `rdm sync --help` and
`rdm queue --help`.
```

If you have set up the path variable like a normal person then you can reproduce the above wall of text by typing 'rdm', assuming the variable is rdm.

`-o`, `-c`, `--allow-private` and `-q` work on every download path: bare `rdm <URL>`, `rdm download`, `rdm sync` and `rdm queue add`.
`--ap` is a shorthand for `--allow-private`.

## Shared direct-file CLI downloads

Build/install the matching CLI and service together (the GUI is optional):

```sh
cargo build -p rdm -p rdm-service --release --locked
./target/release/rdm download https://example.com/file.zip
./target/release/rdm download https://example.com/file.zip -o another.zip --detach
./target/release/rdm jobs list
./target/release/rdm jobs list --json
./target/release/rdm jobs inspect INSTANCE:ID --json
./target/release/rdm jobs cancel INSTANCE:ID
```

- The default waits for the service's terminal result; success means completed
  bytes, not just acceptance. `--quiet` suppresses progress, not failure exit codes.
- `--detach` returns after acceptance, printing the full `INSTANCE:ID` handle to
  stdout even with `--quiet`. Acceptance does **not** mean completion. Closing
  the CLI or GUI does not stop the detached download.
- While waiting, Ctrl+C/SIGTERM requests cancellation of **that job only**, then
  waits for engine cleanup. A second signal stops waiting with an unknown-result
  error, not a false cancellation confirmation. It never shuts down the service.
- `jobs` commands do not start a stopped service, read local config or enqueue
  anything. Cancellation acknowledgement is not proof cleanup has completed.
- Source URLs are omitted from snapshots. JSON is machine-readable; ordinary
  terminal rows sanitize and clip filenames/paths. Handles include the service
  instance so old handles cannot cancel reused numeric IDs after restart.
- `-o` and `-c` resolve against this invocation's config; relative output follows
  the existing configured-download-directory rules, then becomes absolute in
  the CLI's working directory. `--allow-private` / caller `RDM_ALLOW_PRIVATE=1`
  are passed explicitly. The service's own private-network opt-in also applies.
  Other retry, proxy and size-limit config/environment stays service-owned;
  restart the service to apply changes. No environment is copied per request.
- Existing completed output is left unchanged and the CLI exits nonzero asking
  for a different `-o` path. Unlike the old interactive path, managed downloads
  never prompt for overwrite. Valid partial state follows the engine's normal
  resume rules: retry the same URL and output after cancellation has finished.
- Missing service, unsafe runtime, incompatible version, Busy or lost replies
  fail explicitly; no silent fallback to an independent worker and no automatic
  mutation retry. Inspect before resending an uncertain command.
- One active managed job across all clients; 64 recent in-memory records, lost
  on service restart. This is not the persisted `rdm queue` and not a new queue.
- Provider shares, SFTP, directory-shaped URLs, discovered listings, sync and
  queue still use their existing routes. `--detach` is rejected for them. For
  extensionless direct links use `rdm download URL --detach` (the bare form may
  try listing discovery). Empty/failed discovery falls back to managed direct
  files unless the URL is directory-shaped; those retain the legacy path.

This change uses internal IPC **version 2**. Before upgrading, explicitly stop
any old service with its matching GUI/service binary. Build CLI, service, GUI
and Native Messaging host from the same checkout if installed; an incompatible
service is never automatically stopped or replaced. The Firefox extension's
external Native Messaging protocol remains version 1.

## Sync

```
rdm sync <URL> [-o dir] [-c N] [-p N] [-d] [-e flac,mkv]

  -p, --parallel <N>   Files to download concurrently [default: queue_parallel from config]
  -d, --delete         Delete local files that no longer exist on the remote
  -e, --ext <EXT>      Only sync these extensions (repeatable or comma separated)
```

`-o` sets the output path only for a sync, not a filename. `-e` takes a comma separated list or repeated flags, with or without leading dots, and is case insensitive: `-e flac,mkv` and `-e .flac -e .MKV` do the same thing.

## Queue

```
rdm queue add <URL> [-o name] [-c N]     Add to queue                [a]
rdm queue list                           Show queue                  [ls, l]
rdm queue start [-p N]                   Start processing            [run, s]
rdm queue stop                           Stop after current download
rdm queue skip                           Skip the download(s) in flight  [next, n]
rdm queue remove <ID>                    Remove one item             [rm]
rdm queue retry [ID|failed|skipped]      Requeue items               [r]
rdm queue clear [pending|done]            Clear queue (all by default)   [c]
```

`-p` on `queue start` defaults to `queue_parallel` from the config.

Directory-looking URLs are scraped: `rdm <URL>` on a listing enqueues everything it finds and starts downloading, and `rdm queue add <URL>` enqueues without starting.

# Hoster Section

- [mega - click to view](extraInfo/mega.md)
- [dropbox - click to view](extraInfo/dropbox.md)
- [onedrive - click to view](extraInfo/onedrive.md)
- [gdrive - click to view](extraInfo/gdrive.md)
- [pixeldrain - click to view](extraInfo/pixeldrain.md)

# Example Config File

```
# Parallel connections per file
connections = 12

# Default download directory
download_dir = "~/Downloads"

# Max Retries?
max_retries = 69

# multi-file download at once
queue_parallel = 5

# MEGA: chunk workers per file
mega_workers = 6

# MEGA: verify the MAC after downloading. Costs a full reread of the file.
# Turning this off means silent corruption stays silent.
mega_verify_mac = true

# MEGA: when quota-blocked, resume early if your public IP changes
mega_resume_on_ip_change = true

# OneDrive: how many files to download at once (max 15)
onedrive_workers = 5

# Google Drive: how many files to download at once (max 15)
# Drive's quota is per key per second, so a big number buys 403s
gdrive_workers = 5

# Google Drive: API key. Optional — a folder still lists without one, but only
# the API pages a listing to the end and gives every file a size.
# RDM_GDRIVE_API_KEY overrides this for a single run.
gdrive_api_key = ""

# Google Drive: what a Doc, Sheet, Slide deck or Drawing is exported as.
# An extension (pdf, docx, xlsx, csv, png, ...) or "office"
gdrive_doc_format = "pdf"

# pixeldrain: how many files of a list to download at once (max 10)
pixeldrain_workers = 4

# pixeldrain: API key. Optional — it buys speed, not access: anonymous
# transfers are capped and an account's are not.
# RDM_PIXELDRAIN_API_KEY overrides this for a single run.
pixeldrain_api_key = ""
```

Everything in the config is optional as they have their own defaults.

# Environment

All optional, there are other hoster specific env variables which can find in the relevant hoster sections.

| Variable | Effect |
|---|---|
| `RDM_MAX_FILE_BYTES` | Size ceiling for one streaming download. Default 64 GiB, `0` removes it |
| `RDM_ALLOW_PRIVATE` | Same as `--allow-private` |
| `RDM_ALLOW_PROXY` | Let `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` apply to RDM's requests |

- `RDM_ALLOW_PRIVATE` and `RDM_ALLOW_PROXY` are off unless set to `1`, `true`, `yes` or `on`. Setting either to nothing, or to `false`, leaves the protection on.
- `RDM_MAX_FILE_BYTES` only bites when a server refuses to say how big a file is.

# Release
Zack encourages you to build from source. As some random internet person once said, "Always build from source"

NixOS users get a flake for easy installation :)

# Build From Source
Make sure you have libressl, zlib, libssh2, pkg-config and cargo installed

```
git clone https://github.com/MasterZack69/rdm
cd rdm
cargo build -p rdm -p rdm-service --release --locked
```

# Credits
- MasterZack69 - Of course I am getting the Credits
- Claude Opus 4.6 - Wrote the code, fixed the bugs
- GPT 5.2 - Asked “what if it races?” one too many times
- Claude Opus 4.7 - Here to do everything better
- Claude Opus 5 - Clap Migration, Queue System and Hosters
- DeepSeek V4 Flash - Clippy error fixer
- GPT 6 Astra - SFTP

# Prior art
- [MegaBasterd](https://github.com/tonikelope/megabasterd)
- [onedrive-downloader](https://github.com/eugenenuke/onedrive-downloader)
- [goodls](https://github.com/tanaikech/goodls)
- [gdown](https://github.com/wkentaro/gdown)
- [pixeldrain-downloader](https://github.com/neiromaster/pixeldrain-downloader)

## Firefox link handoff (Linux preview)

A plain-JavaScript Firefox extension can send an explicitly chosen file URL to
RDM through a small Rust Native Messaging host. No webview, React, localhost
server, automatic interception or browsing-history access.

```sh
cargo build -p rdm -p rdm-service -p rdm-gui -p rdm-native-host --release --locked
./target/release/rdm-native-host --install
```

Then load `extensions/firefox/manifest.json` as a temporary add-on from
`about:debugging#/runtime/this-firefox`. Right-click a direct file link and choose
**Download with RDM**, or use the popup. Firefox 140+ on ordinary desktop Linux is
required; the extension is unsigned and not an AMO release. See the
[Firefox setup, permissions and limits](extensions/firefox/README.md) and
[native host protocol](crates/rdm-native-host/README.md). Existing single-managed-
download and in-memory history limits still apply. Provider/SFTP/listing/sync/queue
routes remain outside service coordination.
