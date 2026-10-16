//! Linux-only runtime paths; no world-writable /tmp fallback.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "RDM runtime path is not private and owned by this user",
    )
}

pub fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn private_directory(path: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != uid() || m.mode() & 0o077 != 0 {
        return Err(denied());
    }
    Ok(())
}

impl Paths {
    pub fn discover() -> io::Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "XDG_RUNTIME_DIR must name a private runtime directory",
            )
        })?;
        Self::in_runtime(Path::new(&runtime))
    }
    pub fn in_runtime(runtime: &Path) -> io::Result<Self> {
        if !runtime.is_absolute() {
            return Err(denied());
        }
        private_directory(runtime)?;
        let directory = runtime.join("rdm");
        Ok(Self {
            socket: directory.join("service.sock"),
            lock: directory.join("service.lock"),
            directory,
        })
    }
    pub fn validate_socket(&self) -> io::Result<()> {
        private_directory(&self.directory)?;
        let m = fs::symlink_metadata(&self.socket)?;
        if !m.file_type().is_socket() || m.uid() != uid() || m.mode() & 0o077 != 0 {
            return Err(denied());
        }
        Ok(())
    }
}

/// The lock file is never unlinked: unlinking an flock inode permits split-brain.
/// A stale socket is removed only AFTER acquiring this process-lifetime lock.
pub struct Instance {
    _lock: File,
    socket: PathBuf,
    identity: Option<(u64, u64)>,
}

impl Instance {
    pub fn acquire(paths: &Paths) -> io::Result<Self> {
        match fs::DirBuilder::new().mode(0o700).create(&paths.directory) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        private_directory(&paths.directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&paths.lock)?;
        let m = lock.metadata()?;
        if !m.is_file() || m.uid() != uid() || m.mode() & 0o077 != 0 || m.nlink() != 1 {
            return Err(denied());
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        match fs::symlink_metadata(&paths.socket) {
            Ok(m) if m.file_type().is_socket() && m.uid() == uid() => {
                fs::remove_file(&paths.socket)?
            }
            Ok(_) => return Err(denied()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Self {
            _lock: lock,
            socket: paths.socket.clone(),
            identity: None,
        })
    }
    pub fn bound(&mut self) -> io::Result<()> {
        fs::set_permissions(&self.socket, fs::Permissions::from_mode(0o600))?;
        let m = fs::symlink_metadata(&self.socket)?;
        self.identity = Some((m.dev(), m.ino()));
        Ok(())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if let Ok(m) = fs::symlink_metadata(&self.socket)
            && self.identity == Some((m.dev(), m.ino()))
            && m.file_type().is_socket()
        {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_shared_runtime_and_symlinks() {
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Paths::in_runtime(runtime.path()).is_err());
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths::in_runtime(runtime.path()).unwrap();
        std::os::unix::fs::symlink(runtime.path(), &paths.directory).unwrap();
        assert!(Instance::acquire(&paths).is_err());
    }
    #[test]
    fn lock_is_single_owner_and_does_not_delete_foreign_paths() {
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths::in_runtime(runtime.path()).unwrap();
        let first = Instance::acquire(&paths).unwrap();
        assert_eq!(
            Instance::acquire(&paths).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        fs::write(&paths.socket, b"do not delete").unwrap();
        assert!(Instance::acquire(&paths).is_err());
        assert_eq!(fs::read(&paths.socket).unwrap(), b"do not delete");
    }
}
