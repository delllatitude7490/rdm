//! A single actor owns all managed jobs. Socket handlers cannot mutate job state.
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::store::{self, Image, Record, Store};
use crate::transfer::{self, GuiSink};
use rdm::config::Config;
use rdm_ipc::{ErrorCode, Finished, Job, MAX_JOBS, Packet, Request, Response, Snapshot, VERSION};
use rdm_ipc::{
    runtime::{Instance, Paths, uid},
    wire,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct Managed {
    job: Job,
    sink: GuiSink,
    cancel: CancellationToken,
    // URL-bearing input is private: only the owner-only store may serialize it.
    input: Option<rdm_ipc::AddCli>,
}
struct Command {
    request: Request,
    reply: oneshot::Sender<Response>,
}
struct Manager {
    config: Config,
    instance: String,
    revision: u64,
    next_id: u64,
    stopping: bool,
    jobs: VecDeque<Managed>,
    store: Store,
    failed: bool,
}

fn error(code: ErrorCode, message: &str) -> Response {
    Response::Error {
        code,
        message: message.into(),
    }
}

impl Manager {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            instance: self.instance.clone(),
            revision: self.revision,
            default_directory: self.config.download_dir.clone(),
            shutting_down: self.stopping,
            jobs: self.jobs.iter().map(|m| m.job.clone()).collect(),
        }
    }
    fn publish(&mut self, events: &watch::Sender<Snapshot>) {
        self.revision += 1;
        events.send_replace(self.snapshot());
    }
    fn image(&self) -> Image {
        Image {
            next_id: self.next_id,
            records: self
                .jobs
                .iter()
                .map(|m| Record {
                    job: m.job.clone(),
                    input: m.input.clone(),
                })
                .collect(),
        }
    }
    async fn persist(&self) -> io::Result<()> {
        self.store.save(self.image()).await
    }
    // A failed commit must never acknowledge success or schedule more work.
    // Leave the last durable image intact for conservative recovery.
    fn fail_closed(&mut self, events: &watch::Sender<Snapshot>) {
        self.failed = true;
        self.stopping = true;
        for m in &mut self.jobs {
            m.cancel.cancel();
        }
        // Publish only the shutdown flag on the LAST durable snapshot, not
        // uncommitted jobs/results from a failed transaction.
        let mut snapshot = events.borrow().clone();
        snapshot.shutting_down = true;
        self.revision += 1;
        snapshot.revision = self.revision;
        events.send_replace(snapshot);
    }
    async fn stop(&mut self, events: &watch::Sender<Snapshot>) -> io::Result<()> {
        self.stopping = true;
        for managed in &mut self.jobs {
            if managed.job.queued {
                managed.job.queued = false;
                managed.job.result = Some(Finished::Cancelled);
                managed.job.retryable = managed.input.is_some();
            } else if managed.job.running() {
                if !managed.job.pausing {
                    managed.job.cancelling = true;
                }
                managed.cancel.cancel();
            }
        }
        self.persist().await?;
        self.publish(events);
        Ok(())
    }
    fn refresh(&mut self, events: &watch::Sender<Snapshot>) {
        for managed in &mut self.jobs {
            if managed.job.running() {
                managed.job.progress = managed.sink.snapshot();
            }
        }
        // Progress is coalesced in memory, not fsynced four times per second.
        self.publish(events);
    }
    async fn finish(
        &mut self,
        id: u64,
        result: Finished,
        events: &watch::Sender<Snapshot>,
    ) -> io::Result<()> {
        if let Some(managed) = self.jobs.iter_mut().find(|m| m.job.id == id) {
            // Only a cancelled, fully joined worker confirms a pause. A racing
            // completion/failure remains truthful rather than being relabelled.
            let result = if managed.job.pausing && result == Finished::Cancelled {
                Finished::Paused
            } else {
                result
            };
            managed.job.pausing = false;
            managed.job.progress = managed.sink.snapshot();
            if let Finished::Completed { bytes, .. } = &result {
                managed.job.progress.downloaded = *bytes;
                managed.job.progress.total = Some(*bytes);
            }
            if matches!(
                result,
                Finished::Completed { .. } | Finished::AlreadyPresent { .. }
            ) {
                // No reason to retain signed URLs for successful history.
                managed.input = None;
            }
            managed.job.queued = false;
            managed.job.result = Some(result);
            managed.job.retryable = managed.input.is_some();
            managed.job.cancelling = false;
            self.persist().await?;
            self.publish(events);
        }
        Ok(())
    }
    async fn add_input(
        &mut self,
        input: transfer::Input,
        workers: &mut JoinSet<(u64, Finished)>,
        events: &watch::Sender<Snapshot>,
    ) -> io::Result<Response> {
        let id = self.next_id;
        let Some(next) = id.checked_add(1) else {
            return Ok(error(ErrorCode::Internal, "Job ID limit reached."));
        };
        if self.jobs.len() >= MAX_JOBS {
            let Some(index) = self
                .jobs
                .iter()
                .position(|m| !m.job.active() && !m.job.held())
            else {
                return Ok(error(
                    ErrorCode::Busy,
                    "The service queue is full (64 unfinished or held jobs). Remove a waiting job or resume held work.",
                ));
            };
            self.jobs.remove(index);
        }
        self.next_id = next;
        let destination = input.request.output.clone().unwrap_or_default();
        let stored = rdm_ipc::AddCli {
            url: input.request.url,
            output: destination.clone(),
            connections: input.request.connections,
            allow_private: input.request.allow_private,
        };
        self.jobs.push_back(Managed {
            job: Job {
                id,
                label: input.label,
                destination,
                progress: Default::default(),
                queued: true,
                retryable: false,
                retried_as: None,
                pausing: false,
                cancelling: false,
                result: None,
            },
            sink: GuiSink::default(),
            cancel: CancellationToken::new(),
            input: Some(stored),
        });
        // Admission is durable BEFORE engine creation or acceptance acknowledgement.
        self.persist().await?;
        self.start_next(workers).await?;
        self.publish(events);
        Ok(Response::Snapshot(self.snapshot()))
    }
    async fn add(
        &mut self,
        request: Request,
        workers: &mut JoinSet<(u64, Finished)>,
        events: &watch::Sender<Snapshot>,
    ) -> io::Result<Response> {
        if self.stopping {
            return Ok(error(
                ErrorCode::ShuttingDown,
                "The service is shutting down.",
            ));
        }
        let prepared = match request {
            Request::Add(add) => transfer::prepare(
                &add.url,
                &add.directory,
                &add.filename,
                self.config.connections,
            ),
            Request::AddCli(add) => transfer::prepare_cli(&add),
            _ => unreachable!("only add requests reach admission"),
        };
        match prepared {
            Ok(input) => self.add_input(input, workers, events).await,
            Err(message) => Ok(Response::Error {
                code: ErrorCode::Invalid,
                message,
            }),
        }
    }
    /// Persist the running transition BEFORE moving input into an engine. A
    /// joined worker is required before the next starts, including cancellation.
    async fn start_next(&mut self, workers: &mut JoinSet<(u64, Finished)>) -> io::Result<()> {
        if self.stopping || !workers.is_empty() {
            return Ok(());
        }
        while let Some(index) = self.jobs.iter().position(|m| m.job.queued) {
            let managed = &mut self.jobs[index];
            let prepared = transfer::prepare_cli(managed.input.as_ref().expect("queued input"));
            managed.job.queued = false;
            let input = match prepared {
                Ok(input) => input,
                Err(message) => {
                    managed.job.result = Some(Finished::Failed(message));
                    managed.job.retryable = true;
                    self.persist().await?;
                    continue;
                }
            };
            let id = managed.job.id;
            let sink = managed.sink.clone();
            let cancel = managed.cancel.clone();
            self.persist().await?;
            workers.spawn(async move { (id, transfer::download(input, sink, cancel).await) });
            break;
        }
        Ok(())
    }
    async fn command(
        &mut self,
        request: Request,
        workers: &mut JoinSet<(u64, Finished)>,
        events: &watch::Sender<Snapshot>,
    ) -> io::Result<Response> {
        if self.failed {
            return Ok(error(
                ErrorCode::Internal,
                "Durable state write failed. Service is stopping; inspect after recovery before retrying.",
            ));
        }
        match request {
            Request::List | Request::Subscribe => Ok(Response::Snapshot(self.snapshot())),
            request @ (Request::Add(_) | Request::AddCli(_)) => {
                self.add(request, workers, events).await
            }
            Request::Retry { instance, id } => {
                if instance != self.instance {
                    return Ok(error(
                        ErrorCode::StaleInstance,
                        "The service restarted. Reconnect before issuing commands.",
                    ));
                }
                if self.stopping {
                    return Ok(error(
                        ErrorCode::ShuttingDown,
                        "The service is shutting down.",
                    ));
                }
                let Some(managed) = self.jobs.iter().find(|m| m.job.id == id) else {
                    return Ok(error(
                        ErrorCode::NotFound,
                        "That download is no longer retained.",
                    ));
                };
                if !managed.job.retryable {
                    return Ok(error(
                        ErrorCode::Invalid,
                        "Only paused, interrupted, cancelled or failed jobs can be resumed/retried.",
                    ));
                }
                // Revalidate under CURRENT network policy; never replay old env.
                let input =
                    match transfer::prepare_cli(managed.input.as_ref().expect("retry input")) {
                        Ok(input) => input,
                        Err(message) => {
                            return Ok(Response::Error {
                                code: ErrorCode::Invalid,
                                message,
                            });
                        }
                    };
                if self.next_id == u64::MAX {
                    return Ok(error(ErrorCode::Internal, "Job ID limit reached."));
                }
                // Retire the old attempt and admit a DISTINCT ID in the same
                // transaction. Old waiters/cancel handles cannot affect the new
                // attempt. At capacity the retired source may be evicted, never
                // another held Interrupted record.
                let next = self.next_id;
                let managed = self
                    .jobs
                    .iter_mut()
                    .find(|m| m.job.id == id)
                    .expect("found job");
                managed.input = None;
                managed.job.retryable = false;
                managed.job.retried_as = Some(next);
                self.add_input(input, workers, events).await
            }
            Request::Pause { instance, id } => {
                if instance != self.instance {
                    return Ok(error(
                        ErrorCode::StaleInstance,
                        "The service restarted. Reconnect before issuing commands.",
                    ));
                }
                if self.stopping {
                    return Ok(error(
                        ErrorCode::ShuttingDown,
                        "The service is shutting down.",
                    ));
                }
                let Some(managed) = self.jobs.iter_mut().find(|m| m.job.id == id) else {
                    return Ok(error(
                        ErrorCode::NotFound,
                        "That download is no longer retained.",
                    ));
                };
                if managed.job.pausing
                    || (managed.job.result == Some(Finished::Paused) && managed.job.retryable)
                {
                    return Ok(Response::Snapshot(self.snapshot()));
                }
                if !managed.job.active() || managed.job.cancelling {
                    return Ok(error(
                        ErrorCode::Invalid,
                        "Only running or queued downloads can be paused; cancellation cannot be undone.",
                    ));
                }
                if managed.job.queued {
                    // No worker or output exists for this job, so holding it is immediate.
                    managed.job.queued = false;
                    managed.job.result = Some(Finished::Paused);
                    managed.job.retryable = true;
                } else {
                    managed.job.pausing = true;
                }
                let cancel = managed.cancel.clone();
                // Commit intent BEFORE signalling. On crash, recovery holds this
                // job as Paused even if the final drain acknowledgement was lost.
                self.persist().await?;
                cancel.cancel();
                self.publish(events);
                Ok(Response::Snapshot(self.snapshot()))
            }
            request @ (Request::Cancel { .. } | Request::RemoveQueued { .. }) => {
                let queued_only = matches!(request, Request::RemoveQueued { .. });
                let (Request::Cancel { instance, id } | Request::RemoveQueued { instance, id }) =
                    request
                else {
                    unreachable!()
                };
                if instance != self.instance {
                    return Ok(error(
                        ErrorCode::StaleInstance,
                        "The service restarted. Reconnect before issuing commands.",
                    ));
                }
                let Some(managed) = self.jobs.iter_mut().find(|m| m.job.id == id) else {
                    return Ok(error(
                        ErrorCode::NotFound,
                        "That download is no longer retained.",
                    ));
                };
                if queued_only && !managed.job.queued && !managed.job.held() {
                    return Ok(error(
                        ErrorCode::Invalid,
                        "Only queued, paused or interrupted jobs can be removed. Use cancel for a running download.",
                    ));
                }
                if managed.job.queued || managed.job.held() {
                    managed.job.queued = false;
                    managed.job.result = Some(Finished::Cancelled);
                    managed.job.retryable = managed.input.is_some();
                } else if managed.job.running() {
                    managed.job.pausing = false;
                    managed.job.cancelling = true;
                    managed.cancel.cancel();
                }
                self.persist().await?;
                self.publish(events);
                Ok(Response::Snapshot(self.snapshot()))
            }
            Request::Shutdown { instance } => {
                if instance != self.instance {
                    return Ok(error(
                        ErrorCode::StaleInstance,
                        "The service restarted. Reconnect before shutting it down.",
                    ));
                }
                self.stop(events).await?;
                Ok(Response::Snapshot(self.snapshot()))
            }
        }
    }
}

