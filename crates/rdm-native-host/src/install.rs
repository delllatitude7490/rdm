//! Opt-in, per-user registration. No copying binaries, shell commands, sudo,
//! PATH changes, config edits or replacement of existing registration files.
use crate::{EXTENSION_ID, HOST_NAME};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn register() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or(io::ErrorKind::NotFound)?;
    register_at(Path::new(&home), &std::env::current_exe()?)
}

pub fn register_at(home: &Path, executable: &Path) -> io::Result<PathBuf> {
    if !home.is_absolute() || !executable.is_absolute() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let executable = fs::canonicalize(executable)?;
    let service = executable
        .parent()
        .ok_or(io::ErrorKind::InvalidInput)?
        .join("rdm-service");
    for binary in [&executable, &service] {
        let metadata = fs::metadata(binary)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
    }
    let directory = home.join(".mozilla/native-messaging-hosts");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{HOST_NAME}.json"));
    let executable_string = executable.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    let manifest = serde_json::json!({
        "name": HOST_NAME, "description": "RDM Firefox download bridge",
        "path": executable_string, "type": "stdio", "allowed_extensions": [EXTENSION_ID]
    });
    let mut contents = serde_json::to_vec_pretty(&manifest)?;
    contents.push(b'\n');
    // create_new refuses existing files AND symlinks; never blindly overwrite.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let result = file.write_all(&contents).and_then(|_| file.sync_all());
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result.map(|_| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_escapes_paths_restricts_id_and_never_overwrites() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin with spaces and \"quotes\"");
        fs::create_dir(&bin).unwrap();
        for name in ["rdm-native-host", "rdm-service"] {
            let path = bin.join(name);
            fs::write(&path, b"fake").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = register_at(root.path(), &bin.join("rdm-native-host")).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            value["allowed_extensions"],
            serde_json::json!([EXTENSION_ID])
        );
        assert_eq!(value["path"], bin.join("rdm-native-host").to_str().unwrap());
        fs::write(&path, b"keep me").unwrap();
        assert!(register_at(root.path(), &bin.join("rdm-native-host")).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep me");
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(bin.join("rdm-service"), &path).unwrap();
        assert!(register_at(root.path(), &bin.join("rdm-native-host")).is_err());
        assert_eq!(fs::read(bin.join("rdm-service")).unwrap(), b"fake");
    }
    #[test]
    fn registration_rejects_non_utf8_binary_path_without_panicking() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let binary = root
            .path()
            .join(std::ffi::OsString::from_vec(b"host-\xff".to_vec()));
        for path in [&binary, &root.path().join("rdm-service")] {
            fs::write(path, b"fake").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(
            register_at(root.path(), &binary).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(
            !root
                .path()
                .join(".mozilla/native-messaging-hosts/rdm_firefox.json")
                .exists()
        );
    }
    #[test]
    fn registration_requires_absolute_paths_and_sibling_service() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("host");
        fs::write(&host, b"fake").unwrap();
        assert!(register_at(Path::new("relative"), &host).is_err());
        assert!(register_at(root.path(), &host).is_err());
        assert!(!root.path().join(".mozilla").exists());
    }
}
