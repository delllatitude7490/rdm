//! The service-owned, non-terminal transfer adapter. No queue-file edits.
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rdm::engine::{self, DownloadRequest, ExistingPolicy, Outcome};
use rdm::ui::{ProgressSink, SlotState};
use tokio_util::sync::CancellationToken;

pub use rdm_ipc::{Finished, Progress as Snapshot};

#[derive(Clone, Default)]
pub struct GuiSink(Arc<Mutex<Snapshot>>);

impl GuiSink {
    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn change(&self, f: impl FnOnce(&mut Snapshot)) {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

impl ProgressSink for GuiSink {
    fn total(&self, bytes: Option<u64>) {
        self.change(|s| s.total = bytes);
    }

    fn progress(&self, downloaded: u64) {
        self.change(|s| s.downloaded = downloaded);
    }

    fn state(&self, phase: SlotState) {
        self.change(|s| {
            s.phase = match phase {
                SlotState::Waiting => rdm_ipc::Phase::Waiting,
                SlotState::Inspecting => rdm_ipc::Phase::Inspecting,
                SlotState::Downloading => rdm_ipc::Phase::Downloading,
                SlotState::Finishing => rdm_ipc::Phase::Finishing,
            }
        });
    }

    fn note(&self, _: &str) {
        // Engine notices may contain signed URLs. Do not forward arbitrary text.
        self.change(|s| s.has_notice = true);
    }

    // detail and finish intentionally do nothing. finish is NOT success.
}

pub struct Input {
    pub request: DownloadRequest,
    pub directory: PathBuf,
    pub label: String,
}

/// Validate without echoing secret-bearing input into errors or Debug output.
/// Known hosters are refused BEFORE generic URL normalization.
pub fn prepare(
    url: &str,
    directory: &str,
    filename: &str,
    connections: usize,
) -> Result<Input, String> {
    if url.len() > 16 * 1024 || directory.len() > 4096 || filename.len() > 255 {
        return Err("URL or destination exceeds the preview's size limits.".into());
    }
    let url = url.trim();
    if rdm::hoster::detect(url).is_some() {
        return Err("Provider share links are not supported in this preview. Use the CLI.".into());
    }
    let parsed = rdm::net::parse_and_validate_url(url, false).map_err(|_| {
        "Enter a valid public HTTP/HTTPS file URL. Private networks require RDM_ALLOW_PRIVATE=1."
            .to_owned()
    })?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "URLs containing usernames or passwords are not supported in this preview.".into(),
        );
    }
    if parsed.path().ends_with('/') {
        return Err("Directory links are not supported yet. Paste a direct file URL.".into());
    }
    let directory = expand_directory(directory.trim())?;
    let name = if filename.trim().is_empty() {
        engine::extract_filename_from_url(url).unwrap_or_else(|| "download.bin".into())
    } else {
        filename.trim().to_owned()
    };
    validate_filename(&name)?;
    let output = directory.join(&name);
    let output = output
        .to_str()
        .ok_or("The destination must be valid UTF-8.")?
        .to_owned();
    // Reuse never prompts on stdin and never deliberately overwrites a completed
    // file. The UI calls this 'Already exists', not 'Downloaded'. Existing valid
    // partial state still follows the engine's normal resume rules.
    let request = DownloadRequest::new(url.to_owned(), Some(output), connections)
        .with_policy(ExistingPolicy::Reuse);
    Ok(Input {
        request,
        directory,
        label: name,
    })
}

fn expand_directory(value: &str) -> Result<PathBuf, String> {
    let path = if value == "~" {
        dirs::home_dir().ok_or("Could not locate your home directory.")?
    } else if let Some(rest) = value.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or("Could not locate your home directory.")?
            .join(rest)
    } else {
        PathBuf::from(value)
    };
    if !path.is_absolute() {
        return Err("Choose an absolute destination folder, or use ~/Downloads.".into());
    }
    Ok(path)
}

fn validate_filename(name: &str) -> Result<(), String> {
    let mut components = Path::new(name).components();
    let single =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !single
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
        || engine::safe_filename(name).as_deref() != Some(name)
    {
        return Err(
            "Use a single safe filename, without directory separators or control characters."
                .into(),
        );
    }
    Ok(())
}

/// Wait for the engine to unwind and preserve its partial state. Do not select!
/// against cancellation and drop the engine future while it still owns workers.
pub async fn download(input: Input, sink: GuiSink, cancel: CancellationToken) -> Finished {
    if cancel.is_cancelled() {
        return Finished::Cancelled;
    }
    if tokio::fs::create_dir_all(&input.directory).await.is_err() {
        return Finished::Failed(
            "Could not create the destination folder. Check its permissions.".into(),
        );
    }
    if cancel.is_cancelled() {
        return Finished::Cancelled;
    }
    match engine::download(input.request, cancel.clone(), Arc::new(sink)).await {
        Ok(Outcome::Completed { path, bytes }) => Finished::Completed { path, bytes },
        Ok(Outcome::AlreadyPresent { path }) => Finished::AlreadyPresent { path },
        Ok(Outcome::Cancelled) => Finished::Cancelled,
        Err(_) if cancel.is_cancelled() => Finished::Cancelled,
        Err(error) => Finished::Failed(safe_error(&error)),
    }
}

