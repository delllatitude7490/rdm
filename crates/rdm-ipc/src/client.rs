use crate::{
    ErrorCode, Packet, Request, Response, Snapshot, VERSION,
    runtime::{Paths, uid},
    wire,
};
use std::fmt;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    Runtime,
    Incompatible,
    Uncertain,
    Disconnected,
    MissingExecutable,
    StartupFailed,
    Rejected(ErrorCode, String),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("RDM service is not running."),
            Self::Runtime => f.write_str("Cannot access RDM's private socket. Check XDG_RUNTIME_DIR ownership and permissions."),
            Self::Incompatible => f.write_str("The running service uses an incompatible protocol. Shut it down and restart matching binaries."),
            Self::Uncertain => f.write_str("The command outcome could not be confirmed. Reconnect and inspect downloads before retrying; it may have been accepted."),
            Self::Disconnected => f.write_str("Disconnected from the service. Download state is unknown; work may still be running."),
            Self::StartupFailed => f.write_str("RDM service did not become ready. Run rdm-service in a terminal to check its configuration and startup errors."),
            Self::MissingExecutable => f.write_str("Could not start rdm-service. Build/install it alongside rdm-gui or put it on PATH."),
            Self::Rejected(_, message) => f.write_str(message),
        }
    }
}
impl std::error::Error for Error {}

async fn connect(paths: &Paths) -> Result<UnixStream, Error> {
    paths.validate_socket().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Error::Unavailable
        } else {
            Error::Runtime
        }
    })?;
    let stream = timeout(DEADLINE, UnixStream::connect(&paths.socket))
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Error::Unavailable,
            _ => Error::Runtime,
        })?;
    if stream.peer_cred().map_err(|_| Error::Runtime)?.uid() != uid() {
        return Err(Error::Runtime);
    }
    Ok(stream)
}

fn snapshot(packet: Packet<Response>) -> Result<Snapshot, Error> {
    if packet.version != VERSION {
        return Err(Error::Incompatible);
    }
    match packet.body {
        Response::Snapshot(snapshot) => Ok(snapshot),
        Response::Error {
            code: ErrorCode::Version,
            ..
        } => Err(Error::Incompatible),
        Response::Error { code, message } => Err(Error::Rejected(code, message)),
    }
}

/// Mutations are sent exactly once. Any write/read ambiguity is surfaced, not retried.
pub async fn request_at(paths: &Paths, request: Request) -> Result<Snapshot, Error> {
    let mut stream = connect(paths).await?;
    timeout(DEADLINE, wire::write(&mut stream, &Packet::new(request)))
        .await
        .map_err(|_| Error::Uncertain)?
        .map_err(|_| Error::Uncertain)?;
    let packet = timeout(DEADLINE, wire::read(&mut stream))
        .await
        .map_err(|_| Error::Uncertain)?
        .map_err(|_| Error::Uncertain)?;
    snapshot(packet)
}

pub async fn request(request: Request) -> Result<Snapshot, Error> {
    let paths = Paths::discover().map_err(|_| Error::Runtime)?;
    request_at(&paths, request).await
}

pub struct Feed {
    stream: UnixStream,
    pub initial: Snapshot,
}
impl Feed {
    pub async fn next(&mut self) -> Result<Snapshot, Error> {
        let packet = wire::read(&mut self.stream)
            .await
            .map_err(|_| Error::Disconnected)?;
        snapshot(packet)
    }
}

pub async fn subscribe_at(paths: &Paths) -> Result<Feed, Error> {
    let mut stream = connect(paths).await?;
    timeout(
        DEADLINE,
        wire::write(&mut stream, &Packet::new(Request::Subscribe)),
    )
    .await
    .map_err(|_| Error::Disconnected)?
    .map_err(|_| Error::Disconnected)?;
    let packet = timeout(DEADLINE, wire::read(&mut stream))
        .await
        .map_err(|_| Error::Disconnected)?
        .map_err(|_| Error::Disconnected)?;
    Ok(Feed {
        stream,
        initial: snapshot(packet)?,
    })
}

pub async fn subscribe() -> Result<Feed, Error> {
    let paths = Paths::discover().map_err(|_| Error::Runtime)?;
    subscribe_at(&paths).await
}

/// Startup is triggered only by an unavailable service, never by version,
/// permission or protocol errors. The startup probes are read-only.
pub async fn ensure_running() -> Result<(), Error> {
    let paths = Paths::discover().map_err(|_| Error::Runtime)?;
    match request_at(&paths, Request::List).await {
        Ok(_) => return Ok(()),
        Err(Error::Unavailable) => {}
        Err(e) => return Err(e),
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("rdm-service")));
    let executable = sibling
        .filter(|p| p.is_file())
        .unwrap_or_else(|| "rdm-service".into());
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(executable)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| Error::MissingExecutable)?;
    // Reap without coupling service lifetime to the GUI or Tokio task lifetime.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match request_at(&paths, Request::List).await {
            Ok(_) => return Ok(()),
            Err(Error::Unavailable) => {}
            Err(e) => return Err(e),
        }
    }
    Err(Error::StartupFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Instance;
    #[tokio::test]
    async fn ambiguous_command_is_not_retried() {
        use std::os::unix::fs::PermissionsExt;
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths::in_runtime(runtime.path()).unwrap();
        let mut owner = Instance::acquire(&paths).unwrap();
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        owner.bound().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _: Packet<Request> = wire::read(&mut stream).await.unwrap();
            drop(stream); // May have accepted a mutation, but no acknowledgement.
            assert!(
                timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let result = request_at(
            &paths,
            Request::Shutdown {
                instance: "test".into(),
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), Error::Uncertain);
        task.await.unwrap();
    }
}
