use rdm_native_host::{EXTENSION_ID, Reply, dispatch, install, read_message, write_reply};

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--install" {
        match install::register() {
            Ok(path) => eprintln!("Firefox host registered at {}", path.display()),
            Err(_) => {
                eprintln!(
                    "Could not register Firefox host. Check HOME, matching binaries and existing manifest; no existing registration was replaced."
                );
                std::process::exit(1);
            }
        }
        return;
    }
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        eprintln!(
            "rdm-native-host --install\nRegister this binary for Firefox on Linux. Build/install rdm-service alongside it first.\nOtherwise invoked by Firefox Native Messaging, not as a normal terminal command."
        );
        return;
    }
    // Firefox passes manifest path, then the initiating add-on ID. This is a
    // defense in depth check, not authentication against same-user processes.
    if args.len() != 2 || args[1] != EXTENSION_ID {
        eprintln!("This bridge must be launched by the RDM Firefox extension.");
        std::process::exit(1);
    }
    let message = read_message(&mut std::io::stdin().lock());
    let reply = match message {
        Ok(message) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(dispatch(message)),
                Err(_) => Reply::new(false, "startup", "Native bridge could not initialize."),
            }
        }
        Err(_) => Reply::invalid(),
    };
    if write_reply(&mut std::io::stdout().lock(), &reply).is_err() {
        std::process::exit(1);
    }
}
