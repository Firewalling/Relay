use crate::model::{GitSnapshot, SCHEMA_VERSION};
use std::{
    io,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("could not run Git in {directory}: {source}")]
    Spawn {
        directory: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Git command failed in {directory}: {message}")]
    Command { directory: PathBuf, message: String },
    #[error("Git returned non-UTF-8 output in {directory}")]
    NonUtf8 { directory: PathBuf },
}

pub fn capture(directory: &Path) -> Result<GitSnapshot, GitError> {
    let Some(root) = repository_root(directory)? else {
        return Ok(GitSnapshot::unavailable());
    };

    let branch = optional_text(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let commit = optional_text(&root, &["rev-parse", "--verify", "HEAD"])?;
    let status = required_output(
        &root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let changed_files = parse_porcelain_z(&status.stdout, &root)?;

    Ok(GitSnapshot {
        schema_version: SCHEMA_VERSION,
        available: true,
        repository_root: Some(root.to_string_lossy().into_owned()),
        branch,
        commit,
        clean: Some(changed_files.is_empty()),
        changed_files,
    })
}

fn repository_root(directory: &Path) -> Result<Option<PathBuf>, GitError> {
    let output = run(directory, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = decode_trimmed(output.stdout, directory)?;
    Ok(Some(PathBuf::from(text)))
}

fn optional_text(directory: &Path, args: &[&str]) -> Result<Option<String>, GitError> {
    let output = run(directory, args)?;
    if output.status.success() {
        decode_trimmed(output.stdout, directory).map(Some)
    } else {
        Ok(None)
    }
}

fn required_output(directory: &Path, args: &[&str]) -> Result<Output, GitError> {
    let output = run(directory, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitError::Command {
            directory: directory.to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn run(directory: &Path, args: &[&str]) -> Result<Output, GitError> {
    Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|source| GitError::Spawn {
            directory: directory.to_owned(),
            source,
        })
}

fn decode_trimmed(bytes: Vec<u8>, directory: &Path) -> Result<String, GitError> {
    String::from_utf8(bytes)
        .map(|value| value.trim_end().to_owned())
        .map_err(|_| GitError::NonUtf8 {
            directory: directory.to_owned(),
        })
}

fn parse_porcelain_z(bytes: &[u8], directory: &Path) -> Result<Vec<String>, GitError> {
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut paths = Vec::new();

    while let Some(record) = fields.next() {
        if record.len() < 4 {
            return Err(GitError::Command {
                directory: directory.to_owned(),
                message: "malformed porcelain status output".into(),
            });
        }
        let status = &record[..2];
        let path = String::from_utf8(record[3..].to_vec()).map_err(|_| GitError::NonUtf8 {
            directory: directory.to_owned(),
        })?;

        if matches!(status[0], b'R' | b'C') || matches!(status[1], b'R' | b'C') {
            fields.next().ok_or_else(|| GitError::Command {
                directory: directory.to_owned(),
                message: "malformed rename in porcelain status output".into(),
            })?;
            paths.push(path);
        } else {
            paths.push(path);
        }
    }

    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(directory: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(directory)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn outside_repository_is_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = capture(directory.path()).unwrap();
        assert!(!snapshot.available);
        assert_eq!(snapshot.clean, None);
    }

    #[test]
    fn captures_changed_files_including_spaces() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"]);
        fs::write(directory.path().join("changed file.txt"), "new").unwrap();

        let snapshot = capture(directory.path()).unwrap();
        assert!(snapshot.available);
        assert_eq!(snapshot.clean, Some(false));
        assert_eq!(snapshot.changed_files, ["changed file.txt"]);
    }

    #[test]
    fn parses_rename_records_and_sorts_paths() {
        let paths = parse_porcelain_z(
            b"?? zed.txt\0R  renamed.txt\0original.txt\0 M alpha.txt\0",
            Path::new("."),
        )
        .unwrap();

        assert_eq!(paths, ["alpha.txt", "renamed.txt", "zed.txt"]);
    }

    #[test]
    fn consumes_second_path_when_index_or_worktree_reports_rename_or_copy() {
        for status in [b"R ".as_slice(), b" R", b"C ", b" C"] {
            let mut porcelain = status.to_vec();
            porcelain.extend_from_slice(b" destination.txt\0source.txt\0?? other.txt\0");

            let paths = parse_porcelain_z(&porcelain, Path::new(".")).unwrap();

            assert_eq!(paths, ["destination.txt", "other.txt"]);
        }
    }

    #[test]
    fn rejects_truncated_rename_records() {
        for status in [b"R ", b" R", b"C ", b" C"] {
            let mut porcelain = status.to_vec();
            porcelain.extend_from_slice(b" renamed.txt\0");
            let error = parse_porcelain_z(&porcelain, Path::new(".")).unwrap_err();
            assert!(matches!(error, GitError::Command { .. }));
            assert!(error.to_string().contains("malformed rename"));
        }
    }
}
