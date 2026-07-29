use crate::{
    model::{LockOwner, SCHEMA_VERSION},
    storage,
};
use chrono::Utc;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
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
        fs::create_dir_all(&relay).map_err(|source| LockError::Io {
            path: relay.clone(),
            source,
        })?;
        let path = relay.join("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn acquire_error(project: &Path, command: &str) -> LockError {
        match ProjectLock::acquire(project, command) {
            Ok(_) => panic!("expected acquiring the lock to fail"),
            Err(error) => error,
        }
    }

    #[test]
    fn acquiring_creates_state_and_records_owner_metadata() {
        let directory = tempfile::tempdir().unwrap();

        let _lock = ProjectLock::acquire(directory.path(), "checkpoint").unwrap();

        let path = directory.path().join(".relay/lock");
        let owner = storage::read_json::<LockOwner>(&path).unwrap();
        assert_eq!(owner.schema_version, SCHEMA_VERSION);
        assert_eq!(owner.pid, std::process::id());
        assert_eq!(owner.command, "checkpoint");
        assert!(owner.acquired_at <= Utc::now());
    }

    #[test]
    fn contention_reports_the_owning_process_and_clears_after_release() {
        let directory = tempfile::tempdir().unwrap();
        let held = ProjectLock::acquire(directory.path(), "start").unwrap();

        let error = acquire_error(directory.path(), "note");
        assert!(matches!(error, LockError::Busy { .. }));
        let message = error.to_string();
        assert!(message.contains(&format!("PID {}", std::process::id())));
        assert!(message.contains("command `start`"));
        assert!(message.contains("retry after the owning process finishes"));

        drop(held);
        let reacquired = ProjectLock::acquire(directory.path(), "note").unwrap();
        assert_eq!(
            storage::read_json::<LockOwner>(&directory.path().join(".relay/lock"))
                .unwrap()
                .command,
            "note"
        );
        drop(reacquired);
    }

    #[test]
    fn contention_stays_actionable_when_owner_metadata_is_unreadable() {
        let directory = tempfile::tempdir().unwrap();
        let _held = ProjectLock::acquire(directory.path(), "start").unwrap();
        fs::write(directory.path().join(".relay/lock"), "not json").unwrap();

        let error = acquire_error(directory.path(), "note");

        assert!(matches!(&error, LockError::Busy { owner } if owner.is_empty()));
        assert!(error.to_string().starts_with("Relay project is busy;"));
    }

    #[test]
    fn an_unusable_lock_path_is_reported_with_the_offending_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(".relay"), "not a directory").unwrap();

        let error = acquire_error(directory.path(), "start");

        assert!(matches!(error, LockError::Io { .. }));
        assert!(error.to_string().contains(".relay"));
    }

    #[test]
    fn a_lock_file_that_cannot_be_opened_is_reported_with_its_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join(".relay/lock")).unwrap();

        let error = acquire_error(directory.path(), "start");

        assert!(matches!(error, LockError::Io { .. }));
        assert!(error.to_string().contains(".relay/lock"));
    }
}
