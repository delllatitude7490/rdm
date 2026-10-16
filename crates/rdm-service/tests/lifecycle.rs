//! Real service processes + real Unix sockets + local HTTP. No global env changes.
use axum::{
    Router,
    body::{Body, Bytes},
    routing::get,
};
use rdm_ipc::{
    Add, ErrorCode, Finished, Packet, Request, Response, Snapshot, client, runtime::Paths, wire,
};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::time::{sleep, timeout};

struct Service {
    child: Child,
    paths: Paths,
    root: tempfile::TempDir,
}
impl Service {
    fn spawn(root: tempfile::TempDir) -> Self {
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths::in_runtime(&runtime).unwrap();
        let config = root.path().join("config/rdm");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.toml"),
            "connections = 1\ndownload_dir = \"/tmp\"\nmax_retries = 0\nqueue_parallel = 1\n",
        )
        .unwrap();
        let child = Self::command(&root).spawn().unwrap();
        Self { child, paths, root }
    }
    fn command(root: &tempfile::TempDir) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rdm-service"));
        command
            .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("RDM_ALLOW_PRIVATE", "1")
            .env_remove("RDM_ALLOW_PROXY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
    async fn ready(&self) -> Snapshot {
        timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(snapshot) = client::request_at(&self.paths, Request::List).await {
                    break snapshot;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("service should become ready")
    }
    async fn until(&self, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = client::request_at(&self.paths, Request::List)
                    .await
                    .unwrap();
                if predicate(&snapshot) {
                    break snapshot;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("state should reach the expected condition")
    }
    async fn exit(&mut self) {
        timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("service should finish draining workers");
        assert!(!self.paths.socket.exists());
        assert!(
            self.paths.lock.exists(),
            "do not unlink the lifetime lock inode"
        );
    }
    fn add(&self, base: &str, filename: &str) -> Request {
        Request::Add(Add {
            url: format!("{base}/file.bin"),
            directory: self
                .root
                .path()
                .join("downloads")
                .to_string_lossy()
                .into_owned(),
            filename: filename.into(),
        })
    }
    async fn shutdown(&mut self) {
        let snapshot = self.ready().await;
        let _ = client::request_at(
            &self.paths,
            Request::Shutdown {
                instance: snapshot.instance,
            },
        )
        .await
        .unwrap();
        self.exit().await;
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Http {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Http {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn http(finite: bool) -> Http {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().route(
        "/file.bin",
        get(move || async move {
            Body::from_stream(futures_util::stream::unfold(0_u32, move |n| async move {
                if finite && n >= 30 {
                    return None;
                }
                sleep(Duration::from_millis(30)).await;
                Some((
                    Ok::<_, std::io::Error>(Bytes::from_static(&[42; 1024])),
                    n + 1,
                ))
            }))
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Http { base, task }
}

#[tokio::test]
async fn disconnect_reconnect_completion_and_existing_file_protection() {
    let http = http(true).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    let mut feed = client::subscribe_at(&service.paths).await.unwrap();
    let accepted = client::request_at(&service.paths, service.add(&http.base, "file.bin"))
        .await
        .unwrap();
    let id = accepted.jobs[0].id;
    let progress = timeout(Duration::from_secs(5), async {
        loop {
            let s = feed.next().await.unwrap();
            if s.jobs.iter().any(|j| j.progress.downloaded > 0) {
                break s;
            }
        }
    })
    .await
    .unwrap();
    drop(feed); // equivalent to the GUI closing; no cancel message is sent.
    sleep(Duration::from_millis(200)).await;
    let reconnected = client::subscribe_at(&service.paths).await.unwrap();
    assert_eq!(reconnected.initial.instance, accepted.instance);
    assert_eq!(reconnected.initial.jobs[0].id, id);
    assert!(
        reconnected.initial.jobs[0].progress.downloaded >= progress.jobs[0].progress.downloaded
    );
    drop(reconnected);
    let done = service.until(|s| s.jobs[0].result.is_some()).await;
    assert!(matches!(
        done.jobs[0].result,
        Some(Finished::Completed { bytes: 30720, .. })
    ));
    let output = service.root.path().join("downloads/file.bin");
    assert_eq!(fs::read(&output).unwrap(), vec![42; 30720]);
    fs::write(&output, b"keep me").unwrap();
    client::request_at(&service.paths, service.add(&http.base, "file.bin"))
        .await
        .unwrap();
    let reused = service
        .until(|s| s.jobs.last().unwrap().result.is_some())
        .await;
    assert!(matches!(
        reused.jobs.last().unwrap().result,
        Some(Finished::AlreadyPresent { .. })
    ));
    assert_eq!(fs::read(&output).unwrap(), b"keep me");
    service.shutdown().await;
}

#[tokio::test]
async fn one_owner_duplicate_launch_cancel_and_shutdown_drain() {
    let http = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let initial = service.ready().await;
    let status = Service::command(&service.root).status().unwrap();
    assert!(
        status.success(),
        "a duplicate launch must not replace the owner"
    );
    assert_eq!(service.ready().await.instance, initial.instance);
    let accepted = client::request_at(&service.paths, service.add(&http.base, "active.bin"))
        .await
        .unwrap();
    service.until(|s| s.jobs[0].progress.downloaded > 0).await;
    // Includes alias destinations and different names: this milestone admits
    // only one managed transfer, even if the engine later chooses another name.
    let error = client::request_at(&service.paths, service.add(&http.base, "other.bin"))
        .await
        .unwrap_err();
    assert!(matches!(error, client::Error::Rejected(ErrorCode::Busy, _)));
    let error = client::request_at(
        &service.paths,
        Request::Cancel {
            instance: "old-service".into(),
            id: accepted.jobs[0].id,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        client::Error::Rejected(ErrorCode::StaleInstance, _)
    ));
    client::request_at(
        &service.paths,
        Request::Cancel {
            instance: accepted.instance,
            id: accepted.jobs[0].id,
        },
    )
    .await
    .unwrap();
    let cancelled = service.until(|s| s.jobs[0].result.is_some()).await;
    assert_eq!(cancelled.jobs[0].result, Some(Finished::Cancelled));
    assert!(
        service
            .root
            .path()
            .join("downloads/active.bin.part")
            .exists()
    );
    client::request_at(&service.paths, service.add(&http.base, "shutdown.bin"))
        .await
        .unwrap();
    service
        .until(|s| s.jobs.last().unwrap().progress.downloaded > 0)
        .await;
    service.shutdown().await;
    assert!(
        service
            .root
            .path()
            .join("downloads/shutdown.bin.part")
            .exists()
    );
    assert!(!service.root.path().join("downloads/shutdown.bin").exists());
}

#[tokio::test]
async fn crash_disconnect_and_restart_replace_generation_not_history() {
    let http = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let old = service.ready().await;
    client::request_at(&service.paths, service.add(&http.base, "crash.bin"))
        .await
        .unwrap();
    service.until(|s| s.jobs[0].progress.downloaded > 0).await;
    let mut feed = client::subscribe_at(&service.paths).await.unwrap();
    service.child.kill().unwrap();
    service.child.wait().unwrap();
    timeout(Duration::from_secs(3), async {
        while feed.next().await.is_ok() {}
    })
    .await
    .unwrap();
    assert!(
        service.paths.socket.exists(),
        "crash should leave a stale socket to recover"
    );
    service.child = Service::command(&service.root).spawn().unwrap();
    let new = service.ready().await;
    assert_ne!(new.instance, old.instance);
    assert!(
        new.jobs.is_empty(),
        "do not invent persisted history after a crash"
    );
    let error = client::request_at(
        &service.paths,
        Request::Cancel {
            instance: old.instance,
            id: 1,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        client::Error::Rejected(ErrorCode::StaleInstance, _)
    ));
    assert!(
        service
            .root
            .path()
            .join("downloads/crash.bin.part")
            .exists()
    );
    service.shutdown().await;
}

#[tokio::test]
async fn private_permissions_versions_bad_frames_and_dead_subscribers() {
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    assert_eq!(
        fs::metadata(&service.paths.directory).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&service.paths.socket).unwrap().mode() & 0o777,
        0o600
    );
    let mut stream = tokio::net::UnixStream::connect(&service.paths.socket)
        .await
        .unwrap();
    wire::write(
        &mut stream,
        &Packet {
            version: 999,
            body: Request::List,
        },
    )
    .await
    .unwrap();
    let response: Packet<Response> = wire::read(&mut stream).await.unwrap();
    assert!(matches!(
        response.body,
        Response::Error {
            code: ErrorCode::Version,
            ..
        }
    ));
    drop(stream);
    let mut stream = tokio::net::UnixStream::connect(&service.paths.socket)
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    stream
        .write_u32((rdm_ipc::MAX_FRAME + 1) as u32)
        .await
        .unwrap();
    let response: Result<Packet<Response>, _> = wire::read(&mut stream).await;
    assert!(response.is_err());
    for _ in 0..70 {
        let feed = client::subscribe_at(&service.paths).await.unwrap();
        drop(feed);
        sleep(Duration::from_millis(2)).await;
    }
    fs::set_permissions(&service.paths.socket, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        client::request_at(&service.paths, Request::List)
            .await
            .unwrap_err(),
        client::Error::Runtime
    );
    fs::set_permissions(&service.paths.socket, fs::Permissions::from_mode(0o600)).unwrap();
    service.shutdown().await;
}

#[tokio::test]
async fn invalid_inputs_do_not_create_jobs_or_expose_secrets() {
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    for (url, filename) in [
        ("https://u:secret@example.com/file", "file"),
        ("https://example.com/file?key=secret", "../escape"),
        ("https://mega.nz/file/id#secret", "file"),
    ] {
        let error = client::request_at(
            &service.paths,
            Request::Add(Add {
                url: url.into(),
                directory: "/tmp".into(),
                filename: filename.into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            client::Error::Rejected(ErrorCode::Invalid, _)
        ));
        assert!(!error.to_string().contains("secret"));
    }
    assert!(service.ready().await.jobs.is_empty());
    service.shutdown().await;
}

#[tokio::test]
async fn terminal_records_are_bounded_and_evict_oldest() {
    let http = http(true).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    let downloads = service.root.path().join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    fs::write(downloads.join("keep.bin"), b"unchanged").unwrap();
    for id in 1..=(rdm_ipc::MAX_JOBS as u64 + 2) {
        client::request_at(&service.paths, service.add(&http.base, "keep.bin"))
            .await
            .unwrap();
        service
            .until(|s| s.jobs.last().is_some_and(|j| j.id == id && !j.active()))
            .await;
    }
    let snapshot = service.ready().await;
    assert_eq!(snapshot.jobs.len(), rdm_ipc::MAX_JOBS);
    assert_eq!(snapshot.jobs[0].id, 3);
    assert!(
        snapshot
            .jobs
            .iter()
            .all(|j| matches!(j.result, Some(Finished::AlreadyPresent { .. })))
    );
    assert_eq!(fs::read(downloads.join("keep.bin")).unwrap(), b"unchanged");
    service.shutdown().await;
}

#[tokio::test]
async fn sigterm_cancels_and_drains_active_work() {
    let http = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    client::request_at(&service.paths, service.add(&http.base, "signal.bin"))
        .await
        .unwrap();
    service.until(|s| s.jobs[0].progress.downloaded > 0).await;
    assert!(
        Command::new("kill")
            .arg("-TERM")
            .arg(service.child.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    service.exit().await;
    assert!(
        service
            .root
            .path()
            .join("downloads/signal.bin.part")
            .exists()
    );
    assert!(!service.root.path().join("downloads/signal.bin").exists());
}
