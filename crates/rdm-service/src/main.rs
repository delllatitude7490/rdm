use rdm_ipc::{Request, client, runtime::Paths};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    if run().await.is_err() {
        std::process::exit(1);
    }
}

async fn run() -> Result<(), ()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {
            let paths = Paths::discover().map_err(|_| eprintln!("Cannot start RDM: set XDG_RUNTIME_DIR to a private, user-owned runtime directory."))?;
            let config = rdm::config::Config::load().map_err(|_| {
                eprintln!(
                    "Cannot load RDM configuration. Fix the existing config; it has not been reset."
                )
            })?;
            let stop = CancellationToken::new();
            let signal_stop = stop.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                if let (Ok(mut term), Ok(mut interrupt)) = (
                    signal(SignalKind::terminate()),
                    signal(SignalKind::interrupt()),
                ) {
                    tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} }
                    signal_stop.cancel();
                }
            });
            match rdm_service::server::serve(paths, config, stop).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    eprintln!("RDM service is already running.");
                    Ok(())
                }
                Err(_) => {
                    eprintln!(
                        "RDM service could not run. Check private runtime/state paths, permissions, state integrity, and configuration."
                    );
                    Err(())
                }
            }
        }
        [arg] if arg == "--status" => {
            let snapshot = client::request(Request::List)
                .await
                .map_err(|e| eprintln!("{e}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot).map_err(|_| ())?
            );
            Ok(())
        }
        [arg] if arg == "--shutdown" => {
            let snapshot = client::request(Request::List)
                .await
                .map_err(|e| eprintln!("{e}"))?;
            client::request(Request::Shutdown {
                instance: snapshot.instance,
            })
            .await
            .map_err(|e| eprintln!("{e}"))?;
            println!(
                "Shutdown requested. The service will cancel and drain active work before exiting."
            );
            Ok(())
        }
        [arg] if arg == "--help" || arg == "-h" => {
            println!(
                "Usage: rdm-service [--status | --shutdown | --help]\nNo arguments: run the per-user download service."
            );
            Ok(())
        }
        _ => {
            eprintln!("Usage: rdm-service [--status | --shutdown | --help]");
            Err(())
        }
    }
}
