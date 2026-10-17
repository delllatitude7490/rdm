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
            .env("XDG_STATE_HOME", root.path().join("state"))
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
    // Other inputs are now queued, not rejected; remove this one so the rest
    // of this ownership/drain test can focus on its original active transfer.
    let queued = client::request_at(&service.paths, service.add(&http.base, "other.bin"))
        .await
        .unwrap();
    let other = queued.jobs.last().unwrap();
    assert!(other.queued);
    client::request_at(
        &service.paths,
        Request::RemoveQueued {
            instance: queued.instance,
            id: other.id,
        },
    )
    .await
    .unwrap();
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
async fn crash_disconnect_and_restart_restore_interrupted_with_new_generation() {
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
    assert_eq!(new.jobs.len(), 1);
    assert_eq!(new.jobs[0].result, Some(Finished::Interrupted));
    assert!(new.jobs[0].retryable);
    assert!(!new.jobs[0].active());
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

#[tokio::test]
async fn fifo_cancel_queued_then_cancel_running_starts_next_after_drain() {
    let blocked = http(false).await;
    let finite = http(true).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let initial = service.ready().await;
    client::request_at(&service.paths, service.add(&blocked.base, "first.bin"))
        .await
        .unwrap();
    service.until(|s| s.jobs[0].progress.downloaded > 0).await;
    client::request_at(&service.paths, service.add(&finite.base, "second.bin"))
        .await
        .unwrap();
    let s = client::request_at(&service.paths, service.add(&finite.base, "third.bin"))
        .await
        .unwrap();
    assert_eq!(s.jobs.iter().filter(|j| j.running()).count(), 1);
    assert!(!s.jobs[0].queued && s.jobs[1].queued && s.jobs[2].queued);
    assert_eq!(s.jobs[1].progress.downloaded, 0);
    assert!(!service.root.path().join("downloads/second.bin").exists());
    assert!(
        !service
            .root
            .path()
            .join("downloads/second.bin.part")
            .exists()
    );
    let error = client::request_at(
        &service.paths,
        Request::RemoveQueued {
            instance: initial.instance.clone(),
            id: s.jobs[0].id,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        client::Error::Rejected(ErrorCode::Invalid, _)
    ));
    let error = client::request_at(
        &service.paths,
        Request::RemoveQueued {
            instance: "stale".into(),
            id: s.jobs[2].id,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        client::Error::Rejected(ErrorCode::StaleInstance, _)
    ));
    let removed = client::request_at(
        &service.paths,
        Request::RemoveQueued {
            instance: initial.instance.clone(),
            id: s.jobs[2].id,
        },
    )
    .await
    .unwrap();
    assert_eq!(removed.jobs[2].result, Some(Finished::Cancelled));
    assert!(!removed.jobs[2].queued);
    client::request_at(
        &service.paths,
        Request::Cancel {
            instance: initial.instance,
            id: s.jobs[0].id,
        },
    )
    .await
    .unwrap();
    let done = service.until(|s| s.jobs[1].result.is_some()).await;
    assert_eq!(done.jobs[0].result, Some(Finished::Cancelled));
    assert!(matches!(
        done.jobs[1].result,
        Some(Finished::Completed { bytes: 30720, .. })
    ));
    assert_eq!(done.jobs[2].result, Some(Finished::Cancelled));
    assert_eq!(
        fs::read(service.root.path().join("downloads/second.bin")).unwrap(),
        vec![42; 30720]
    );
    assert!(
        !service
            .root
            .path()
            .join("downloads/third.bin.part")
            .exists()
    );
    service.shutdown().await;
}

#[tokio::test]
async fn queue_preserves_fifo_through_completion_failure_and_existing_output() {
    let finite = http(true).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    client::request_at(&service.paths, service.add(&finite.base, "first.bin"))
        .await
        .unwrap();
    // A 404 is validated and queued, then fails only when its turn arrives.
    client::request_at(
        &service.paths,
        Request::Add(Add {
            url: format!("{}/missing.bin", finite.base),
            directory: service
                .root
                .path()
                .join("downloads")
                .to_string_lossy()
                .into(),
            filename: "missing.bin".into(),
        }),
    )
    .await
    .unwrap();
    // Reuse the first output: must not corrupt it when this queued item starts.
    client::request_at(&service.paths, service.add(&finite.base, "first.bin"))
        .await
        .unwrap();
    client::request_at(&service.paths, service.add(&finite.base, "last.bin"))
        .await
        .unwrap();
    let done = service
        .until(|s| {
            assert!(s.jobs.iter().filter(|j| j.running()).count() <= 1);
            // No later job may start while an earlier FIFO job is unfinished.
            let mut unfinished = false;
            for job in &s.jobs {
                if unfinished {
                    assert!(!job.running());
                }
                unfinished |= job.active();
            }
            s.jobs.len() == 4 && s.jobs[3].result.is_some()
        })
        .await;
    assert!(matches!(
        done.jobs[0].result,
        Some(Finished::Completed { .. })
    ));
    assert!(matches!(done.jobs[1].result, Some(Finished::Failed(_))));
    assert!(matches!(
        done.jobs[2].result,
        Some(Finished::AlreadyPresent { .. })
    ));
    assert!(matches!(
        done.jobs[3].result,
        Some(Finished::Completed { .. })
    ));
    assert_eq!(
        fs::read(service.root.path().join("downloads/first.bin")).unwrap(),
        vec![42; 30720]
    );
    service.shutdown().await;
}

#[tokio::test]
async fn full_queue_never_evicts_unfinished_work_and_shutdown_discards_pending() {
    let blocked = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let initial = service.ready().await;
    for n in 0..64 {
        client::request_at(
            &service.paths,
            service.add(&blocked.base, &format!("file-{n}.bin")),
        )
        .await
        .unwrap();
    }
    let s = service.ready().await;
    assert_eq!(s.jobs.len(), 64);
    assert_eq!(s.jobs.iter().filter(|j| j.running()).count(), 1);
    assert_eq!(s.jobs.iter().filter(|j| j.queued).count(), 63);
    let error = client::request_at(&service.paths, service.add(&blocked.base, "overflow.bin"))
        .await
        .unwrap_err();
    assert!(matches!(error, client::Error::Rejected(ErrorCode::Busy, _)));
    assert_eq!(service.ready().await.jobs[0].id, s.jobs[0].id);
    client::request_at(
        &service.paths,
        Request::RemoveQueued {
            instance: initial.instance.clone(),
            id: s.jobs[20].id,
        },
    )
    .await
    .unwrap();
    let after = client::request_at(
        &service.paths,
        service.add(&blocked.base, "replacement.bin"),
    )
    .await
    .unwrap();
    assert_eq!(after.jobs.len(), 64);
    assert_eq!(after.jobs[0].id, s.jobs[0].id);
    assert!(!after.jobs.iter().any(|j| j.id == s.jobs[20].id));
    let stopping = client::request_at(
        &service.paths,
        Request::Shutdown {
            instance: initial.instance,
        },
    )
    .await
    .unwrap();
    assert!(stopping.shutting_down);
    assert!(!stopping.jobs.iter().any(|j| j.queued));
    assert!(
        stopping
            .jobs
            .iter()
            .skip(1)
            .all(|j| j.result == Some(Finished::Cancelled))
    );
    service.exit().await;
    // Queued work must never touch its files, including on shutdown.
    for n in 1..64 {
        assert!(
            !service
                .root
                .path()
                .join(format!("downloads/file-{n}.bin.part"))
                .exists()
        );
    }
    assert!(
        !service
            .root
            .path()
            .join("downloads/replacement.bin.part")
            .exists()
    );
}

#[tokio::test]
async fn recovery_holds_pending_and_retry_creates_safe_distinct_attempt() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let observed = hits.clone();
    let router = Router::new().route(
        "/file.bin",
        get(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            async { "durable bytes" }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let blocked = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let old = service.ready().await;
    client::request_at(&service.paths, service.add(&blocked.base, "active.bin"))
        .await
        .unwrap();
    service.until(|s| s.jobs[0].progress.downloaded > 0).await;
    client::request_at(&service.paths, service.add(&base, "held.bin"))
        .await
        .unwrap();
    client::request_at(&service.paths, service.add(&base, "held-two.bin"))
        .await
        .unwrap();
    service.child.kill().unwrap();
    service.child.wait().unwrap();
    service.child = Service::command(&service.root).spawn().unwrap();
    let recovered = service.ready().await;
    assert_ne!(old.instance, recovered.instance);
    assert_eq!(
        recovered.jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(
        recovered
            .jobs
            .iter()
            .all(|j| j.result == Some(Finished::Interrupted) && j.retryable)
    );
    sleep(Duration::from_millis(400)).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "recovery must not issue even HEAD requests"
    );
    assert!(!service.root.path().join("downloads/held.bin.part").exists());
    let denied = client::request_at(
        &service.paths,
        Request::Retry {
            instance: old.instance,
            id: 4,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied,
        client::Error::Rejected(ErrorCode::StaleInstance, _)
    ));
    let retry = client::request_at(
        &service.paths,
        Request::Retry {
            instance: recovered.instance.clone(),
            id: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(retry.jobs.last().unwrap().id, 4);
    let done = service
        .until(|s| {
            matches!(
                s.jobs.last().unwrap().result,
                Some(Finished::Completed { .. })
            )
        })
        .await;
    assert_eq!(
        done.jobs[1].result,
        Some(Finished::Interrupted),
        "other pending work remains held"
    );
    assert!(!done.jobs[3].retryable, "successful URLs are purged");
    assert_eq!(
        fs::read(service.root.path().join("downloads/held.bin")).unwrap(),
        b"durable bytes"
    );
    assert!(!service.root.path().join("downloads/held-two.bin").exists());
    let denied = client::request_at(
        &service.paths,
        Request::Retry {
            instance: done.instance,
            id: 4,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied,
        client::Error::Rejected(ErrorCode::Invalid, _)
    ));
    service.shutdown().await;
    service.child = Service::command(&service.root).spawn().unwrap();
    let again = service.ready().await;
    assert_eq!(again.jobs.len(), 4);
    assert!(matches!(
        again.jobs[3].result,
        Some(Finished::Completed { bytes: 13, .. })
    ));
    assert_eq!(again.jobs[2].result, Some(Finished::Interrupted));
    service.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn shutdown_history_and_next_id_survive_runtime_directory_recreation() {
    let blocked = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    client::request_at(&service.paths, service.add(&blocked.base, "stop.bin"))
        .await
        .unwrap();
    client::request_at(&service.paths, service.add(&blocked.base, "pending.bin"))
        .await
        .unwrap();
    service.shutdown().await;
    fs::remove_dir_all(service.root.path().join("runtime")).unwrap();
    fs::create_dir(service.root.path().join("runtime")).unwrap();
    fs::set_permissions(
        service.root.path().join("runtime"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    service.child = Service::command(&service.root).spawn().unwrap();
    let state = service.ready().await;
    assert_eq!(state.jobs.len(), 2);
    assert!(
        state
            .jobs
            .iter()
            .all(|j| j.result == Some(Finished::Cancelled) && j.retryable)
    );
    let retry = client::request_at(
        &service.paths,
        Request::Retry {
            instance: state.instance,
            id: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(retry.jobs.last().unwrap().id, 3);
    assert_eq!(
        retry.jobs.last().unwrap().destination,
        service
            .root
            .path()
            .join("downloads/pending.bin")
            .to_string_lossy()
    );
    service.shutdown().await;
}

#[tokio::test]
async fn failed_admission_commit_stops_without_network_or_reset() {
    let remote = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    let state_dir = service.root.path().join("state/rdm/service");
    // Directory obstruction works even in containers with CAP_DAC_OVERRIDE.
    let journal = state_dir.join("jobs.sqlite3-journal");
    fs::create_dir(&journal).unwrap();
    let result =
        client::request_at(&service.paths, service.add(&remote.base, "not-started.bin")).await;
    fs::remove_dir(&journal).unwrap();
    assert!(matches!(
        result.unwrap_err(),
        client::Error::Rejected(ErrorCode::Internal, _)
    ));
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = service.child.try_wait().unwrap() {
                assert!(
                    !status.success(),
                    "failed storage must not look like normal shutdown"
                );
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !service.root.path().join("downloads").exists(),
        "engine cannot start before durable admission"
    );
    service.child = Service::command(&service.root).spawn().unwrap();
    assert!(
        service.ready().await.jobs.is_empty(),
        "last committed image preserved"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn held_recovery_at_capacity_is_not_evicted_and_retry_still_works() {
    let blocked = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    for n in 0..64 {
        client::request_at(
            &service.paths,
            service.add(&blocked.base, &format!("held-{n}.bin")),
        )
        .await
        .unwrap();
    }
    service.child.kill().unwrap();
    service.child.wait().unwrap();
    service.child = Service::command(&service.root).spawn().unwrap();
    let recovered = service.ready().await;
    assert_eq!(recovered.jobs.len(), 64);
    assert!(
        recovered
            .jobs
            .iter()
            .all(|j| j.result == Some(Finished::Interrupted) && j.retryable)
    );
    let denied = client::request_at(&service.paths, service.add(&blocked.base, "no-room.bin"))
        .await
        .unwrap_err();
    assert!(matches!(
        denied,
        client::Error::Rejected(ErrorCode::Busy, _)
    ));
    let retry = client::request_at(
        &service.paths,
        Request::Retry {
            instance: recovered.instance.clone(),
            id: 20,
        },
    )
    .await
    .unwrap();
    assert_eq!(retry.jobs.len(), 64);
    assert_eq!(retry.jobs.last().unwrap().id, 65);
    assert!(!retry.jobs.iter().any(|j| j.id == 20));
    assert_eq!(
        retry
            .jobs
            .iter()
            .filter(|j| j.result == Some(Finished::Interrupted))
            .count(),
        63
    );
    let stale = client::request_at(
        &service.paths,
        Request::Cancel {
            instance: recovered.instance.clone(),
            id: 20,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        stale,
        client::Error::Rejected(ErrorCode::NotFound, _)
    ));
    let removed = client::request_at(
        &service.paths,
        Request::RemoveQueued {
            instance: recovered.instance,
            id: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(removed.jobs[0].result, Some(Finished::Cancelled));
    service.shutdown().await;
}

#[tokio::test]
async fn failed_cancel_commit_drains_without_promoting_pending_work() {
    let blocked = http(false).await;
    let finite = http(true).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let initial = service.ready().await;
    client::request_at(&service.paths, service.add(&blocked.base, "active.bin"))
        .await
        .unwrap();
    service.until(|s| s.jobs[0].progress.downloaded > 0).await;
    client::request_at(&service.paths, service.add(&finite.base, "untouched.bin"))
        .await
        .unwrap();
    let journal = service
        .root
        .path()
        .join("state/rdm/service/jobs.sqlite3-journal");
    fs::create_dir(&journal).unwrap();
    let error = client::request_at(
        &service.paths,
        Request::Cancel {
            instance: initial.instance,
            id: 1,
        },
    )
    .await
    .unwrap_err();
    fs::remove_dir(&journal).unwrap();
    assert!(matches!(
        error,
        client::Error::Rejected(ErrorCode::Internal, _)
    ));
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = service.child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !service
            .root
            .path()
            .join("downloads/untouched.bin.part")
            .exists()
    );
    assert!(!service.root.path().join("downloads/untouched.bin").exists());
    service.child = Service::command(&service.root).spawn().unwrap();
    let recovered = service.ready().await;
    assert_eq!(recovered.jobs.len(), 2);
    assert!(
        recovered
            .jobs
            .iter()
            .all(|j| j.result == Some(Finished::Interrupted))
    );
    service.shutdown().await;
}

#[tokio::test]
async fn retry_revalidates_current_service_network_policy_without_replaying_env() {
    let blocked = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    // GUI Add had no per-job private flag: its old ambient service opt-in must
    // NOT become a permanently stored per-job permission during recovery.
    client::request_at(&service.paths, service.add(&blocked.base, "permission.bin"))
        .await
        .unwrap();
    service.child.kill().unwrap();
    service.child.wait().unwrap();
    service.child = Service::command(&service.root)
        .env_remove("RDM_ALLOW_PRIVATE")
        .spawn()
        .unwrap();
    let recovered = service.ready().await;
    let error = client::request_at(
        &service.paths,
        Request::Retry {
            instance: recovered.instance,
            id: 1,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        client::Error::Rejected(ErrorCode::Invalid, _)
    ));
    let held = service.ready().await;
    assert_eq!(held.jobs.len(), 1);
    assert_eq!(held.jobs[0].result, Some(Finished::Interrupted));
    assert!(held.jobs[0].retryable);
    assert!(held.jobs[0].retried_as.is_none());
    service.shutdown().await;
}
#[tokio::test]
async fn failed_pause_commit_drains_without_publishing_paused_or_promoting() {
    let blocked = http(false).await;
    let finite = http(true).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let initial = service.ready().await;
    client::request_at(&service.paths, service.add(&blocked.base, "active.bin"))
        .await
        .unwrap();
    service.until(|s| s.jobs[0].progress.downloaded > 0).await;
    client::request_at(&service.paths, service.add(&finite.base, "untouched.bin"))
        .await
        .unwrap();
    let journal = service
        .root
        .path()
        .join("state/rdm/service/jobs.sqlite3-journal");
    fs::create_dir(&journal).unwrap();
    let error = client::request_at(
        &service.paths,
        Request::Pause {
            instance: initial.instance,
            id: 1,
        },
    )
    .await
    .unwrap_err();
    fs::remove_dir(&journal).unwrap();
    assert!(matches!(
        error,
        client::Error::Rejected(ErrorCode::Internal, _)
    ));
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = service.child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !service
            .root
            .path()
            .join("downloads/untouched.bin.part")
            .exists()
    );
    assert!(!service.root.path().join("downloads/untouched.bin").exists());
    service.child = Service::command(&service.root).spawn().unwrap();
    let recovered = service.ready().await;
    assert_eq!(recovered.jobs.len(), 2);
    assert!(
        recovered
            .jobs
            .iter()
            .all(|j| j.result == Some(Finished::Interrupted))
    );
    service.shutdown().await;
}

#[tokio::test]
async fn pause_drains_before_next_worker_and_held_jobs_survive_restart() {
    let blocked = http(false).await;
    let finite = http(true).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let original = service.ready().await;
    client::request_at(&service.paths, service.add(&blocked.base, "pause.bin"))
        .await
        .unwrap();
    service.until(|s| s.jobs[0].progress.downloaded > 0).await;
    client::request_at(&service.paths, service.add(&finite.base, "next.bin"))
        .await
        .unwrap();
    client::request_at(&service.paths, service.add(&finite.base, "held.bin"))
        .await
        .unwrap();
    let held = client::request_at(
        &service.paths,
        Request::Pause {
            instance: original.instance.clone(),
            id: 3,
        },
    )
    .await
    .unwrap();
    assert_eq!(held.jobs[2].result, Some(Finished::Paused));
    assert!(held.jobs[2].held());
    assert!(!service.root.path().join("downloads/held.bin.part").exists());
    let requested = client::request_at(
        &service.paths,
        Request::Pause {
            instance: original.instance.clone(),
            id: 1,
        },
    )
    .await
    .unwrap();
    assert!(requested.jobs[0].pausing || requested.jobs[0].result == Some(Finished::Paused));
    let next = service.until(|s| s.jobs[1].result.is_some()).await;
    assert_eq!(next.jobs[0].result, Some(Finished::Paused));
    assert!(!next.jobs[0].pausing);
    assert!(next.jobs[0].held());
    assert!(matches!(
        next.jobs[1].result,
        Some(Finished::Completed { .. })
    ));
    let part = service.root.path().join("downloads/pause.bin.part");
    let size = fs::metadata(&part).unwrap().len();
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fs::metadata(&part).unwrap().len(),
        size,
        "worker fully drained"
    );
    // Idempotent hold; don't create a new attempt from repeated Pause.
    let repeated = client::request_at(
        &service.paths,
        Request::Pause {
            instance: original.instance.clone(),
            id: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(repeated.jobs.len(), 3);
    let denied = client::request_at(
        &service.paths,
        Request::Pause {
            instance: original.instance.clone(),
            id: 2,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied,
        client::Error::Rejected(ErrorCode::Invalid, _)
    ));
    service.shutdown().await;
    service.child = Service::command(&service.root).spawn().unwrap();
    let recovered = service.ready().await;
    assert!(recovered.jobs[0].held() && recovered.jobs[2].held());
    assert_eq!(recovered.jobs[0].result, Some(Finished::Paused));
    sleep(Duration::from_millis(200)).await;
    assert!(!service.ready().await.jobs.iter().any(|j| j.active()));
    assert!(matches!(
        client::request_at(
            &service.paths,
            Request::Pause {
                instance: original.instance,
                id: 1
            }
        )
        .await
        .unwrap_err(),
        client::Error::Rejected(ErrorCode::StaleInstance, _)
    ));
    let resumed = client::request_at(
        &service.paths,
        Request::Retry {
            instance: recovered.instance.clone(),
            id: 3,
        },
    )
    .await
    .unwrap();
    assert_eq!(resumed.jobs.last().unwrap().id, 4);
    assert_eq!(resumed.jobs[2].retried_as, Some(4));
    service
        .until(|s| s.jobs.last().unwrap().result.is_some())
        .await;
    assert_eq!(
        fs::read(service.root.path().join("downloads/held.bin")).unwrap(),
        vec![42; 30720]
    );
    let removed = client::request_at(
        &service.paths,
        Request::RemoveQueued {
            instance: recovered.instance,
            id: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(removed.jobs[0].result, Some(Finished::Cancelled));
    service.shutdown().await;
}

#[tokio::test]
async fn paused_capacity_is_protected_and_resume_can_retire_its_own_record() {
    let blocked = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    let s = service.ready().await;
    for n in 0..64 {
        let added = client::request_at(
            &service.paths,
            service.add(&blocked.base, &format!("pause-{n}.bin")),
        )
        .await
        .unwrap();
        let id = added.jobs.last().unwrap().id;
        client::request_at(
            &service.paths,
            Request::Pause {
                instance: s.instance.clone(),
                id,
            },
        )
        .await
        .unwrap();
        service
            .until(|s| s.jobs.last().unwrap().result == Some(Finished::Paused))
            .await;
    }
    assert!(matches!(
        client::request_at(&service.paths, service.add(&blocked.base, "overflow.bin"))
            .await
            .unwrap_err(),
        client::Error::Rejected(ErrorCode::Busy, _)
    ));
    let resumed = client::request_at(
        &service.paths,
        Request::Retry {
            instance: s.instance,
            id: 32,
        },
    )
    .await
    .unwrap();
    assert_eq!(resumed.jobs.len(), 64);
    assert_eq!(resumed.jobs.last().unwrap().id, 65);
    assert_eq!(resumed.jobs.iter().filter(|j| j.held()).count(), 63);
    assert!(!resumed.jobs.iter().any(|j| j.id == 32));
    service.shutdown().await;
}

#[tokio::test]
async fn recovery_honours_committed_pause_intent_without_replaying_it() {
    let blocked = http(false).await;
    let mut service = Service::spawn(tempfile::tempdir().unwrap());
    service.ready().await;
    client::request_at(&service.paths, service.add(&blocked.base, "intent.bin"))
        .await
        .unwrap();
    service.child.kill().unwrap();
    service.child.wait().unwrap();
    // Deterministically simulate a crash between the committed intent and
    // worker drain. Editing is only for this isolated, stopped test service.
    let db = rusqlite::Connection::open(service.root.path().join("state/rdm/service/jobs.sqlite3"))
        .unwrap();
    let payload: String = db
        .query_row("SELECT payload FROM state WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let mut image: serde_json::Value = serde_json::from_str(&payload).unwrap();
    image["records"][0]["job"]["pausing"] = true.into();
    db.execute(
        "UPDATE state SET payload=?1 WHERE id=1",
        [image.to_string()],
    )
    .unwrap();
    drop(db);
    service.child = Service::command(&service.root).spawn().unwrap();
    let s = service.ready().await;
    assert_eq!(s.jobs[0].result, Some(Finished::Paused));
    assert!(!s.jobs[0].pausing);
    assert!(s.jobs[0].held());
    assert!(!s.jobs[0].active());
    service.shutdown().await;
}
