//! CLI control plane. Only the service owns the engine and its cancellation token.
use std::{fmt, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, bail};
use rdm_ipc::{AddCli, Finished, Job, Request, Snapshot, client};
use tokio::signal::unix::{SignalKind, signal};

use crate::{
    args::JobsCommand,
    ui::{self, ProgressSink},
};

/// Instance-qualified IDs cannot accidentally cancel a new job after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    pub instance: String,
    pub id: u64,
}
impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.instance, self.id)
    }
}
impl FromStr for Handle {
    type Err = String;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let invalid = || "Use a full INSTANCE:ID handle from `rdm jobs list`.".to_owned();
        let (instance, id) = value.split_once(':').ok_or_else(invalid)?;
        if instance.is_empty()
            || instance.len() > 96
            || !instance
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err(invalid());
        }
        let id = id
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(invalid)?;
        Ok(Self {
            instance: instance.into(),
            id,
        })
    }
}
fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?)
}
fn safe_error(e: client::Error) -> anyhow::Error {
    anyhow::anyhow!("{}", ui::terminal_safe(&e.to_string()))
}
fn find<'a>(snapshot: &'a Snapshot, handle: &Handle) -> Result<&'a Job> {
    if snapshot.instance != handle.instance {
        bail!(
            "Service restarted; {handle} belongs to an older instance. Download state is unknown."
        );
    }
    snapshot
        .jobs
        .iter()
        .find(|j| j.id == handle.id)
        .context("Job is no longer in service memory; its result is unknown.")
}
fn print_job(instance: &str, job: &Job) {
    let state = match &job.result {
        None if job.cancelling => "cancelling",
        None => "active",
        Some(Finished::Completed { .. }) => "completed",
        Some(Finished::AlreadyPresent { .. }) => "already-present",
        Some(Finished::Cancelled) => "cancelled",
        Some(Finished::Failed(_)) => "failed",
    };
    println!(
        "{}",
        ui::clip(
            &ui::terminal_safe(&format!(
                "{instance}:{}  {state}  {}  {} bytes  {}",
                job.id, job.label, job.progress.downloaded, job.destination
            )),
            ui::term_width().saturating_sub(1)
        )
    );
}
/// Read-only commands never start the service or depend on local config parsing.
pub fn jobs(command: &JobsCommand) -> Result<()> {
    runtime()?.block_on(async {
        match command {
            JobsCommand::List { json } => {
                let s = client::request(Request::List).await.map_err(safe_error)?;
                if *json { println!("{}", serde_json::to_string(&s)?); }
                else { for j in &s.jobs { print_job(&s.instance, j); } }
            }
            JobsCommand::Inspect { handle, json } => {
                let s = client::request(Request::List).await.map_err(safe_error)?;
                let j = find(&s, handle)?;
                if *json { println!("{}", serde_json::to_string(j)?); } else { print_job(&s.instance, j); }
            }
            JobsCommand::Cancel { handle } => {
                let s = client::request(Request::Cancel { instance: handle.instance.clone(), id: handle.id }).await.map_err(safe_error)?;
                let j = find(&s, handle)?;
                if j.active() { println!("Cancellation requested for {handle}; inspect again for the terminal result."); }
                else { print_job(&s.instance, j); }
            }
        }
        Ok(())
    })
}

pub fn download(
    url: String,
    output: String,
    connections: usize,
    allow_private: bool,
    quiet: bool,
    detach: bool,
) -> Result<()> {
    // Resolve relative configuration/output in the caller, not the daemon's cwd.
    let path = PathBuf::from(output);
    let output = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let output = output
        .to_str()
        .context("Destination must be UTF-8.")?
        .to_owned();
    let allow_private = allow_private || crate::net::env_flag("RDM_ALLOW_PRIVATE");
    runtime()?.block_on(async move {
        // Register BEFORE startup/submission so a signal during acknowledgement
        // is buffered. Never select/drop a mutation future: it may have committed.
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        client::ensure_running().await.map_err(safe_error)?;
        let initial = client::request(Request::AddCli(AddCli {
            url,
            output,
            connections,
            allow_private,
        }))
        .await
        .map_err(safe_error)?;
        let id = initial
            .jobs
            .last()
            .context("Service acknowledged without a job; inspect before retrying.")?
            .id;
        let handle = Handle {
            instance: initial.instance.clone(),
            id,
        };
        if detach {
            println!("{handle}");
            if !quiet {
                eprintln!(
                    "Accepted by RDM (not completed). Inspect with `rdm jobs inspect {handle}`."
                );
            }
            return Ok(());
        }
        if !quiet {
            eprintln!("RDM job {handle}; Ctrl+C requests cancellation of this job only.");
        }
        let bar: std::sync::Arc<dyn ProgressSink> = if quiet {
            ui::silent()
        } else {
            ui::SoloBar::new(&find(&initial, &handle)?.label)
        };
        let result = wait(initial, &handle, &bar, &mut interrupt, &mut terminate).await;
        bar.finish();
        let outcome = result.with_context(|| {
            format!("Job {handle}; inspect its state before retrying. It may still be running.")
        })?;
        match outcome {
            Finished::Completed { path, bytes } => {
                if !quiet {
                    eprintln!("Downloaded {bytes} bytes to {}", ui::terminal_safe(&path));
                }
                Ok(())
            }
            Finished::AlreadyPresent { .. } => bail!(
                "Destination already exists and was left unchanged. Choose a different -o path."
            ),
            Finished::Cancelled => bail!(
                "Download cancelled ({handle}). Valid partial state is retained where supported."
            ),
            Finished::Failed(message) => bail!(
                "Download failed ({handle}): {}",
                ui::terminal_safe(&message)
            ),
        }
    })
}
async fn wait(
    initial: Snapshot,
    handle: &Handle,
    bar: &std::sync::Arc<dyn ProgressSink>,
    interrupt: &mut tokio::signal::unix::Signal,
    terminate: &mut tokio::signal::unix::Signal,
) -> Result<Finished> {
    if let Some(result) = &find(&initial, handle)?.result {
        return Ok(result.clone());
    }
    let mut feed = client::subscribe().await.map_err(safe_error)?;
    let mut snapshot = feed.initial.clone();
    let mut cancelling = false;
    loop {
        let job = find(&snapshot, handle)?;
        bar.total(job.progress.total);
        bar.progress(job.progress.downloaded);
        bar.state(match job.progress.phase {
            rdm_ipc::Phase::Waiting => ui::SlotState::Waiting,
            rdm_ipc::Phase::Inspecting => ui::SlotState::Inspecting,
            rdm_ipc::Phase::Downloading => ui::SlotState::Downloading,
            rdm_ipc::Phase::Finishing => ui::SlotState::Finishing,
        });
        if let Some(result) = &job.result {
            return Ok(result.clone());
        }
        tokio::select! {
            next = feed.next() => snapshot = next.map_err(safe_error)?,
            _ = async { tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} } } => {
                if cancelling { bail!("Stopped waiting; cancellation has not yet been confirmed."); }
                // Await the one-shot cancellation acknowledgement too. A lost
                // reply is ambiguous; no retry and no service shutdown.
                snapshot = client::request(Request::Cancel { instance: handle.instance.clone(), id: handle.id }).await.map_err(safe_error)?;
                cancelling = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handles_are_instance_qualified() {
        for value in ["1", ":1", "a:0", "a:1:2", "a\n:1", "a/../b:1", "a:-1"] {
            assert!(value.parse::<Handle>().is_err(), "{value:?}");
        }
        let h: Handle = "123-456:7".parse().unwrap();
        assert_eq!(h.to_string(), "123-456:7");
    }
}
