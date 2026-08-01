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
    let project_prefix = directory.strip_prefix(&root).unwrap_or(Path::new(""));

    let branch = optional_text(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let commit = optional_text(&root, &["rev-parse", "--verify", "HEAD"])?;
    let mut status_command = Command::new("git");
    status_command
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
        ])
        .arg(if project_prefix.as_os_str().is_empty() {
            Path::new(".")
        } else {
            project_prefix
        })
        .arg({
            let relay = if project_prefix.as_os_str().is_empty() {
                ".relay".into()
            } else {
                project_prefix.join(".relay")
            };
            format!(":(exclude,literal){}", relay.to_string_lossy())
        })
        .current_dir(&root);
    let status = status_command.output().map_err(|source| GitError::Spawn {
        directory: root.clone(),
        source,
    })?;
    if !status.status.success() {
        return Err(GitError::Command {
            directory: root.clone(),
            message: String::from_utf8_lossy(&status.stderr).trim().to_owned(),
        });
    }
    let (changed_files, escaped) = parse_porcelain_z(&status.stdout, &root, project_prefix)?;

    Ok(GitSnapshot {
        schema_version: SCHEMA_VERSION,
        available: true,
        repository_root: Some(if project_prefix.as_os_str().is_empty() {
            ".".into()
        } else {
            std::iter::repeat_n("..", project_prefix.components().count())
                .collect::<Vec<_>>()
                .join("/")
        }),
        branch,
        commit,
        clean: Some(changed_files.is_empty()),
        changed_files,
        git_path_encoding: escaped.then(|| "escaped_bytes_v1".into()),
    })
}

pub fn repository_root(directory: &Path) -> Result<Option<PathBuf>, GitError> {
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

fn parse_porcelain_z(
    bytes: &[u8],
    directory: &Path,
    project_prefix: &Path,
) -> Result<(Vec<String>, bool), GitError> {
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut paths = Vec::new();
    let mut escaped = false;

    while let Some(record) = fields.next() {
        if record.len() < 4 {
            return Err(GitError::Command {
                directory: directory.to_owned(),
                message: "malformed porcelain status output".into(),
            });
        }
        let status = &record[..2];
        let raw_path = strip_path_prefix(&record[3..], project_prefix);
        let (path, path_escaped) = display_git_path(raw_path);
        escaped |= path_escaped;

        if matches!(status[0], b'R' | b'C') || matches!(status[1], b'R' | b'C') {
            fields.next().ok_or_else(|| GitError::Command {
                directory: directory.to_owned(),
                message: "malformed rename in porcelain status output".into(),
            })?;
        }
        paths.push(path);
    }

    paths.sort();
    paths.dedup();
    Ok((paths, escaped))
}

fn strip_path_prefix<'a>(path: &'a [u8], prefix: &Path) -> &'a [u8] {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let prefix = prefix.as_os_str().as_bytes();
        if !prefix.is_empty() && path.starts_with(prefix) && path.get(prefix.len()) == Some(&b'/') {
            return &path[prefix.len() + 1..];
        }
    }
    path
}

fn display_git_path(path: &[u8]) -> (String, bool) {
    match std::str::from_utf8(path) {
        Ok(path) => (path.to_owned(), false),
        Err(_) => {
            let mut rendered = String::new();
            for &byte in path {
                if byte.is_ascii_graphic() && byte != b'\\' {
                    rendered.push(char::from(byte));
                } else {
                    rendered.push_str(&format!("\\x{byte:02X}"));
                }
            }
            (rendered, true)
        }
    }
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

    #[cfg(unix)]
    #[test]
    fn captures_an_actual_non_utf8_worktree_path() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"]);
        let name = OsString::from_vec(b"invalid-\xFF.txt".to_vec());
        fs::write(directory.path().join(name), "new").unwrap();

        let snapshot = capture(directory.path()).unwrap();

        assert_eq!(snapshot.changed_files, [r"invalid-\xFF.txt"]);
        assert_eq!(
            snapshot.git_path_encoding.as_deref(),
            Some("escaped_bytes_v1")
        );
    }

    #[test]
    fn parses_rename_records_and_sorts_paths() {
        let (paths, _) = parse_porcelain_z(
            b"?? zed.txt\0R  renamed.txt\0original.txt\0 M alpha.txt\0",
            Path::new("."),
            Path::new(""),
        )
        .unwrap();

        assert_eq!(paths, ["alpha.txt", "renamed.txt", "zed.txt"]);
    }

    #[test]
    fn consumes_second_path_when_index_or_worktree_reports_rename_or_copy() {
        for status in [b"R ".as_slice(), b" R", b"C ", b" C"] {
            let mut porcelain = status.to_vec();
            porcelain.extend_from_slice(b" destination.txt\0source.txt\0?? other.txt\0");

            let (paths, _) = parse_porcelain_z(&porcelain, Path::new("."), Path::new("")).unwrap();

            assert_eq!(paths, ["destination.txt", "other.txt"]);
        }
    }

    #[test]
    fn rejects_truncated_rename_records() {
        for status in [b"R ", b" R", b"C ", b" C"] {
            let mut porcelain = status.to_vec();
            porcelain.extend_from_slice(b" renamed.txt\0");
            let error = parse_porcelain_z(&porcelain, Path::new("."), Path::new("")).unwrap_err();
            assert!(matches!(error, GitError::Command { .. }));
            assert!(error.to_string().contains("malformed rename"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn escapes_non_utf8_paths_without_loss() {
        let (paths, escaped) =
            parse_porcelain_z(b"?? bad-\xFF-name\0", Path::new("."), Path::new("")).unwrap();
        assert_eq!(paths, [r"bad-\xFF-name"]);
        assert!(escaped);
    }
}
