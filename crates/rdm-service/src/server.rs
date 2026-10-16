//! A single actor owns all managed jobs. Socket handlers cannot mutate job state.
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::transfer::{self, GuiSink};
use rdm::config::Config;
use rdm_ipc::{
    Add, ErrorCode, Finished, Job, MAX_JOBS, Packet, Request, Response, Snapshot, VERSION,
};
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
    fn stop(&mut self, events: &watch::Sender<Snapshot>) {
        self.stopping = true;
        for managed in &mut self.jobs {
            if managed.job.active() {
                managed.job.cancelling = true;
                managed.cancel.cancel();
            }
        }
        self.publish(events);
    }
    fn refresh(&mut self, events: &watch::Sender<Snapshot>) {
        for managed in &mut self.jobs {
            if managed.job.active() {
                managed.job.progress = managed.sink.snapshot();
            }
        }
        // Active heartbeats also let clients decay speed to zero during stalls.
        // This is disabled altogether when there are no running workers.
        self.publish(events);
    }
    fn finish(&mut self, id: u64, result: Finished, events: &watch::Sender<Snapshot>) {
        if let Some(managed) = self.jobs.iter_mut().find(|m| m.job.id == id) {
            managed.job.progress = managed.sink.snapshot();
            if let Finished::Completed { bytes, .. } = &result {
                managed.job.progress.downloaded = *bytes;
                managed.job.progress.total = Some(*bytes);
            }
            managed.job.result = Some(result);
            managed.job.cancelling = false;
            self.publish(events);
        }
    }
    fn add(
        &mut self,
        add: Add,
        workers: &mut JoinSet<(u64, Finished)>,
        events: &watch::Sender<Snapshot>,
    ) -> Response {
        if self.stopping {
            return error(ErrorCode::ShuttingDown, "The service is shutting down.");
        }
        // This milestone deliberately admits ONE managed transfer. This protects
        // even aliases and engine-selected Content-Disposition filenames from
        // simultaneous managed writes. A concurrent queue is a later milestone.
        if self.jobs.iter().any(|m| m.job.active()) {
            return error(
                ErrorCode::Busy,
                "A managed download is already active. Cancel it or wait for completion.",
            );
        }
        let input = match transfer::prepare(
            &add.url,
            &add.directory,
            &add.filename,
            self.config.connections,
        ) {
            Ok(input) => input,
            Err(message) => {
                return Response::Error {
                    code: ErrorCode::Invalid,
                    message,
                };
            }
        };
        while self.jobs.len() >= MAX_JOBS {
            self.jobs.pop_front();
        }
        let id = self.next_id;
        self.next_id += 1;
        let sink = GuiSink::default();
        let cancel = CancellationToken::new();
        self.jobs.push_back(Managed {
            job: Job {
                id,
                label: input.label.clone(),
                destination: input.request.output.clone().unwrap_or_default(),
                progress: Default::default(),
                cancelling: false,
                result: None,
            },
            sink: sink.clone(),
            cancel: cancel.clone(),
        });
        workers.spawn(async move { (id, transfer::download(input, sink, cancel).await) });
        self.publish(events);
        Response::Snapshot(self.snapshot())
    }
    fn command(
        &mut self,
        request: Request,
        workers: &mut JoinSet<(u64, Finished)>,
        events: &watch::Sender<Snapshot>,
    ) -> Response {
        match request {
            Request::List | Request::Subscribe => Response::Snapshot(self.snapshot()),
            Request::Add(add) => self.add(add, workers, events),
            Request::Cancel { instance, id } => {
                if instance != self.instance {
                    return error(
                        ErrorCode::StaleInstance,
                        "The service restarted. Reconnect before issuing commands.",
                    );
                }
                let Some(managed) = self.jobs.iter_mut().find(|m| m.job.id == id) else {
                    return error(
                        ErrorCode::NotFound,
                        "That download is no longer in service memory.",
                    );
                };
                if managed.job.active() {
                    managed.job.cancelling = true;
                    managed.cancel.cancel();
                    self.publish(events);
                }
                Response::Snapshot(self.snapshot())
            }
            Request::Shutdown { instance } => {
                if instance != self.instance {
                    return error(
                        ErrorCode::StaleInstance,
                        "The service restarted. Reconnect before shutting it down.",
                    );
                }
                self.stop(events);
                Response::Snapshot(self.snapshot())
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
        next_id: 1,
        stopping: false,
        jobs: VecDeque::new(),
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
                    Err(e) => { accept_error = Some(e); manager.stop(&events); }
                }
            }
            Some(command) = incoming.recv() => {
                let response = manager.command(command.request, &mut workers, &events);
                let _ = command.reply.send(response);
            }
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                match result {
                    Ok((id, result)) => manager.finish(id, result, &events),
                    Err(_) => {
                        if let Some(id) = manager.jobs.iter().find(|m| m.job.active()).map(|m| m.job.id) {
                            manager.finish(id, Finished::Failed("The download worker stopped unexpectedly.".into()), &events);
                        }
                    }
                }
            }
            _ = tick.tick(), if !workers.is_empty() => manager.refresh(&events),
            _ = external_stop.cancelled(), if !manager.stopping => manager.stop(&events),
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
