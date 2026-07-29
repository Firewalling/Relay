use crate::{
    model::{LockOwner, SCHEMA_VERSION},
    storage,
};
use chrono::Utc;
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LockError {
    #[error("Relay project is busy{owner}; retry after the owning process finishes")]
    Busy { owner: String },
    #[error("could not access project lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Storage(#[from] storage::StorageError),
}

pub struct ProjectLock {
    file: File,
}

impl ProjectLock {
    pub fn acquire(project: &Path, command: &str) -> Result<Self, LockError> {
        let relay = project.join(".relay");
        storage::create_dir_all_private(&relay)?;
        let path = relay.join("lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|source| LockError::Io {
            path: path.clone(),
            source,
        })?;
        storage::restrict_existing(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                let owner = storage::read_json::<LockOwner>(&path)
                    .ok()
                    .map(|owner| format!(" (PID {}, command `{}`)", owner.pid, owner.command))
                    .unwrap_or_default();
                return Err(LockError::Busy { owner });
            }
            Err(source) => return Err(LockError::Io { path, source }),
        }
        let owner = LockOwner {
            schema_version: SCHEMA_VERSION,
            pid: std::process::id(),
            command: command.to_owned(),
            acquired_at: Utc::now(),
        };
        storage::write_json_to_open_file(&file, &path, &owner)?;
        #[cfg(debug_assertions)]
        if let Ok(milliseconds) = std::env::var("RELAY_TEST_HOLD_LOCK_MS")
            && let Ok(milliseconds) = milliseconds.parse::<u64>()
        {
            std::thread::sleep(std::time::Duration::from_millis(milliseconds));
        }
        Ok(Self { file })
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}
