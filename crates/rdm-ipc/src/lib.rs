//! Versioned, bounded local IPC shared by the native GUI and service.
#![cfg(target_os = "linux")]
pub mod client;
pub mod runtime;
pub mod wire;

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 4;
pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_JOBS: usize = 64;
pub type JobId = u64;

#[derive(Clone, Serialize, Deserialize)]
pub struct Add {
    pub url: String,
    pub directory: String,
    pub filename: String,
}

impl std::fmt::Debug for Add {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Add")
            .field("url", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// CLI-resolved destination and per-invocation network options.
#[derive(Clone, Serialize, Deserialize)]
pub struct AddCli {
    pub url: String,
    pub output: String,
    pub connections: usize,
    pub allow_private: bool,
}
impl std::fmt::Debug for AddCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AddCli { [redacted] }")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", content = "data", rename_all = "snake_case")]
pub enum Request {
    Add(Add),
    AddCli(AddCli),
    List,
    Subscribe,
    Cancel {
        instance: String,
        id: JobId,
    },
    RemoveQueued {
        instance: String,
        id: JobId,
    },
    /// Explicitly requeue a retryable record; never automatic on recovery.
    Retry {
        instance: String,
        id: JobId,
    },
    Shutdown {
        instance: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet<T> {
    pub version: u32,
    pub body: T,
}

impl<T> Packet<T> {
    pub fn new(body: T) -> Self {
        Self {
            version: VERSION,
            body,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Waiting,
    Inspecting,
    Downloading,
    Finishing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub total: Option<u64>,
    pub downloaded: u64,
    pub phase: Phase,
    pub has_notice: bool,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            total: None,
            downloaded: 0,
            phase: Phase::Waiting,
            has_notice: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Finished {
    Completed {
        path: String,
        bytes: u64,
    },
    AlreadyPresent {
        path: String,
    },
    Cancelled,
    /// Recovered unfinished work; requires explicit user action.
    Interrupted,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub label: String,
    pub destination: String,
    pub progress: Progress,
    /// Accepted but not started. Queued jobs have no engine or filesystem work.
    // Allows decoding an old snapshot only to report its version mismatch.
    #[serde(default)]
    pub queued: bool,
    /// Private retry input exists; snapshots never contain the source URL.
    #[serde(default)]
    pub retryable: bool,
    /// A deliberate retry created a distinct attempt; old handles stay safe.
    #[serde(default)]
    pub retried_as: Option<JobId>,
    pub cancelling: bool,
    pub result: Option<Finished>,
}
impl Job {
    /// Unfinished, including queued jobs (not a count of running workers).
    pub fn active(&self) -> bool {
        self.result.is_none()
    }
    pub fn running(&self) -> bool {
        self.active() && !self.queued
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub instance: String,
    /// Monotonic per service instance; command replies cannot roll back newer events.
    pub revision: u64,
    pub default_directory: String,
    pub shutting_down: bool,
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    Invalid,
    Busy,
    ShuttingDown,
    StaleInstance,
    NotFound,
    Version,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Snapshot(Snapshot),
    Error { code: ErrorCode, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_debug_never_contains_urls_or_credentials() {
        let request = Request::Add(Add {
            url: "https://u:secret@example.com/file?token=secret".into(),
            directory: "/tmp".into(),
            filename: "file".into(),
        });
        assert!(!format!("{request:?}").contains("secret"));
        let bytes = serde_json::to_vec(&Packet::new(request)).unwrap();
        let decoded: Packet<Request> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.version, VERSION);
        assert!(matches!(decoded.body, Request::Add(_)));
    }
}

#[cfg(test)]
mod cli_tests {
    #[test]
    fn cli_input_debug_is_redacted() {
        let request = super::Request::AddCli(super::AddCli {
            url: "https://secret/file?token=secret".into(),
            output: "/secret/file".into(),
            connections: 1,
            allow_private: true,
        });
        assert!(!format!("{request:?}").contains("secret"));
    }
}
