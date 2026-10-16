//! Restricted Firefox-to-service adapter. Browser input cannot choose paths,
//! cancel downloads, shut down the service, or send arbitrary IPC commands.
#![cfg(target_os = "linux")]
pub mod install;
use rdm_ipc::{Add, Request, client};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const EXTENSION_ID: &str = "rdm@delllatitude7490";
pub const HOST_NAME: &str = "rdm_firefox";
pub const VERSION: u32 = 1;
pub const MAX_INPUT: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub version: u32,
    pub command: Command,
}
// Intentionally no Debug: requests may contain bearer/signed URLs.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Add { url: String },
    Status,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u32,
    pub ok: bool,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
}
impl Reply {
    pub fn new(ok: bool, code: &str, message: &str) -> Self {
        Self {
            version: VERSION,
            ok,
            code: code.into(),
            message: message.into(),
            job_id: None,
        }
    }
    pub fn invalid() -> Self {
        Self::new(
            false,
            "invalid",
            "Invalid request. Use a direct HTTP/HTTPS file URL.",
        )
    }
    fn client_error(error: client::Error) -> Self {
        // Never relay arbitrary native/service error text into browser UI.
        use client::Error;
        let (code, message) = match error {
            Error::Unavailable => (
                "unavailable",
                "RDM service is not running. Sending a link will try to start it.",
            ),
            Error::Runtime => (
                "runtime",
                "Cannot access RDM's private socket. Check XDG_RUNTIME_DIR and permissions.",
            ),
            Error::Incompatible => (
                "incompatible",
                "RDM versions do not match. Restart with matching service and bridge binaries.",
            ),
            Error::MissingExecutable => (
                "missing_service",
                "rdm-service is missing. Install it alongside rdm-native-host.",
            ),
            Error::StartupFailed => (
                "startup",
                "RDM service could not start. Run rdm-service in a terminal to check configuration.",
            ),
            Error::Uncertain | Error::Disconnected => (
                "uncertain",
                "Outcome unconfirmed. Check the RDM GUI before retrying; the download may have started.",
            ),
            Error::Rejected(code, _) => match code {
                rdm_ipc::ErrorCode::Busy => (
                    "busy",
                    "The RDM queue is full. Remove a queued download or wait.",
                ),
                rdm_ipc::ErrorCode::ShuttingDown => (
                    "stopping",
                    "RDM is shutting down. Wait, then send the link again.",
                ),
                rdm_ipc::ErrorCode::Invalid => (
                    "invalid",
                    "RDM refused this URL or its configured destination. Use a direct public HTTP/HTTPS file link; provider shares are not supported.",
                ),
                _ => (
                    "service",
                    "RDM could not accept this request. Check the GUI before retrying.",
                ),
            },
        };
        Self::new(false, code, message)
    }
}

/// Firefox uses NATIVE endian length, unlike RDM's big-endian socket protocol.
/// This host handles exactly one message; sendNativeMessage launches one process.
pub fn read_message(reader: &mut impl Read) -> io::Result<Message> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_ne_bytes(header) as usize;
    if len == 0 || len > MAX_INPUT {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|_| io::ErrorKind::InvalidData.into())
}
pub fn write_reply(writer: &mut impl Write, reply: &Reply) -> io::Result<()> {
    let bytes = serde_json::to_vec(reply).map_err(|_| io::ErrorKind::InvalidData)?;
    if bytes.len() > MAX_INPUT {
        return Err(io::ErrorKind::InvalidData.into());
    }
    writer.write_all(&(bytes.len() as u32).to_ne_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub async fn dispatch(message: Message) -> Reply {
    if message.version != VERSION {
        return Reply::new(
            false,
            "version",
            "Unsupported extension protocol. Install matching extension and native binaries.",
        );
    }
    let url = match message.command {
        Command::Status => {
            return match client::request(Request::List).await {
                Ok(state) if state.shutting_down => {
                    Reply::new(false, "stopping", "RDM service is shutting down.")
                }
                Ok(state) if state.jobs.iter().any(|j| j.active()) => Reply::new(
                    true,
                    "active",
                    "RDM is connected. A download is active; open the GUI for progress.",
                ),
                Ok(_) => Reply::new(true, "ready", "RDM is connected and ready for a link."),
                Err(error) => Reply::client_error(error),
            };
        }
        Command::Add { url } => url,
    };
    // The engine/service remains the authority on URL/provider/network policy.
    // Reject oversized/control input before any on-demand process startup.
    let url = url.trim();
    if url.is_empty() || url.len() > 16 * 1024 || url.chars().any(char::is_control) {
        return Reply::invalid();
    }
    if let Err(error) = client::ensure_running().await {
        return Reply::client_error(error);
    }
    let state = match client::request(Request::List).await {
        Ok(state) => state,
        Err(error) => return Reply::client_error(error),
    };
    // Only configured service default directory; browser cannot name output paths.
    let response = client::request(Request::Add(Add {
        url: url.to_owned(),
        directory: state.default_directory,
        filename: String::new(),
    }))
    .await;
    match response {
        Ok(state) => {
            let queued = state.jobs.last().is_some_and(|j| j.queued);
            let mut reply = if queued {
                Reply::new(
                    true,
                    "queued",
                    "Queued in RDM. Open the GUI for position and progress; it has not started yet.",
                )
            } else {
                Reply::new(
                    true,
                    "accepted",
                    "Accepted by RDM. Open the GUI to view progress; acceptance is not completion.",
                )
            };
            reply.job_id = state.jobs.last().map(|j| j.id);
            reply
        }
        Err(error) => Reply::client_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(value: &[u8]) -> Vec<u8> {
        [(value.len() as u32).to_ne_bytes().as_slice(), value].concat()
    }
    #[test]
    fn native_endian_roundtrip_and_stdout_has_only_one_frame() {
        let value = br#"{"version":1,"command":{"action":"status"}}"#;
        assert!(matches!(
            read_message(&mut frame(value).as_slice()).unwrap().command,
            Command::Status
        ));
        let mut output = Vec::new();
        write_reply(&mut output, &Reply::invalid()).unwrap();
        let len = u32::from_ne_bytes(output[..4].try_into().unwrap()) as usize;
        assert_eq!(len + 4, output.len());
        let reply: Reply = serde_json::from_slice(&output[4..]).unwrap();
        assert!(!reply.ok);
    }
    #[test]
    fn bad_frames_and_arbitrary_commands_are_rejected() {
        for data in [
            vec![],
            vec![0],
            0u32.to_ne_bytes().to_vec(),
            ((MAX_INPUT + 1) as u32).to_ne_bytes().to_vec(),
            frame(b"bad"),
            vec![8, 0, 0, 0, 123],
        ] {
            assert!(read_message(&mut data.as_slice()).is_err());
        }
        for value in [
            br#"{"version":1,"command":{"action":"shutdown"}}"#.as_slice(),
            br#"{"version":1,"command":{"action":"add","url":"https://example.com/f","directory":"/etc"}}"#,
            br#"{"version":1,"command":{"action":"status"},"other":true}"#,
        ] { assert!(read_message(&mut frame(value).as_slice()).is_err()); }
    }
    #[test]
    fn errors_do_not_echo_arbitrary_service_text() {
        let reply = Reply::client_error(client::Error::Rejected(
            rdm_ipc::ErrorCode::Invalid,
            "https://u:secret@host/file?token=secret".into(),
        ));
        assert!(!serde_json::to_string(&reply).unwrap().contains("secret"));
    }
}
