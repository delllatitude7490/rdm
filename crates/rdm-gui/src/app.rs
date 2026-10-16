use std::time::Duration;

use iced::widget::{button, column, container, progress_bar, row, scrollable, text, text_input};
use iced::{Element, Fill, Subscription, Task};
use rdm::config::Config;
use rdm::secret_url::SecretUrl;
use rdm::ui::{self, Rate, SlotState};
use tokio_util::sync::CancellationToken;

use crate::backend::{self, Finished, GuiSink, Snapshot};
use crate::theme::{ACCENT, ERROR, MUTED};

pub struct App {
    config: Option<Config>,
    url: SecretUrl,
    directory: String,
    filename: String,
    error: Option<String>,
    job: Option<Job>,
    next_id: u64,
    closing: bool,
}

struct Job {
    id: u64,
    label: String,
    sink: GuiSink,
    snapshot: Snapshot,
    cancel: CancellationToken,
    result: Option<Finished>,
    rate: Rate,
    speed: Option<u64>,
}

impl Job {
    fn active(&self) -> bool {
        self.result.is_none()
    }

    fn refresh(&mut self) {
        self.snapshot = self.sink.snapshot();
        self.rate.record(self.snapshot.downloaded);
        self.speed = self.rate.per_second();
    }

    fn status(&self) -> &str {
        match &self.result {
            Some(Finished::Completed { .. }) => "Completed",
            Some(Finished::AlreadyPresent { .. }) => "Already exists — file left unchanged",
            Some(Finished::Cancelled) => "Cancelled",
            Some(Finished::Failed(_)) => "Failed",
            None if self.cancel.is_cancelled() => "Cancelling safely…",
            None => match self.snapshot.phase {
                SlotState::Waiting => "Preparing",
                SlotState::Inspecting => "Connecting",
                SlotState::Downloading => "Downloading",
                SlotState::Finishing => "Saving",
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
    Tick,
    Cancel,
    Finished(u64, Finished),
    CloseRequested,
}

impl App {
    pub fn boot() -> Self {
        let (config, directory, error) = match Config::load() {
            Ok(config) => {
                let directory = config.download_dir.clone();
                rdm::engine::set_shared_config(config.clone());
                (Some(config), directory, None)
            }
            Err(_) => (
                None,
                String::new(),
                Some(format!(
                    "Could not load {}. Fix the configuration and restart; your settings have not been reset.",
                    rdm::config::config_path().display()
                )),
            ),
        };
        Self {
            config,
            url: SecretUrl::new(""),
            directory,
            filename: String::new(),
            error,
            job: None,
            next_id: 1,
            closing: false,
        }
    }

    fn active(&self) -> bool {
        self.job.as_ref().is_some_and(Job::active)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::UrlChanged(value) if !self.active() && !self.closing => self.url = value,
            Message::DirectoryChanged(value) if !self.active() && !self.closing => {
                self.directory = value
            }
            Message::FilenameChanged(value) if !self.active() && !self.closing => {
                self.filename = value
            }
            Message::Start if !self.active() && !self.closing => {
                let Some(config) = &self.config else {
                    return Task::none();
                };
                let input = match backend::prepare(
                    self.url.as_str(),
                    &self.directory,
                    &self.filename,
                    config.connections,
                ) {
                    Ok(input) => input,
                    Err(error) => {
                        self.error = Some(error);
                        return Task::none();
                    }
                };
                let id = self.next_id;
                self.next_id += 1;
                let sink = GuiSink::default();
                let cancel = CancellationToken::new();
                self.error = None;
                self.job = Some(Job {
                    id,
                    label: input.label.clone(),
                    sink: sink.clone(),
                    snapshot: Snapshot::default(),
                    cancel: cancel.clone(),
                    result: None,
                    rate: Rate::new(),
                    speed: None,
                });
                return Task::perform(backend::download(input, sink, cancel), move |result| {
                    Message::Finished(id, result)
                });
            }
            Message::Tick => {
                if let Some(job) = &mut self.job
                    && job.active()
                {
                    job.refresh();
                }
            }
            Message::Cancel => {
                if let Some(job) = &self.job
                    && job.active()
                {
                    job.cancel.cancel();
                }
            }
            Message::Finished(id, result) => {
                if let Some(job) = &mut self.job
                    && job.id == id
                    && job.active()
                {
                    job.refresh();
                    if let Finished::Completed { bytes, .. } = &result {
                        job.snapshot.downloaded = *bytes;
                        job.snapshot.total = Some(*bytes);
                    }
                    job.result = Some(result);
                    job.speed = None;
                    if self.closing {
                        return iced::exit();
                    }
                }
            }
            Message::CloseRequested => {
                self.closing = true;
                if let Some(job) = &self.job
                    && job.active()
                {
                    job.cancel.cancel();
                } else {
                    return iced::exit();
                }
            }
            _ => {}
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let close = iced::window::close_requests().map(|_| Message::CloseRequested);
        if self.active() {
            Subscription::batch([
                close,
                iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick),
            ])
        } else {
            // No timer, polling loop or continuous animation when idle.
            close
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let editable = !self.active() && !self.closing && self.config.is_some();
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
            text("NATIVE PREVIEW").size(11).color(MUTED),
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
        if let Some(job) = &self.job {
            content = content.push(self.job_view(job));
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
        let footer = if self.closing {
            "Closing after cancellation finishes. Waiting for the engine to release files safely…"
        } else {
            "Single-download preview · Closing cancels active work · No background service yet"
        };
        content = content.push(text(footer).size(12).color(MUTED));
        container(scrollable(content).height(Fill))
            .padding(28)
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn job_view<'a>(&self, job: &'a Job) -> Element<'a, Message> {
        let snapshot = &job.snapshot;
        let mut heading = row![
            column![
                text(&job.label).size(18),
                text(job.status()).size(12).color(MUTED)
            ]
            .spacing(5),
            iced::widget::space().width(Fill),
        ]
        .align_y(iced::Alignment::Center);
        if job.active() && !job.cancel.is_cancelled() {
            heading = heading.push(
                button("Cancel")
                    .on_press(Message::Cancel)
                    .height(44)
                    .padding([8, 16])
                    .style(button::secondary),
            );
        }
        let mut card = column![heading].spacing(14);
        if !matches!(job.result, Some(Finished::AlreadyPresent { .. })) {
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
        if snapshot.has_notice && job.active() {
            card = card.push(
                text("The server reported a notice; RDM may be retrying.")
                    .size(12)
                    .color(MUTED),
            );
        }
        match &job.result {
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

    fn test_app() -> App {
        App {
            config: Some(Config::default()),
            url: SecretUrl::new(""),
            directory: "/tmp".into(),
            filename: String::new(),
            error: None,
            job: Some(Job {
                id: 1,
                label: "file".into(),
                sink: GuiSink::default(),
                snapshot: Snapshot::default(),
                cancel: CancellationToken::new(),
                result: None,
                rate: Rate::new(),
                speed: None,
            }),
            next_id: 2,
            closing: false,
        }
    }

    #[test]
    fn close_requests_cancel_without_discarding_the_active_job() {
        let mut app = test_app();
        let _ = app.update(Message::CloseRequested);
        assert!(app.closing);
        assert!(app.active());
        assert!(app.job.as_ref().unwrap().cancel.is_cancelled());
    }

    #[test]
    fn stale_completion_cannot_finish_a_newer_job() {
        let mut app = test_app();
        let _ = app.update(Message::Finished(0, Finished::Cancelled));
        assert!(app.active());
        let _ = app.update(Message::Finished(1, Finished::Cancelled));
        assert!(!app.active());
    }
}
