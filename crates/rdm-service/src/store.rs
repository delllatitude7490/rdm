//! Service-only durable state. All SQLite/fsync work runs off Tokio workers.
//! The bounded image is replaced atomically, never reset on corruption/errors.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rdm_ipc::{AddCli, Finished, Job, MAX_JOBS, runtime::uid};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub job: Job,
    // Intentionally no Debug implementation: this can contain signed URLs.
    pub input: Option<AddCli>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Image {
    pub next_id: u64,
    pub records: Vec<Record>,
}
struct Database {
    connection: Connection,
    _lock: File,
}
#[derive(Clone)]
pub(crate) struct Store(Arc<Mutex<Database>>);
fn invalid() -> io::Error {
    io::Error::other(
        "Service state is unsafe, incompatible, corrupt, or unavailable; it was not reset",
    )
}
fn db_error(_: rusqlite::Error) -> io::Error {
    // SQLite errors can include filenames or payloads. Never forward them.
    invalid()
}

pub(crate) fn discover() -> io::Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => dirs::home_dir().ok_or_else(invalid)?.join(".local/state"),
    };
    if !base.is_absolute() {
        return Err(invalid());
    }
    Ok(base.join("rdm/service"))
}

fn directory(path: &Path) -> io::Result<()> {
    // Do not follow symlink components, including an XDG_STATE_HOME override.
    // Existing non-private ancestors (e.g. /home) are OK; service directory
    // and its immediate parent must be private and user-owned.
    let mut current = PathBuf::new();
    for part in path.components() {
        if !matches!(part, Component::RootDir | Component::Normal(_)) {
            return Err(invalid());
        }
        current.push(part);
        match fs::DirBuilder::new().mode(0o700).create(&current) {
            Ok(()) => {
                if let Some(parent) = current.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let m = fs::symlink_metadata(&current)?;
        if !m.is_dir()
            || (m.uid() != uid() && m.uid() != 0)
            || (m.mode() & 0o022 != 0 && !(m.uid() == 0 && m.mode() & 0o1000 != 0))
        {
            return Err(invalid());
        }
    }
    for p in [path, path.parent().ok_or_else(invalid)?] {
        let m = fs::symlink_metadata(p)?;
        if m.uid() != uid() || m.mode() & 0o077 != 0 {
            return Err(invalid());
        }
    }
    Ok(())
}
fn private_file(path: &Path, create: bool) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != uid() || m.mode() & 0o077 != 0 || m.nlink() != 1 {
        return Err(invalid());
    }
    Ok(f)
}
impl Store {
    pub async fn open(path: PathBuf) -> io::Result<(Self, Image)> {
        tokio::task::spawn_blocking(move || {
            directory(&path)?;
            // Separate durable lock also excludes services in DIFFERENT runtime
            // directories. Never unlink its inode, even on shutdown.
            let lock = private_file(&path.join("state.lock"), true)?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                // Not WouldBlock: main uses that for the runtime's existing service.
                return Err(invalid());
            }
            let db_path = path.join("jobs.sqlite3");
            let file = private_file(&db_path, true)?;
            if file.metadata()?.len() > 16 * 1024 * 1024 { return Err(invalid()); }
            for suffix in ["-journal", "-wal", "-shm"] {
                let side = path.join(format!("jobs.sqlite3{suffix}"));
                match fs::symlink_metadata(&side) {
                    Ok(_) => { private_file(&side, false)?; },
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {},
                    Err(e) => return Err(e),
                }
            }
            let mut connection = Connection::open_with_flags(&db_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW).map_err(db_error)?;
            connection.busy_timeout(std::time::Duration::from_secs(2)).map_err(db_error)?;
            // DELETE avoids persistent WAL sidecars. FULL commits before ACK.
            connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;").map_err(db_error)?;
            let check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0)).map_err(db_error)?;
            if check != "ok" { return Err(invalid()); }
            let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(db_error)?;
            let image = if version == 0 {
                // Only a genuinely new/empty database may be initialized.
                let tables: i64 = connection.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0)).map_err(db_error)?;
                if tables != 0 { return Err(invalid()); }
                let image = Image { next_id: 1, records: vec![] };
                let tx = connection.transaction().map_err(db_error)?;
                tx.execute_batch("CREATE TABLE state (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL); PRAGMA user_version=1;").map_err(db_error)?;
                tx.execute("INSERT INTO state VALUES (1, ?1)", [serde_json::to_string(&image).map_err(|_| invalid())?]).map_err(db_error)?;
                tx.commit().map_err(db_error)?;
                // Persist newly created directory entries, too.
                File::open(&path)?.sync_all()?;
                if let Some(parent) = path.parent() { File::open(parent)?.sync_all()?; }
                image
            } else if version == 1 {
                let payload: String = connection.query_row("SELECT payload FROM state WHERE id=1", [], |r| r.get(0)).map_err(db_error)?;
                if payload.len() > 4 * 1024 * 1024 { return Err(invalid()); }
                serde_json::from_str::<Image>(&payload).map_err(|_| invalid())?
            } else { return Err(invalid()); };
            validate(&image)?;
            Ok((Self(Arc::new(Mutex::new(Database { connection, _lock: lock }))), image))
        }).await.map_err(|_| invalid())?
    }
    pub async fn save(&self, image: Image) -> io::Result<()> {
        validate(&image)?;
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let payload = serde_json::to_string(&image).map_err(|_| invalid())?;
            let mut db = db.lock().map_err(|_| invalid())?;
            let tx = db.connection.transaction().map_err(db_error)?;
            if tx
                .execute("UPDATE state SET payload=?1 WHERE id=1", params![payload])
                .map_err(db_error)?
                != 1
            {
                return Err(invalid());
            }
            tx.commit().map_err(db_error)
        })
        .await
        .map_err(|_| invalid())?
    }
}
fn validate(image: &Image) -> io::Result<()> {
    if image.records.len() > MAX_JOBS || image.next_id == 0 {
        return Err(invalid());
    }
    let mut ids = std::collections::HashSet::new();
    for r in &image.records {
        if r.job.id == 0
            || !ids.insert(r.job.id)
            || r.job.id >= image.next_id
            || r.job.label.len() > 255
            || r.job.destination.len() > 8192
            || (r.job.active() && r.input.is_none())
            || (r.job.retried_as.is_some() && (r.input.is_some() || r.job.active()))
            || r.job
                .retried_as
                .is_some_and(|id| id <= r.job.id || id >= image.next_id)
            || (r.job.queued && !r.job.active())
            || (r.job.retryable != (r.input.is_some() && !r.job.active()))
            || (matches!(
                r.job.result,
                Some(Finished::Completed { .. } | Finished::AlreadyPresent { .. })
            ) && r.input.is_some())
        {
            return Err(invalid());
        }
        if let Some(input) = &r.input
            && (input.url.len() > 16 * 1024
                || input.output != r.job.destination
                || !Path::new(&input.output).is_absolute()
                || !(1..=64).contains(&input.connections))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn image() -> Image {
        Image {
            next_id: 2,
            records: vec![Record {
                job: Job {
                    id: 1,
                    label: "file.bin".into(),
                    destination: "/tmp/file.bin".into(),
                    progress: Default::default(),
                    queued: true,
                    retryable: false,
                    retried_as: None,
                    cancelling: false,
                    result: None,
                },
                input: Some(AddCli {
                    url: "https://example.com/file.bin?token=DO_NOT_LEAK".into(),
                    output: "/tmp/file.bin".into(),
                    connections: 2,
                    allow_private: false,
                }),
            }],
        }
    }
    #[tokio::test]
    async fn round_trip_private_and_single_writer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rdm/service");
        let (store, initial) = Store::open(path.clone()).await.unwrap();
        assert!(initial.records.is_empty());
        store.save(image()).await.unwrap();
        assert!(
            Store::open(path.clone()).await.is_err(),
            "durable lock excludes other runtimes too"
        );
        let db = path.join("jobs.sqlite3");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&db).unwrap().mode() & 0o777, 0o600);
        drop(store);
        let (_store, loaded) = Store::open(path).await.unwrap();
        assert_eq!(loaded.next_id, 2);
        assert!(
            loaded.records[0]
                .input
                .as_ref()
                .unwrap()
                .url
                .contains("DO_NOT_LEAK")
        );
        assert!(
            !serde_json::to_string(&loaded.records[0].job)
                .unwrap()
                .contains("DO_NOT_LEAK")
        );
    }
    #[tokio::test]
    async fn refuses_symlinks_hardlinks_and_shared_state_without_reset() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rdm/service");
        let (store, _) = Store::open(path.clone()).await.unwrap();
        drop(store);
        let db = path.join("jobs.sqlite3");
        let original = fs::read(&db).unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Store::open(path.clone()).await.is_err());
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&db, path.join("other.sqlite3")).unwrap();
        assert!(Store::open(path.clone()).await.is_err());
        fs::remove_file(path.join("other.sqlite3")).unwrap();
        symlink(&db, path.join("jobs.sqlite3-journal")).unwrap();
        assert!(Store::open(path.clone()).await.is_err());
        fs::remove_file(path.join("jobs.sqlite3-journal")).unwrap();
        fs::rename(&db, path.join("real.sqlite3")).unwrap();
        symlink(path.join("real.sqlite3"), &db).unwrap();
        assert!(Store::open(path.clone()).await.is_err());
        assert_eq!(fs::read(&db).unwrap(), original);
        let alias = root.path().join("alias");
        symlink(root.path().join("rdm"), &alias).unwrap();
        assert!(Store::open(alias.join("service")).await.is_err());
    }
    #[tokio::test]
    async fn corrupt_and_future_schema_are_never_reset() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rdm/service");
        let (store, _) = Store::open(path.clone()).await.unwrap();
        drop(store);
        let db = path.join("jobs.sqlite3");
        let con = Connection::open(&db).unwrap();
        con.execute_batch("PRAGMA user_version=99").unwrap();
        drop(con);
        let original = fs::read(&db).unwrap();
        assert!(Store::open(path.clone()).await.is_err());
        assert_eq!(fs::read(&db).unwrap(), original);
        fs::write(&db, b"corrupt DO_NOT_LEAK").unwrap();
        let error = Store::open(path).await.err().unwrap();
        assert!(!error.to_string().contains("DO_NOT_LEAK"));
        assert_eq!(fs::read(db).unwrap(), b"corrupt DO_NOT_LEAK");
    }
    #[tokio::test]
    async fn failed_transaction_keeps_last_committed_image() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rdm/service");
        let (store, _) = Store::open(path.clone()).await.unwrap();
        store.save(image()).await.unwrap();
        // Real SQLITE_FULL injection, not a pretend error after committing.
        store
            .0
            .lock()
            .unwrap()
            .connection
            .execute_batch("PRAGMA max_page_count=2")
            .unwrap();
        let mut big = image();
        big.records[0].input.as_mut().unwrap().url =
            format!("https://example.com/{}", "x".repeat(15_000));
        assert!(store.save(big).await.is_err());
        drop(store);
        let (_, loaded) = Store::open(path).await.unwrap();
        assert!(
            loaded.records[0]
                .input
                .as_ref()
                .unwrap()
                .url
                .contains("DO_NOT_LEAK")
        );
    }
}
