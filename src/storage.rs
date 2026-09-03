use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("could not access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not encode JSON for {path}: {source}")]
    Encode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("corrupt JSON in {path}: {source}")]
    CorruptJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("corrupt JSON Lines record in {path} at line {line}: {source}")]
    CorruptJsonLine {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "corrupt JSON Lines record in {path} at line {line}: final record is not terminated by a newline; it may be partial"
    )]
    PartialJsonLine { path: PathBuf, line: usize },
}

pub fn write_json_to_open_file<T: Serialize>(
    mut file: &fs::File,
    path: &Path,
    value: &T,
) -> Result<(), StorageError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| StorageError::Encode {
        path: path.to_owned(),
        source,
    })?;
    bytes.push(b'\n');
    file.set_len(0).map_err(|source| io_error(path, source))?;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&bytes))
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|source| io_error(path, source))
}

pub fn remove_file_synced(path: &Path) -> Result<(), StorageError> {
    fs::remove_file(path).map_err(|source| io_error(path, source))?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|source| io_error(path, source))
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StorageError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| StorageError::Encode {
        path: path.to_owned(),
        source,
    })?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

pub fn write_markdown_atomic(path: &Path, markdown: &str) -> Result<(), StorageError> {
    write_atomic(path, markdown.as_bytes())
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;

    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| io_error(parent, source))?;
    temporary
        .write_all(contents)
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| io_error(path, source))?;
    temporary
        .persist(path)
        .map_err(|error| io_error(path, error.error))?;

    sync_directory(parent).map_err(|source| io_error(parent, source))
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, StorageError> {
    let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
    serde_json::from_slice(&bytes).map_err(|source| StorageError::CorruptJson {
        path: path.to_owned(),
        source,
    })
}

pub fn append_json_line<T: Serialize>(path: &Path, value: &T) -> Result<(), StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    let mut line = serde_json::to_vec(value).map_err(|source| StorageError::Encode {
        path: path.to_owned(),
        source,
    })?;
    line.push(b'\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    file.write_all(&line)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(path, source))
}

pub fn read_json_lines<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, StorageError> {
    let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(StorageError::PartialJsonLine {
            path: path.to_owned(),
            line: bytes.iter().filter(|byte| **byte == b'\n').count() + 1,
        });
    }

    bytes
        .strip_suffix(b"\n")
        .unwrap_or(&bytes)
        .split(|byte| *byte == b'\n')
        .enumerate()
        .map(|(index, line)| {
            let line_number = index + 1;
            serde_json::from_slice(line).map_err(|source| StorageError::CorruptJsonLine {
                path: path.to_owned(),
                line: line_number,
                source,
            })
        })
        .collect()
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn io_error(path: &Path, source: io::Error) -> StorageError {
    StorageError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        value: String,
    }

    #[test]
    fn json_and_json_lines_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let json = directory.path().join("record.json");
        let jsonl = directory.path().join("events.jsonl");
        let record = Record {
            value: "hello".into(),
        };

        write_json_atomic(&json, &record).unwrap();
        assert_eq!(read_json::<Record>(&json).unwrap(), record);
        append_json_line(&jsonl, &record).unwrap();
        append_json_line(&jsonl, &record).unwrap();
        assert_eq!(
            read_json_lines::<Record>(&jsonl).unwrap(),
            [
                record,
                Record {
                    value: "hello".into()
                },
            ]
        );
    }

    #[test]
    fn malformed_json_line_reports_its_line() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        fs::write(&path, "{\"value\":\"ok\"}\nnot-json\n").unwrap();

        let error = read_json_lines::<Record>(&path).unwrap_err();
        assert!(matches!(
            error,
            StorageError::CorruptJsonLine { line: 2, .. }
        ));
    }

    #[test]
    fn unterminated_final_json_line_is_rejected_as_partial() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        fs::write(&path, "{\"value\":\"ok\"}\n{\"value\":\"partial\"}").unwrap();

        let error = read_json_lines::<Record>(&path).unwrap_err();
        assert!(matches!(
            error,
            StorageError::PartialJsonLine { line: 2, .. }
        ));
        assert!(error.to_string().contains("not terminated by a newline"));
    }

    #[test]
    fn malformed_json_is_reported_without_changing_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        let contents = b"{not valid json}\n";
        fs::write(&path, contents).unwrap();

        let error = read_json::<Record>(&path).unwrap_err();
        assert!(matches!(error, StorageError::CorruptJson { .. }));
        assert_eq!(fs::read(path).unwrap(), contents);
    }
}
