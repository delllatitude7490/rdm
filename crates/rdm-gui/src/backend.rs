//! The GUI owns a subscription, never a transfer or a cancellation token.
use iced::futures::{SinkExt, Stream};
use rdm_ipc::{Snapshot, client};

#[derive(Debug, Clone)]
pub enum Event {
    Connecting,
    Snapshot(Snapshot),
    Disconnected(String),
}

pub fn events(epoch: &u64) -> impl Stream<Item = (u64, Event)> + use<> {
    let epoch = *epoch;
    iced::stream::channel(
        8,
        async move |mut output: iced::futures::channel::mpsc::Sender<(u64, Event)>| {
            if output.send((epoch, Event::Connecting)).await.is_err() {
                return;
            }
            let mut startup_error = client::ensure_running().await.err().map(|e| e.to_string());
            loop {
                match client::subscribe().await {
                    Ok(mut feed) => {
                        startup_error = None;
                        if output
                            .send((epoch, Event::Snapshot(feed.initial.clone())))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        loop {
                            match feed.next().await {
                                Ok(snapshot) => {
                                    if output
                                        .send((epoch, Event::Snapshot(snapshot)))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Err(error) => {
                                    if output
                                        .send((epoch, Event::Disconnected(error.to_string())))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let message = startup_error.clone().unwrap_or_else(|| error.to_string());
                        if output
                            .send((epoch, Event::Disconnected(message)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                // Reconnect read-only. A stopped service is NOT automatically restarted
                // in a loop; the user can explicitly choose Reconnect/start service.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        },
    )
}