fn safe_error(error: &anyhow::Error) -> String {
    // Arbitrary error chains can contain raw credentials, signatures, or remote
    // response bodies. Show useful categories, never format the chain here.
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
            if error.is_timeout() {
                return "The server timed out. Try again; valid partial state can be resumed."
                    .into();
            }
            if let Some(status) = error.status() {
                return format!(
                    "The server returned HTTP {}. Check the link or try again.",
                    status.as_u16()
                );
            }
            return "The network request failed. Check the link and your connection.".into();
        }
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            return format!(
                "A local file operation failed ({:?}). Check destination permissions and free space.",
                error.kind()
            );
        }
    }
    "Download failed. The link may be unavailable or blocked by RDM's network safety policy. Try the CLI for further diagnosis.".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_noninteractive_and_preserves_defaults() {
        let input = prepare("https://example.com/file.zip", "/tmp/rdm", "", 8).unwrap();
        assert_eq!(input.request.policy, ExistingPolicy::Reuse);
        assert!(!input.request.allow_private);
        assert!(input.request.client.is_none());
        assert_eq!(input.request.output.as_deref(), Some("/tmp/rdm/file.zip"));
        assert_eq!(input.request.connections, 8);
    }

    #[test]
    fn refuses_unsupported_links_without_echoing_secrets() {
        for url in [
            "file:///etc/passwd",
            "sftp://example.com/file",
            "https://example.com/folder/",
            "https://u:secret@example.com/file",
            "https://mega.nz/file/id#secret",
            "https://drive.google.com/file/d/id/view?key=secret",
        ] {
            let error = prepare(url, "/tmp", "", 8).err().expect("must refuse");
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn filename_cannot_escape_the_selected_directory() {
        for name in [
            "../file",
            "a/b",
            "a\\b",
            ".",
            "..",
            "/absolute",
            "bad\nname",
        ] {
            assert!(prepare("https://example.com/file", "/tmp", name, 8).is_err());
        }
        assert!(prepare("https://example.com/file", "relative", "file", 8).is_err());
    }

    #[test]
    fn progress_is_coalesced_and_finish_is_not_success() {
        let sink = GuiSink::default();
        sink.total(None);
        for bytes in 0..10_000 {
            sink.progress(bytes);
        }
        sink.finish();
        assert_eq!(sink.snapshot().downloaded, 9_999);
        assert_eq!(sink.snapshot().total, None);
        sink.total(Some(0));
        assert_eq!(sink.snapshot().total, Some(0));
    }

    #[test]
    fn error_chain_does_not_expose_arbitrary_text() {
        let error = anyhow::anyhow!("https://user:password@example.com/file?token=secret");
        let message = safe_error(&error);
        assert!(!message.contains("password"));
        assert!(!message.contains("secret"));
    }

    #[tokio::test]
    async fn pre_cancelled_download_does_not_create_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("not-created");
        let input = prepare(
            "https://example.com/file",
            destination.to_str().unwrap(),
            "",
            1,
        )
        .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            download(input, GuiSink::default(), cancel).await,
            Finished::Cancelled
        );
        assert!(!destination.exists());
    }

    struct Server {
        base: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve(router: axum::Router) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Server { base, task }
    }

    fn local_input(base: &str, dir: &Path) -> Input {
        let mut input =
            prepare("https://example.com/file.bin", dir.to_str().unwrap(), "", 1).unwrap();
        // Test-only opt-in. Production requests still go through the existing
        // network policy with private destinations disabled by default.
        input.request.url = format!("{base}/file.bin");
        input.request.allow_private = true;
        input
    }

    #[tokio::test]
    async fn downloads_real_bytes_and_does_not_overwrite_completed_files() {
        use axum::routing::get;
        let server =
            serve(axum::Router::new().route("/file.bin", get(|| async { "hello RDM" }))).await;
        let dir = tempfile::tempdir().unwrap();
        let sink = GuiSink::default();
        let result = download(
            local_input(&server.base, dir.path()),
            sink.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(result, Finished::Completed { bytes: 9, .. }),
            "{result:?}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("file.bin")).unwrap(),
            b"hello RDM"
        );
        assert_eq!(sink.snapshot().total, Some(9));

        std::fs::write(dir.path().join("file.bin"), b"keep me").unwrap();
        let result = download(
            local_input(&server.base, dir.path()),
            GuiSink::default(),
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Finished::AlreadyPresent { .. }));
        assert_eq!(
            std::fs::read(dir.path().join("file.bin")).unwrap(),
            b"keep me"
        );
    }

    #[tokio::test]
    async fn downloads_unknown_length_without_inventing_a_total() {
        use axum::body::{Body, Bytes};
        use axum::routing::get;
        let server = serve(axum::Router::new().route(
            "/file.bin",
            get(|| async {
                Body::from_stream(futures_util::stream::iter([
                    Ok::<_, std::io::Error>(Bytes::from_static(b"hello ")),
                    Ok(Bytes::from_static(b"stream")),
                ]))
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let sink = GuiSink::default();
        let result = download(
            local_input(&server.base, dir.path()),
            sink.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(result, Finished::Completed { bytes: 12, .. }),
            "{result:?}"
        );
        assert_eq!(sink.snapshot().total, None);
        assert_eq!(
            std::fs::read(dir.path().join("file.bin")).unwrap(),
            b"hello stream"
        );
    }

    #[tokio::test]
    async fn cancelling_a_live_stream_waits_for_engine_cleanup() {
        use axum::body::{Body, Bytes};
        use axum::routing::get;
        use std::time::Duration;

        let server = serve(axum::Router::new().route(
            "/file.bin",
            get(|| async {
                Body::from_stream(futures_util::stream::unfold(0_u64, |n| async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from_static(b"chunk")), n + 1))
                }))
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let sink = GuiSink::default();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(download(
            local_input(&server.base, dir.path()),
            sink.clone(),
            cancel.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while sink.snapshot().downloaded == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("download should begin");
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("engine should finish cancellation")
            .unwrap();
        assert_eq!(result, Finished::Cancelled);
        assert!(!dir.path().join("file.bin").exists());
        assert!(dir.path().join("file.bin.part").exists());
    }
}
