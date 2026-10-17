//! Explicit local desktop requests. Never a shell, automatic launch or service RPC.
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub enum Target {
    File,
    Folder,
}

fn prepare(path: &Path, target: Target) -> Result<String, String> {
    if !path.is_absolute() {
        return Err("Only absolute local download paths can be opened.".into());
    }
    let canonical = match target {
        Target::Folder => {
            let parent = path
                .parent()
                .ok_or("The download has no containing folder.")?;
            let dir = parent
                .canonicalize()
                .map_err(|_| "The containing folder is missing or inaccessible.")?;
            if !dir.is_dir() {
                return Err("The containing folder is not a directory.".into());
            }
            dir
        }
        Target::File => {
            // Read only, no creation/truncation. A planted FIFO must not block;
            // validate the descriptor, not a metadata-then-open pathname check.
            let mut file = OpenOptions::new().read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path).map_err(|_| "The file is missing, inaccessible or a symbolic link. Use Open folder to inspect it.")?;
            rdm::safe_file::validate_regular_owned(&file)
                .map_err(|_| "Open file requires a regular, user-owned, single-link file. Use Open folder to inspect it.")?;
            let metadata = file
                .metadata()
                .map_err(|_| "Could not inspect the downloaded file.")?;
            let extension = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let mut prefix = Vec::new();
            (&mut file)
                .take(128)
                .read_to_end(&mut prefix)
                .map_err(|_| "Could not read the downloaded file.")?;
            if metadata.permissions().mode() & 0o111 != 0
                || matches!(
                    extension.as_str(),
                    "desktop"
                        | "appimage"
                        | "run"
                        | "sh"
                        | "bash"
                        | "zsh"
                        | "fish"
                        | "py"
                        | "pl"
                        | "rb"
                        | "exe"
                        | "msi"
                        | "bat"
                        | "cmd"
                        | "jar"
                )
                || prefix.starts_with(b"#!")
                || prefix.starts_with(b"\x7fELF")
                || prefix.starts_with(b"MZ")
                || prefix.windows(15).any(|p| p == b"[Desktop Entry]")
            {
                return Err("This file looks executable or contains a script/launcher marker; Open file is disabled. Use Open folder to inspect the file.".into());
            }
            let canonical = path
                .canonicalize()
                .map_err(|_| "The downloaded file moved or disappeared.")?;
            let current = std::fs::symlink_metadata(&canonical)
                .map_err(|_| "The downloaded file moved or disappeared.")?;
            if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
                return Err(
                    "The downloaded file changed while being checked. Inspect it before opening."
                        .into(),
                );
            }
            canonical
        }
    };
    // A single file URI argument escapes spaces, #, ?, %, newlines and Unicode;
    // no filename is ever interpreted as an option or passed through a shell.
    url::Url::from_file_path(canonical)
        .map(String::from)
        .map_err(|_| "Could not represent the local path as a file URI.".into())
}

async fn launch(program: &Path, uri: &str, limit: Duration) -> Result<(), String> {
    let mut child = tokio::process::Command::new(program)
        .arg(uri)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            "Could not start xdg-open. Install xdg-utils and check your desktop session.".to_owned()
        })?;
    match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(_) => Err(
            "The desktop opener failed. Check your default application or use your file manager."
                .into(),
        ),
        Err(_) => {
            // Reap our helper; a desktop application it delegated to is NOT
            // tracked/killed. A timeout is uncertain, so never retry for the user.
            let _ = child.kill().await;
            Err("The desktop opener timed out; the application may still have opened. Check before trying again.".into())
        }
    }
}

pub async fn open(path: String, target: Target) -> Result<(), String> {
    let uri = tokio::task::spawn_blocking(move || prepare(&PathBuf::from(path), target))
        .await
        .map_err(|_| "Could not inspect the local path.".to_owned())??;
    launch(Path::new("xdg-open"), &uri, Duration::from_secs(10)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn file_uri_is_local_and_escapes_untrusted_filename_characters() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("--option ;$(touch NO)#?%\n文.txt");
        std::fs::write(&path, b"safe plain text").unwrap();
        let uri = prepare(&path, Target::File).unwrap();
        let parsed = url::Url::parse(&uri).unwrap();
        assert_eq!(parsed.scheme(), "file");
        assert!(parsed.host_str().is_none());
        assert_eq!(parsed.to_file_path().unwrap(), path);
        assert!(!uri.contains('\n') && !uri.contains(' '));
        assert!(uri.contains("%23") && uri.contains("%3F") && uri.contains("%25"));
    }
    #[test]
    fn missing_relative_symlink_directory_and_fifo_files_are_refused_without_creation() {
        let root = tempfile::tempdir().unwrap();
        assert!(prepare(Path::new("relative.txt"), Target::File).is_err());
        let missing = root.path().join("missing.txt");
        assert!(prepare(&missing, Target::File).is_err());
        assert!(!missing.exists());
        assert!(prepare(root.path(), Target::File).is_err());
        let file = root.path().join("file.txt");
        std::fs::write(&file, b"text").unwrap();
        let link = root.path().join("link.txt");
        symlink(&file, &link).unwrap();
        assert!(prepare(&link, Target::File).is_err());
        let fifo = root.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(prepare(&fifo, Target::File).is_err());
        assert!(
            prepare(&missing, Target::Folder).is_ok(),
            "folder action still works if final file moved"
        );
        assert!(prepare(&root.path().join("absent/file.txt"), Target::Folder).is_err());
    }
    #[test]
    fn executable_scripts_and_launchers_are_not_handed_to_desktop() {
        let root = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("file.desktop", b"plain".as_slice()),
            ("script.txt", b"#!/bin/sh"),
            ("elf.txt", b"\x7fELF"),
            ("windows.txt", b"MZ"),
            ("launcher.txt", b"[Desktop Entry]"),
            ("script.py", b"print(1)"),
            ("tool.AppImage", b"data"),
        ] {
            let path = root.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(prepare(&path, Target::File).is_err(), "{name}");
            assert!(prepare(&path, Target::Folder).is_ok());
        }
        let path = root.path().join("executable.txt");
        std::fs::write(&path, b"plain").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(prepare(&path, Target::File).is_err());
    }
    fn helper(root: &Path, body: &str) -> PathBuf {
        let script = root.join("opener");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        script
    }
    #[tokio::test]
    async fn opener_gets_exactly_one_literal_uri_argument_and_reports_exit_status() {
        let root = tempfile::tempdir().unwrap();
        let record = root.path().join("argument");
        let script = helper(
            root.path(),
            &format!(
                "[ \"$#\" = 1 ] || exit 9\nprintf '%s' \"$1\" > '{}'",
                record.display()
            ),
        );
        let uri = "file:///tmp/--file%20%24%28touch%20NO%29.txt";
        launch(&script, uri, Duration::from_secs(2)).await.unwrap();
        assert_eq!(std::fs::read_to_string(record).unwrap(), uri);
        let script = helper(root.path(), "exit 4");
        assert!(launch(&script, uri, Duration::from_secs(2)).await.is_err());
        assert!(
            launch(&root.path().join("missing"), uri, Duration::from_secs(2))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn hanging_helper_times_out_without_automatic_retry() {
        let root = tempfile::tempdir().unwrap();
        let script = helper(root.path(), "exec sleep 3");
        let error = launch(&script, "file:///tmp/file", Duration::from_millis(40))
            .await
            .unwrap_err();
        assert!(error.contains("may still have opened"));
    }
}
