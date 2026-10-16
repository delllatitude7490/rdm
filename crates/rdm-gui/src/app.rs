use crate::backend;
use crate::theme::{ACCENT, ERROR, MUTED};
use iced::widget::{button, column, container, progress_bar, row, scrollable, text, text_input};
use iced::{Element, Fill, Subscription, Task};
use rdm::secret_url::SecretUrl;
use rdm::ui::{self, Rate};
use rdm_ipc::{Add, Finished, Job, Phase, Request, Snapshot, client};

pub struct App {
    url: SecretUrl,
    directory: String,
    filename: String,
    error: Option<String>,
    snapshot: Option<Snapshot>,
    jobs: Vec<Tracked>,
    connected: bool,
    pending: bool,
    epoch: u64,
    connection_message: String,
}
struct Tracked {
    record: Job,
    rate: Rate,
    speed: Option<u64>,
}
impl Tracked {
    fn status(&self) -> &str {
        match &self.record.result {
            Some(Finished::Completed { .. }) => "Completed",
            Some(Finished::AlreadyPresent { .. }) => "Already exists — file left unchanged",
            Some(Finished::Cancelled) => "Cancelled",
            Some(Finished::Failed(_)) => "Failed",
            None if self.record.cancelling => "Cancelling safely…",
            None => match self.record.progress.phase {
                Phase::Waiting => "Preparing",
                Phase::Inspecting => "Connecting",
                Phase::Downloading => "Downloading",
                Phase::Finishing => "Saving",
            },
        }
    }
}
#[derive(Debug, Clone)]
pub enum Message {
    UrlChanged(SecretUrl),
    DirectoryChanged(String),
    FilenameChanged(String),
    Start,
    Cancel(u64),
    Shutdown,
    Reconnect,
    CloseRequested,
    Service(u64, backend::Event),
    ActionDone {
        epoch: u64,
        instance: String,
        result: Result<Snapshot, client::Error>,
    },
}
impl App {
    pub fn boot() -> Self {
        Self {
            url: SecretUrl::new(""),
            directory: String::new(),
            filename: String::new(),
            error: None,
            snapshot: None,
            jobs: Vec::new(),
            connected: false,
            pending: false,
            epoch: 0,
            connection_message: "Connecting to the download service…".into(),
        }
    }
    fn active(&self) -> bool {
        self.jobs.iter().any(|j| j.record.active())
    }
    fn stopping(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| s.shutting_down)
    }
    fn apply(&mut self, snapshot: Snapshot) {
        if let Some(old) = &self.snapshot {
            if old.instance == snapshot.instance && old.revision > snapshot.revision {
                return;
            }
            if old.instance != snapshot.instance {
                self.jobs.clear();
                self.pending = false;
                self.error = Some("The service restarted. Previous in-memory records are not restored; partial files remain managed by the engine.".into());
            }
        }
        if self.directory.is_empty() {
            self.directory = snapshot.default_directory.clone();
        }
        let mut old = std::mem::take(&mut self.jobs);
        for record in &snapshot.jobs {
            let existing = old
                .iter()
                .position(|j| j.record.id == record.id)
                .map(|i| old.remove(i));
            let mut tracked = existing.unwrap_or_else(|| Tracked {
                record: record.clone(),
                rate: Rate::new(),
                speed: None,
            });
            tracked.record = record.clone();
            if record.active() {
                tracked.rate.record(record.progress.downloaded);
                tracked.speed = tracked.rate.per_second();
            } else {
                tracked.speed = None;
            }
            self.jobs.push(tracked);
        }
        self.snapshot = Some(snapshot);
    }
    fn send(&mut self, request: Request) -> Task<Message> {
        let Some(snapshot) = &self.snapshot else {
            return Task::none();
        };
        if !self.connected || self.pending {
            return Task::none();
        }
        let instance = snapshot.instance.clone();
        let epoch = self.epoch;
        self.pending = true;
        self.error = None;
        Task::perform(client::request(request), move |result| {
            Message::ActionDone {
                epoch,
                instance: instance.clone(),
                result,
            }
        })
    }
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            // Dropping the GUI subscription never cancels service-owned work.
            Message::CloseRequested => return iced::exit(),
            Message::Reconnect => {
                self.epoch += 1;
                self.connected = false;
                self.pending = false;
                self.connection_message = "Reconnecting; inspect the service state before retrying an unconfirmed command.".into();
            }
            Message::Service(epoch, event) if epoch == self.epoch => match event {
                backend::Event::Connecting => {
                    self.connected = false;
                    self.connection_message = "Connecting to the download service…".into();
                }
                backend::Event::Snapshot(snapshot) => {
                    self.connected = true;
                    self.connection_message = if snapshot.shutting_down {
                        "Service is shutting down safely…"
                    } else {
                        "Downloads run independently of this window."
                    }
                    .into();
                    self.apply(snapshot);
                }
                backend::Event::Disconnected(message) => {
                    self.connected = false;
                    self.connection_message = message;
                    for job in &mut self.jobs {
                        job.speed = None;
                    }
                }
            },
            Message::ActionDone {
                epoch,
                instance,
                result,
            } if epoch == self.epoch
                && self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.instance == instance) =>
            {
                self.pending = false;
                match result {
                    Ok(snapshot) if self.connected && snapshot.instance == instance => {
                        self.apply(snapshot)
                    }
                    Ok(_) => {}
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            Message::UrlChanged(value) if !self.active() && !self.pending => self.url = value,
            Message::DirectoryChanged(value) if !self.active() && !self.pending => {
                self.directory = value
            }
            Message::FilenameChanged(value) if !self.active() && !self.pending => {
                self.filename = value
            }
            Message::Start if self.connected && !self.active() && !self.stopping() => {
                return self.send(Request::Add(Add {
                    url: self.url.as_str().to_owned(),
                    directory: self.directory.clone(),
                    filename: self.filename.clone(),
                }));
            }
            Message::Cancel(id) if self.connected && !self.stopping() => {
                if let Some(snapshot) = &self.snapshot {
                    return self.send(Request::Cancel {
                        instance: snapshot.instance.clone(),
                        id,
                    });
                }
            }
            Message::Shutdown if self.connected && !self.stopping() => {
                if let Some(snapshot) = &self.snapshot {
                    return self.send(Request::Shutdown {
                        instance: snapshot.instance.clone(),
                    });
                }
            }
            _ => {}
        }
        Task::none()
    }
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::window::close_requests().map(|_| Message::CloseRequested),
            Subscription::run_with(self.epoch, backend::events)
                .map(|(epoch, event)| Message::Service(epoch, event)),
        ])
    }
    pub fn view(&self) -> Element<'_, Message> {
        let editable = self.connected && !self.active() && !self.pending && !self.stopping();
        let mut url = text_input("https://example.com/file.zip", self.url.as_str()).padding(12);
        let mut directory = text_input("/home/you/Downloads", &self.directory).padding(12);
        let mut filename = text_input("Automatic from URL", &self.filename).padding(12);
        if editable {
            url = url
                .on_input(|value| Message::UrlChanged(SecretUrl::new(value)))
                .on_submit(Message::Start);
            directory = directory.on_input(Message::DirectoryChanged);
            filename = filename.on_input(Message::FilenameChanged);
        }
        let mut start = button(text("Start download").size(15))
            .padding([11, 18])
            .height(44);
        if editable && !self.url.as_str().trim().is_empty() {
            start = start.on_press(Message::Start);
        }

        let header = row![
            column![
                text("RDM").size(32).color(ACCENT),
                text("Download quietly.").size(15).color(MUTED),
            ]
            .spacing(3),
            iced::widget::space().width(Fill),
            text(if self.connected {
                "SERVICE CONNECTED"
            } else {
                "SERVICE OFFLINE"
            })
            .size(11)
            .color(MUTED),
        ]
        .align_y(iced::Alignment::Center);

        let form = column![
            text("NEW DOWNLOAD").size(11).color(MUTED),
            text("File URL").size(13),
            url,
            row![
                column![text("Save folder").size(13), directory]
                    .spacing(7)
                    .width(Fill),
                column![text("Filename · optional").size(13), filename]
                    .spacing(7)
                    .width(Fill),
            ]
            .spacing(14),
            row![
                text("Direct HTTP / HTTPS files").size(12).color(MUTED),
                iced::widget::space().width(Fill),
                start,
            ]
            .align_y(iced::Alignment::Center),
        ]
        .spacing(12);
        let mut content = column![
            header,
            container(form)
                .padding(22)
                .width(Fill)
                .style(container::rounded_box),
        ]
        .spacing(22);

        if let Some(error) = &self.error {
            content = content.push(text(error).size(13).color(ERROR));
        }
        let connection_control = if self.connected {
            let mut stop = button("Stop service & cancel downloads")
                .height(44)
                .style(button::secondary);
            if !self.pending && !self.stopping() {
                stop = stop.on_press(Message::Shutdown);
            }
            stop
        } else {
            button("Reconnect / start service")
                .height(44)
                .on_press(Message::Reconnect)
        };
        content = content.push(
            row![
                text(&self.connection_message)
                    .size(13)
                    .color(MUTED)
                    .width(Fill),
                connection_control
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center),
        );
        if !self.jobs.is_empty() {
            for job in self.jobs.iter().rev() {
                content = content.push(self.job_view(job));
            }
        } else {
            content = content.push(
                container(
                    column![
                        text("A little less waiting.").size(22),
                        text("Paste a file link above to get started.")
                            .size(14)
                            .color(MUTED),
                    ]
                    .spacing(8),
                )
                .padding(22),
            );
        }
        let footer = "Closing this window leaves downloads running. Stop service explicitly to cancel and exit.";
        content = content.push(text(footer).size(12).color(MUTED));
        container(
            scrollable(
                container(content)
                    .width(Fill)
                    .padding(iced::Padding::ZERO.right(14)),
            )
            .height(Fill),
        )
        .padding(28)
        .width(Fill)
        .height(Fill)
        .into()
    }

    fn job_view<'a>(&self, job: &'a Tracked) -> Element<'a, Message> {
        let snapshot = &job.record.progress;
        let mut heading = row![
            column![
                text(&job.record.label).size(18),
                text(if !self.connected && job.record.active() {
                    "Last known state — service disconnected"
                } else {
                    job.status()
                })
                .size(12)
                .color(MUTED)
            ]
            .spacing(5)
            .width(Fill),
        ]
        .align_y(iced::Alignment::Center);
        if self.connected
            && !self.pending
            && job.record.active()
            && !job.record.cancelling
            && !self.stopping()
        {
            heading = heading.push(
                button("Cancel")
                    .on_press(Message::Cancel(job.record.id))
                    .height(44)
                    .padding([8, 16])
                    .style(button::secondary),
            );
        }
        let mut card = column![heading].spacing(14);
        if !matches!(job.record.result, Some(Finished::AlreadyPresent { .. })) {
            if let Some(fraction) = fraction(snapshot.downloaded, snapshot.total) {
                card = card.push(progress_bar(0.0..=1.0, fraction).girth(5));
            } else {
                // A static indeterminate state: truthful and no animation timer.
                card = card.push(
                    text("Size unknown — progress will appear when available")
                        .size(12)
                        .color(MUTED),
                );
            }
            let amount = match snapshot.total {
                Some(total) => format!(
                    "{} / {}",
                    ui::format_size(snapshot.downloaded),
                    ui::format_size(total)
                ),
                None => format!("{} received", ui::format_size(snapshot.downloaded)),
            };
            card = card.push(row![
                text(amount).size(13).color(MUTED),
                iced::widget::space().width(Fill),
                text(ui::format_speed(job.speed)).size(13).color(MUTED),
            ]);
        }
        if snapshot.has_notice && job.record.active() {
            card = card.push(
                text("The server reported a notice; RDM may be retrying.")
                    .size(12)
                    .color(MUTED),
            );
        }
        match &job.record.result {
            Some(Finished::Failed(error)) => card = card.push(text(error).size(13).color(ERROR)),
            Some(Finished::Completed { path, .. } | Finished::AlreadyPresent { path }) => {
                card = card.push(text(path).size(12).color(MUTED));
            }
            Some(Finished::Cancelled) => {
                card = card.push(text("Valid partial state is retained by the engine where supported. Start the same URL and destination to retry.").size(12).color(MUTED));
            }
            _ => {}
        }
        container(card)
            .padding(22)
            .width(Fill)
            .style(container::rounded_box)
            .into()
    }
}