async fn write_response(stream: &mut UnixStream, response: Response) -> io::Result<()> {
    timeout(
        Duration::from_secs(2),
        wire::write(stream, &Packet::new(response)),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Slow IPC consumer"))?
}

async fn handle(
    mut stream: UnixStream,
    commands: mpsc::Sender<Command>,
    mut events: watch::Receiver<Snapshot>,
) -> io::Result<()> {
    if stream.peer_cred()?.uid() != uid() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let packet: Packet<Request> = timeout(Duration::from_secs(5), wire::read(&mut stream))
        .await
        .map_err(|_| io::ErrorKind::TimedOut)??;
    if packet.version != VERSION {
        return write_response(
            &mut stream,
            error(ErrorCode::Version, "Unsupported RDM IPC version."),
        )
        .await;
    }
    let subscribe = matches!(packet.body, Request::Subscribe);
    let (reply, response) = oneshot::channel();
    commands
        .send(Command {
            request: packet.body,
            reply,
        })
        .await
        .map_err(|_| io::ErrorKind::BrokenPipe)?;
    let response = response.await.map_err(|_| io::ErrorKind::BrokenPipe)?;
    write_response(&mut stream, response).await?;
    if subscribe {
        loop {
            tokio::select! {
                // A subscriber sends no more commands. Detect disconnection even
                // while the service is idle, so dead feeds release their permit.
                readable = stream.readable() => {
                    readable?;
                    let mut byte = [0; 1];
                    match stream.try_read(&mut byte) {
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                        _ => return Ok(()),
                    }
                }
                changed = events.changed() => {
                    if changed.is_err() { return Ok(()); }
                    let snapshot = events.borrow_and_update().clone();
                    write_response(&mut stream, Response::Snapshot(snapshot)).await?;
                }
            }
        }
    }
    Ok(())
}

/// Never abort worker futures on shutdown: cancel and drain them first.
pub async fn serve(
    paths: Paths,
    config: Config,
    external_stop: CancellationToken,
) -> io::Result<()> {
    let mut instance = Instance::acquire(&paths)?;
    let (store, mut image) = Store::open(store::discover()?).await?;
    // No engine is created during recovery. Every unfinished record becomes a
    // held interrupted result, even if it was merely queued before the crash.
    for record in &mut image.records {
        if record.job.active() {
            record.job.queued = false;
            record.job.cancelling = false;
            record.job.result = Some(if record.job.pausing {
                Finished::Paused
            } else {
                Finished::Interrupted
            });
            record.job.pausing = false;
            record.job.retryable = record.input.is_some();
        }
    }
    store.save(image.clone()).await?;
    let listener = UnixListener::bind(&paths.socket)?;
    instance.bound()?;
    rdm::engine::set_shared_config(config.clone());
    let generation = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut manager = Manager {
        config,
        instance: format!("{}-{generation}", std::process::id()),
        revision: 0,
        next_id: image.next_id,
        stopping: false,
        jobs: image
            .records
            .into_iter()
            .map(|r| Managed {
                job: r.job,
                input: r.input,
                sink: GuiSink::default(),
                cancel: CancellationToken::new(),
            })
            .collect(),
        store,
        failed: false,
    };
    let (events, _) = watch::channel(manager.snapshot());
    let (commands, mut incoming) = mpsc::channel::<Command>(64);
    let permits = Arc::new(Semaphore::new(32));
    let mut clients = JoinSet::new();
    let mut workers = JoinSet::new();
    let mut accept_error = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if manager.stopping && workers.is_empty() {
            break;
        }
        tokio::select! {
            accepted = listener.accept(), if !manager.stopping => {
                match accepted {
                    Ok((stream, _)) => {
                        if let Ok(permit) = Arc::clone(&permits).try_acquire_owned() {
                            let commands = commands.clone();
                            let events = events.subscribe();
                            clients.spawn(async move { let _permit = permit; let _ = handle(stream, commands, events).await; });
                        }
                    }
                    Err(e) => {
                        accept_error = Some(e);
                        if manager.stop(&events).await.is_err() { manager.fail_closed(&events); }
                    }
                }
            }
            Some(command) = incoming.recv() => {
                let response = match manager.command(command.request, &mut workers, &events).await {
                    Ok(response) => response,
                    Err(e) => {
                        accept_error = Some(e);
                        manager.fail_closed(&events);
                        error(ErrorCode::Internal, "Durable state write failed. Service is stopping; inspect after recovery before retrying.")
                    }
                };
                let _ = command.reply.send(response);
            }
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                if !manager.failed {
                    let (id, outcome) = match result {
                        Ok(pair) => pair,
                        Err(_) => {
                            let id = manager.jobs.iter().find(|m| m.job.running()).expect("one running worker").job.id;
                            (id, Finished::Failed("The download worker stopped unexpectedly.".into()))
                        }
                    };
                    let result = async {
                        manager.finish(id, outcome, &events).await?;
                        manager.start_next(&mut workers).await?;
                        manager.publish(&events);
                        Ok::<_, io::Error>(())
                    }.await;
                    if let Err(e) = result { accept_error = Some(e); manager.fail_closed(&events); }
                }
            }
            _ = tick.tick(), if !workers.is_empty() && !manager.failed => manager.refresh(&events),
            _ = external_stop.cancelled(), if !manager.stopping => {
                if let Err(e) = manager.stop(&events).await { accept_error = Some(e); manager.fail_closed(&events); }
            }
            Some(_) = clients.join_next(), if !clients.is_empty() => {},
        }
    }
    // Allow accepted command replies (especially Shutdown) a brief bounded drain.
    // Only socket handlers can be aborted; all transfer workers are already done.
    drop(events);
    let drain = async { while clients.join_next().await.is_some() {} };
    if timeout(Duration::from_secs(3), drain).await.is_err() {
        clients.abort_all();
        while clients.join_next().await.is_some() {}
    }
    drop(listener);
    drop(instance);
    match accept_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod pause_tests {
    use super::*;
    async fn manager() -> (tempfile::TempDir, Manager, watch::Sender<Snapshot>) {
        let root = tempfile::tempdir().unwrap();
        let (store, _) = Store::open(root.path().join("rdm/service")).await.unwrap();
        let manager = Manager {
            config: Config::default(),
            instance: "test".into(),
            revision: 0,
            next_id: 2,
            stopping: false,
            failed: false,
            store,
            jobs: VecDeque::from([Managed {
                job: Job {
                    id: 1,
                    label: "file.bin".into(),
                    destination: "/tmp/file.bin".into(),
                    progress: Default::default(),
                    queued: false,
                    retryable: false,
                    retried_as: None,
                    pausing: true,
                    cancelling: false,
                    result: None,
                },
                sink: GuiSink::default(),
                cancel: CancellationToken::new(),
                input: Some(rdm_ipc::AddCli {
                    url: "https://example.com/file.bin".into(),
                    output: "/tmp/file.bin".into(),
                    connections: 1,
                    allow_private: false,
                }),
            }]),
        };
        let (events, _) = watch::channel(manager.snapshot());
        (root, manager, events)
    }
    #[tokio::test]
    async fn pause_only_wins_after_drained_cancellation_not_completion_or_failure() {
        for outcome in [
            Finished::Cancelled,
            Finished::Completed {
                path: "/tmp/file.bin".into(),
                bytes: 17,
            },
            Finished::AlreadyPresent {
                path: "/tmp/file.bin".into(),
            },
            Finished::Failed("network failure".into()),
        ] {
            let (_root, mut m, events) = manager().await;
            let expected = if outcome == Finished::Cancelled {
                Finished::Paused
            } else {
                outcome.clone()
            };
            m.finish(1, outcome, &events).await.unwrap();
            assert_eq!(m.jobs[0].job.result, Some(expected));
            assert!(!m.jobs[0].job.pausing);
            assert!(!m.jobs[0].job.cancelling);
            assert_eq!(m.jobs[0].job.retryable, m.jobs[0].input.is_some());
        }
    }
    #[tokio::test]
    async fn shutdown_preserves_pause_but_explicit_cancel_overrides_it() {
        let (_root, mut m, events) = manager().await;
        m.stop(&events).await.unwrap();
        assert!(m.jobs[0].job.pausing);
        assert!(!m.jobs[0].job.cancelling);
        m.finish(1, Finished::Cancelled, &events).await.unwrap();
        assert_eq!(m.jobs[0].job.result, Some(Finished::Paused));
        let (_root, mut m, events) = manager().await;
        m.command(
            Request::Cancel {
                instance: "test".into(),
                id: 1,
            },
            &mut JoinSet::new(),
            &events,
        )
        .await
        .unwrap();
        assert!(!m.jobs[0].job.pausing);
        assert!(m.jobs[0].job.cancelling);
        m.finish(1, Finished::Cancelled, &events).await.unwrap();
        assert_eq!(m.jobs[0].job.result, Some(Finished::Cancelled));
    }
}
