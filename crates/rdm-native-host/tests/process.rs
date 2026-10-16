use rdm_ipc::{ErrorCode, Job, Packet, Request, Response, Snapshot};
use rdm_native_host::{EXTENSION_ID, Reply};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}
fn run(root: &tempfile::TempDir, payload: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rdm-native-host"))
        .args(["/unused/rdm_firefox.json", EXTENSION_ID])
        .env("XDG_RUNTIME_DIR", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(payload).unwrap();
    drop(input);
    child.wait_with_output().unwrap()
}
fn frame(value: serde_json::Value) -> Vec<u8> {
    let bytes = serde_json::to_vec(&value).unwrap();
    [(bytes.len() as u32).to_ne_bytes().as_slice(), &bytes].concat()
}
fn reply(output: &std::process::Output) -> Reply {
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let bytes = &output.stdout;
    assert!(bytes.len() >= 4);
    let len = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(
        len + 4,
        bytes.len(),
        "stdout must contain only one native frame"
    );
    serde_json::from_slice(&bytes[4..]).unwrap()
}
fn state() -> Snapshot {
    Snapshot {
        instance: "fake".into(),
        revision: 1,
        default_directory: "/tmp/service-default".into(),
        shutting_down: false,
        jobs: vec![],
    }
}

#[test]
fn status_does_not_spawn_service_and_validation_does_not_echo_input() {
    let root = root();
    let output = run(
        &root,
        &frame(serde_json::json!({"version":1,"command":{"action":"status"}})),
    );
    assert_eq!(reply(&output).code, "unavailable");
    assert!(!root.path().join("rdm").exists());
    for payload in [
        frame(serde_json::json!({"version":1,"command":{"action":"shutdown"}})),
        frame(
            serde_json::json!({"version":1,"command":{"action":"add","url":"secret","directory":"/etc"}}),
        ),
        frame(serde_json::json!({"version":1,"command":{"action":"add","url":"x".repeat(16385)}})),
        ((rdm_native_host::MAX_INPUT + 1) as u32)
            .to_ne_bytes()
            .to_vec(),
    ] {
        let output = run(&root, &payload);
        assert_eq!(reply(&output).code, "invalid");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
    }
    assert_eq!(
        reply(&run(
            &root,
            &frame(serde_json::json!({"version":2,"command":{"action":"status"}}))
        ))
        .code,
        "version"
    );
}

#[test]
fn actual_host_frames_translate_url_only_and_never_retry_mutations() {
    for mode in ["accepted", "busy", "lost"] {
        let root = root();
        let dir = root.path().join("rdm");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            for index in 0..3 {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut header = [0; 4];
                stream.read_exact(&mut header).unwrap();
                let len = u32::from_be_bytes(header) as usize;
                assert!(len <= rdm_ipc::MAX_FRAME);
                let mut bytes = vec![0; len];
                stream.read_exact(&mut bytes).unwrap();
                let request: Packet<Request> = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(request.version, rdm_ipc::VERSION);
                let mut snapshot = state();
                if index < 2 {
                    assert!(matches!(request.body, Request::List));
                } else {
                    match request.body {
                        Request::Add(add) => {
                            assert_eq!(add.url, "https://example.com/f?token=secret");
                            assert_eq!(add.directory, "/tmp/service-default");
                            assert!(add.filename.is_empty());
                        }
                        _ => panic!("expected Add"),
                    }
                    snapshot.jobs.push(Job {
                        id: 7,
                        label: "file".into(),
                        destination: "/tmp/service-default/file".into(),
                        progress: Default::default(),
                        cancelling: false,
                        result: None,
                    });
                    if mode == "lost" {
                        drop(stream);
                        break;
                    }
                }
                let response = if index == 2 && mode == "busy" {
                    Response::Error {
                        code: ErrorCode::Busy,
                        message: "secret".into(),
                    }
                } else {
                    Response::Snapshot(snapshot)
                };
                let bytes = serde_json::to_vec(&Packet::new(response)).unwrap();
                stream
                    .write_all(&(bytes.len() as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(&bytes).unwrap();
            }
            // Any automatically retried Add would create another connection.
            let deadline = Instant::now() + Duration::from_millis(150);
            while Instant::now() < deadline {
                assert!(
                    matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock)
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let output = run(
            &root,
            &frame(
                serde_json::json!({"version":1,"command":{"action":"add","url":"https://example.com/f?token=secret"}}),
            ),
        );
        let response = reply(&output);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("service-default"));
        assert_eq!(
            response.code,
            if mode == "lost" { "uncertain" } else { mode }
        );
        if mode == "accepted" {
            assert_eq!(response.job_id, Some(7));
        }
        server.join().unwrap();
    }
}
#[test]
fn wrong_extension_id_is_rejected_before_reading_stdin() {
    let output = Command::new(env!("CARGO_BIN_EXE_rdm-native-host"))
        .args(["/unused/manifest.json", "other@extension"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