fn fraction(downloaded: u64, total: Option<u64>) -> Option<f32> {
    total.map(|total| {
        if total == 0 {
            1.0
        } else {
            (downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(instance: &str, revision: u64) -> Snapshot {
        Snapshot {
            instance: instance.into(),
            revision,
            default_directory: "/tmp".into(),
            shutting_down: false,
            jobs: vec![Job {
                id: 1,
                label: "file.bin".into(),
                destination: "/tmp/file.bin".into(),
                progress: Default::default(),
                cancelling: false,
                result: None,
            }],
        }
    }
    #[test]
    fn unknown_size_is_not_a_fake_percentage() {
        assert_eq!(fraction(100, None), None);
        assert_eq!(fraction(50, Some(100)), Some(0.5));
        assert_eq!(fraction(110, Some(100)), Some(1.0));
        assert_eq!(fraction(0, Some(0)), Some(1.0));
    }
    #[test]
    fn input_messages_redact_signed_urls_in_debug_output() {
        let message = Message::UrlChanged(SecretUrl::new("https://example.com/file?token=secret"));
        assert!(!format!("{message:?}").contains("secret"));
    }
    #[test]
    fn closing_does_not_cancel_or_change_service_owned_jobs() {
        let mut app = App::boot();
        app.apply(state("a", 1));
        let _ = app.update(Message::CloseRequested);
        assert!(app.active());
        assert!(!app.jobs[0].record.cancelling);
    }
    #[test]
    fn stale_snapshot_cannot_roll_back_progress() {
        let mut app = App::boot();
        app.apply(state("a", 5));
        let mut stale = state("a", 4);
        stale.jobs.clear();
        app.apply(stale);
        assert_eq!(app.snapshot.unwrap().revision, 5);
        assert_eq!(app.jobs.len(), 1);
    }
    #[test]
    fn reconnect_applies_service_state_without_inventing_persisted_jobs() {
        let mut app = App::boot();
        app.apply(state("a", 10));
        let mut restarted = state("b", 0);
        restarted.jobs.clear();
        app.apply(restarted);
        assert!(app.jobs.is_empty());
        assert!(app.error.is_some());
    }
    #[test]
    fn disconnection_marks_state_unknown_without_sending_cancel() {
        let mut app = App::boot();
        app.connected = true;
        app.apply(state("a", 1));
        let _ = app.update(Message::Service(
            0,
            backend::Event::Disconnected("offline".into()),
        ));
        assert!(!app.connected);
        assert!(app.active());
        assert!(!app.jobs[0].record.cancelling);
    }
    #[test]
    fn old_subscriptions_and_mutation_replies_cannot_replace_new_session() {
        let mut app = App::boot();
        app.apply(state("b", 5));
        app.epoch = 2;
        app.pending = true;
        let _ = app.update(Message::Service(
            1,
            backend::Event::Snapshot(state("a", 20)),
        ));
        let _ = app.update(Message::ActionDone {
            epoch: 2,
            instance: "a".into(),
            result: Ok(state("a", 20)),
        });
        assert_eq!(app.snapshot.unwrap().instance, "b");
        assert!(app.pending);
    }
}
