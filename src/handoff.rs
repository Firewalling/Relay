use crate::{
    cli::HandoffArgs,
    model::{
        Checkpoint, Event, GitSnapshot, HANDOFF_SCHEMA_VERSION, HandoffCheckpointReference,
        HandoffEntry, HandoffEntrySource, HandoffNoteReference, HandoffOutputReference,
        HandoffOutputType, HandoffRecord, HandoffReferences, HandoffRunReference,
        HandoffSessionSnapshot, HandoffTaskSnapshot, HandoffUnavailable, HandoffWorkflowSnapshot,
        SCHEMA_VERSION, Session, SessionState, WORKTREE_SCHEMA_VERSION, WorktreeProvenance,
    },
    orchestrate::{OutputRecord, OutputType, RunState, TaskStatus},
    storage,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

pub const MAX_HANDOFF_COUNT: usize = 128;
pub const MAX_HANDOFF_RECORD_BYTES: usize = 256 * 1024;
pub const MAX_HANDOFF_LEDGER_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_HANDOFF_GOAL_BYTES: usize = 4 * 1024;
pub const MAX_HANDOFF_ENTRY_BYTES: usize = 4 * 1024;
pub const MAX_HANDOFF_ENTRIES: usize = 32;
pub const MAX_HANDOFF_AGENT_BYTES: usize = 128;
pub const MAX_HANDOFF_RUN_REFERENCES: usize = 8;
pub const MAX_HANDOFF_OUTPUT_REFERENCES: usize = 32;
pub const MAX_HANDOFF_CHECKPOINT_REFERENCES: usize = 16;
pub const MAX_HANDOFF_NOTE_REFERENCES: usize = 32;
pub const MAX_HANDOFF_TASKS_PER_RUN: usize = 64;
pub const MAX_HANDOFF_LEGACY_BYTES: u64 = 256 * 1024;
pub const MAX_HANDOFF_CHANGED_FILES: usize = 256;
pub const MAX_HANDOFF_PATH_BYTES: usize = 512;
pub const MAX_HANDOFF_GIT_PATH_BYTES: usize = 4 * 1024;
pub const MAX_HANDOFF_STATUS_BYTES: usize = 256;

const HANDOFF_DIRECTORY: &str = ".relay/handoffs";
const RUN_DIRECTORY: &str = ".relay/runs/";
const CHECKPOINT_DIRECTORY: &str = ".relay/checkpoints/";
const OUTPUT_DIRECTORY: &str = ".relay/runs/outputs/";
const MAX_RENDER_BYTES: usize = 256 * 1024;
#[cfg(unix)]
fn safe_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && metadata.nlink() == 1
}

#[cfg(not(unix))]
fn safe_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}
fn reject_symlinked_components(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        current.push(component.as_os_str());
        if components.peek().is_none() {
            break;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "handoff path contains symlinked component: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "could not inspect handoff path component {}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

fn open_handoff_file(path: &Path) -> Result<fs::File, String> {
    reject_symlinked_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect handoff file {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "handoff file must not be symlinked: {}",
            path.display()
        ));
    }
    if !safe_regular_file(&metadata) {
        return Err(format!(
            "handoff file is not a singly-linked regular file: {}",
            path.display()
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path).map_err(|error| {
        format!(
            "could not securely open handoff file {}: {error}",
            path.display()
        )
    })?;
    let opened = file
        .metadata()
        .map_err(|error| format!("could not inspect handoff file {}: {error}", path.display()))?;
    if !safe_regular_file(&opened) {
        return Err(format!(
            "handoff file is not a singly-linked regular file: {}",
            path.display()
        ));
    }
    Ok(file)
}

fn read_handoff_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    let file = open_handoff_file(path)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect handoff file {}: {error}", path.display()))?;
    if metadata.len() > maximum {
        return Err(format!(
            "handoff file exceeds {maximum} bytes: {}",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read handoff file {}: {error}", path.display()))?;
    if bytes.len() as u64 > maximum {
        return Err(format!(
            "handoff file exceeds {maximum} bytes: {}",
            path.display()
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffAction {
    Create { target: String },
    Show(String),
    Latest,
    List,
}

pub fn action(args: &HandoffArgs) -> Result<HandoffAction, String> {
    if let Some(selector) = args.show.as_deref() {
        if args.agent.is_some() || has_create_options(args) {
            return Err("handoff --show accepts one id and no create options".into());
        }
        return Ok(HandoffAction::Show(selector.to_owned()));
    }
    if args.latest {
        if args.agent.is_some() || has_create_options(args) {
            return Err("handoff --latest accepts no selector or create options".into());
        }
        return Ok(HandoffAction::Latest);
    }
    if args.list {
        if args.agent.is_some() || has_create_options(args) {
            return Err("handoff --list accepts no selector or create options".into());
        }
        return Ok(HandoffAction::List);
    }
    match args.agent.as_deref() {
        None => {
            let target = args
                .to
                .clone()
                .ok_or_else(|| "handoff create requires --to <agent>".to_owned())?;
            Ok(HandoffAction::Create { target })
        }
        Some("create") if args.to.is_some() => Ok(HandoffAction::Create {
            target: args.to.clone().unwrap(),
        }),
        Some(subject) if args.to.is_none() => Ok(HandoffAction::Create {
            target: subject.to_owned(),
        }),
        Some(_) => Err("target options require handoff create".into()),
    }
}

fn has_create_options(args: &HandoffArgs) -> bool {
    args.to.is_some()
        || args.source_agent.is_some()
        || args.target_worktree.is_some()
        || args.goal.is_some()
        || !args.completed_work.is_empty()
        || !args.decision.is_empty()
        || !args.failure.is_empty()
        || args.next_action.is_some()
        || !args.next_actions.is_empty()
        || !args.runs.is_empty()
        || !args.checkpoints.is_empty()
        || !args.notes.is_empty()
        || args.supersedes.is_some()
}

pub fn is_mutating(args: &HandoffArgs) -> bool {
    matches!(action(args), Ok(HandoffAction::Create { .. }))
}

#[derive(Debug, Clone)]
pub enum StoredHandoff {
    Structured {
        record: Box<HandoffRecord>,
        path: PathBuf,
    },
    Legacy {
        id: String,
        path: PathBuf,
        markdown: String,
        created_at: Option<DateTime<Utc>>,
    },
}
#[derive(Debug, Clone)]
struct CheckpointCandidate {
    checkpoint: Checkpoint,
    reference: HandoffCheckpointReference,
}

impl StoredHandoff {
    pub fn id(&self) -> String {
        match self {
            Self::Structured { record, .. } => record.id.to_string(),
            Self::Legacy { id, .. } => id.clone(),
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Structured { path, .. } | Self::Legacy { path, .. } => path,
        }
    }

    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Structured { record, .. } => Some(record.created_at),
            Self::Legacy { created_at, .. } => *created_at,
        }
    }

    pub fn record(&self) -> Option<&HandoffRecord> {
        match self {
            Self::Structured { record, .. } => Some(record),
            Self::Legacy { .. } => None,
        }
    }

    pub fn render(&self, project: &Path) -> String {
        match self {
            Self::Structured { record, .. } => render(record, project),
            Self::Legacy { markdown, .. } => truncate_render(&safe_markdown(project, markdown)),
        }
    }

    pub fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy { .. })
    }
}

pub fn read_records(path: &Path) -> Result<Vec<HandoffRecord>, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "could not inspect handoff ledger {}: {error}",
                path.display()
            ));
        }
    }
    let bytes = read_handoff_bytes(path, MAX_HANDOFF_LEDGER_BYTES)?;
    decode_records(&bytes)
}

fn decode_records(bytes: &[u8]) -> Result<Vec<HandoffRecord>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.ends_with(b"\n") {
        return Err("handoff ledger has an unterminated final record".into());
    }
    let mut records = Vec::new();
    let mut ids = HashSet::new();
    for (index, line) in bytes
        .strip_suffix(b"\n")
        .unwrap_or(bytes)
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if index >= MAX_HANDOFF_COUNT {
            return Err(format!(
                "handoff ledger contains more than {MAX_HANDOFF_COUNT} records"
            ));
        }
        if line.len() > MAX_HANDOFF_RECORD_BYTES {
            return Err(format!(
                "handoff record exceeds {MAX_HANDOFF_RECORD_BYTES} bytes"
            ));
        }
        let value: Value = serde_json::from_slice(line).map_err(|error| {
            format!(
                "malformed structured handoff at line {}: {error}",
                index + 1
            )
        })?;
        let schema_version = value
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| "handoff record is missing integer schema_version".to_owned())?;
        if schema_version != u64::from(HANDOFF_SCHEMA_VERSION) {
            return Err(format!(
                "unsupported handoff schema_version {schema_version}; this Relay supports {HANDOFF_SCHEMA_VERSION}"
            ));
        }
        let record: HandoffRecord = serde_json::from_value(value).map_err(|error| {
            format!(
                "malformed structured handoff at line {}: {error}",
                index + 1
            )
        })?;
        validate_record(&record)?;
        if !ids.insert(record.id) {
            return Err(format!("duplicate handoff id {}", record.id));
        }
        records.push(record);
    }
    records.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(records)
}

pub fn append_record(path: &Path, record: &HandoffRecord) -> Result<(), String> {
    validate_record(record)?;
    reject_symlinked_components(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create handoff ledger directory: {error}"))?;
    let file_existed = match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "could not inspect handoff ledger {}: {error}",
                path.display()
            ));
        }
    };
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).append(true);
    #[cfg(unix)]
    options
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .mode(0o600);
    let mut file = options.open(path).map_err(|error| {
        format!(
            "could not securely open handoff ledger {}: {error}",
            path.display()
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        format!(
            "could not inspect handoff ledger {}: {error}",
            path.display()
        )
    })?;
    if !safe_regular_file(&metadata) {
        return Err(format!(
            "handoff ledger is not a singly-linked regular file: {}",
            path.display()
        ));
    }
    let mut existing = Vec::new();
    file.seek(SeekFrom::Start(0))
        .and_then(|_| {
            (&mut file)
                .take(MAX_HANDOFF_LEDGER_BYTES.saturating_add(1))
                .read_to_end(&mut existing)
        })
        .map_err(|error| format!("could not read handoff ledger {}: {error}", path.display()))?;
    if existing.len() as u64 > MAX_HANDOFF_LEDGER_BYTES {
        return Err(format!(
            "handoff ledger exceeds {MAX_HANDOFF_LEDGER_BYTES} bytes"
        ));
    }
    let existing_records = decode_records(&existing)?;
    if existing_records.len() >= MAX_HANDOFF_COUNT {
        return Err(format!(
            "handoff store reached maximum of {MAX_HANDOFF_COUNT} records"
        ));
    }
    if existing_records
        .iter()
        .any(|existing| existing.id == record.id)
    {
        return Err(format!("duplicate handoff id {}", record.id));
    }
    let mut line = serde_json::to_vec(record)
        .map_err(|error| format!("handoff cannot be encoded: {error}"))?;
    line.push(b'\n');
    if (existing.len() as u64).saturating_add(line.len() as u64) > MAX_HANDOFF_LEDGER_BYTES {
        return Err(format!(
            "handoff ledger exceeds {MAX_HANDOFF_LEDGER_BYTES} bytes"
        ));
    }
    file.write_all(&line)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| {
            format!(
                "could not append handoff ledger {}: {error}",
                path.display()
            )
        })?;
    if !file_existed {
        storage::sync_directory(parent).map_err(|error| {
            format!(
                "could not sync handoff ledger directory {}: {error}",
                parent.display()
            )
        })?;
    }
    Ok(())
}

pub fn read_stored(
    project: &Path,
    ledger_path: &Path,
    directory: &Path,
) -> Result<Vec<StoredHandoff>, String> {
    // Structured records are historical snapshots. Live run, output, and
    // checkpoint objects are validated only while creating a new record.
    let records = read_records(ledger_path)?;
    let mut entries = Vec::with_capacity(records.len());
    let mut generated_paths = HashSet::with_capacity(records.len());
    for record in records {
        let path = project.join(&record.artifact_path);
        reject_symlinked_components(&path)?;
        generated_paths.insert(path.clone());
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "handoff artifact must not be symlinked: {}",
                        path.display()
                    ));
                }
                if !safe_regular_file(&metadata) {
                    return Err(format!(
                        "handoff artifact is not a regular file: {}",
                        path.display()
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect handoff artifact {}: {error}",
                    path.display()
                ));
            }
        }
        entries.push(StoredHandoff::Structured {
            record: Box::new(record),
            path,
        });
    }
    reject_symlinked_components(directory)?;

    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            sort_entries(&mut entries);
            return Ok(entries);
        }
        Err(error) => {
            return Err(format!(
                "could not inspect handoff directory {}: {error}",
                directory.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "handoff directory must not be symlinked: {}",
            directory.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "handoff path is not a directory: {}",
            directory.display()
        ));
    }
    let mut files = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "could not read handoff directory {}: {error}",
                directory.display()
            )
        })?
        .map(|entry| entry.map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            format!(
                "could not read handoff directory {}: {error}",
                directory.display()
            )
        })?;
    files.sort_by_key(|entry| entry.file_name());
    let mut legacy_bytes = 0u64;
    for entry in files {
        let path = entry.path();
        let metadata = entry.file_type().map_err(|error| {
            format!("could not inspect handoff file {}: {error}", path.display())
        })?;
        if metadata.is_symlink() {
            return Err(format!(
                "handoff file must not be symlinked: {}",
                path.display()
            ));
        }
        if !metadata.is_file() || path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        if generated_paths.contains(&path) {
            continue;
        }
        if entries.len() >= MAX_HANDOFF_COUNT {
            return Err(format!(
                "handoff store contains more than {MAX_HANDOFF_COUNT} records"
            ));
        }
        let bytes = read_handoff_bytes(&path, MAX_HANDOFF_LEGACY_BYTES)?;
        legacy_bytes = legacy_bytes.saturating_add(bytes.len() as u64);
        if legacy_bytes > MAX_HANDOFF_LEDGER_BYTES {
            return Err(format!(
                "legacy handoff store exceeds {MAX_HANDOFF_LEDGER_BYTES} bytes"
            ));
        }

        let markdown = String::from_utf8(bytes)
            .map_err(|_| format!("legacy handoff is not UTF-8: {}", path.display()))?;
        let id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| format!("legacy handoff filename is not UTF-8: {}", path.display()))?
            .to_owned();
        entries.push(StoredHandoff::Legacy {
            id,
            created_at: legacy_timestamp(&path),
            path,
            markdown,
        });
    }
    sort_entries(&mut entries);
    if entries.len() > MAX_HANDOFF_COUNT {
        return Err(format!(
            "handoff store contains more than {MAX_HANDOFF_COUNT} records"
        ));
    }
    Ok(entries)
}

pub fn select<'a>(
    entries: &'a [StoredHandoff],
    selector: &str,
) -> Result<&'a StoredHandoff, String> {
    if selector.eq_ignore_ascii_case("latest") {
        return entries
            .first()
            .ok_or_else(|| "no Relay handoff records exist".to_owned());
    }
    let display_selector = safe_inline(Path::new(""), selector);
    let selector = selector.to_ascii_lowercase();
    let matches = entries
        .iter()
        .filter(|entry| entry.id().to_ascii_lowercase().starts_with(&selector))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [entry] => Ok(entry),
        [] => Err(format!("no Relay handoff matches `{display_selector}`")),
        _ => Err(format!(
            "handoff selector `{display_selector}` is ambiguous; use more UUID characters"
        )),
    }
}

pub fn build_record(
    project: &Path,
    args: &HandoffArgs,
    session: &Session,
    events: &[Event],
    mut git: GitSnapshot,
    target_worktree: Option<WorktreeProvenance>,
    run_states: &[(PathBuf, RunState)],
) -> Result<HandoffRecord, String> {
    let HandoffAction::Create { target } = action(args)? else {
        return Err("handoff inspection action cannot create a record".into());
    };
    if args.to.is_some() {
        validate_agent(&target, "target agent")?;
    } else {
        validate_display_agent(&target, "target agent")?;
    }
    if let Some(source_agent) = args.source_agent.as_deref() {
        validate_agent(source_agent, "source agent")?;
    }
    let (session_goal, _) =
        bound_historical_nonblank(&session.goal, MAX_HANDOFF_GOAL_BYTES, "session goal")?;
    let goal = if let Some(goal) = args.goal.clone() {
        validate_nonblank_bounded(&goal, MAX_HANDOFF_GOAL_BYTES, "handoff goal")?;
        Some(goal)
    } else {
        Some(session_goal.clone())
    };
    let session_last_agent = session
        .last_agent
        .as_deref()
        .map(|value| {
            bound_historical_nonblank(value, MAX_HANDOFF_AGENT_BYTES, "session agent")
                .map(|(value, _)| value)
        })
        .transpose()?;
    let supersedes = args
        .supersedes
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|error| format!("supersedes must be a UUID: {error}"))?;

    let note_candidates = note_references(events, session.id)?;
    let selected_notes = select_notes(&note_candidates, &args.notes)?;
    let checkpoint_candidates = checkpoint_references(events, session.id)?;
    let selected_checkpoint_candidates =
        select_checkpoints(&checkpoint_candidates, &args.checkpoints)?;
    for candidate in &selected_checkpoint_candidates {
        validate_checkpoint_for_handoff_creation(project, candidate)?;
    }
    let selected_checkpoints = selected_checkpoint_candidates
        .into_iter()
        .map(|candidate| candidate.reference)
        .collect();

    let explicit_completed = explicit_entries(&args.completed_work, None)?;
    let explicit_decisions = explicit_entries(&args.decision, None)?;
    let explicit_blockers = explicit_entries(&args.failure, None)?;
    let mut explicit_next_values = Vec::new();
    if let Some(next_action) = args.next_action.as_deref() {
        explicit_next_values.push(next_action.to_owned());
    }
    explicit_next_values.extend(args.next_actions.iter().cloned());
    let explicit_next = explicit_entries(&explicit_next_values, None)?;
    let mut completed = entries_for_category(
        &selected_notes,
        "progress",
        MAX_HANDOFF_ENTRIES.saturating_sub(explicit_completed.len()),
    );
    completed.extend(explicit_completed);
    let mut decisions = entries_for_category(
        &selected_notes,
        "decision",
        MAX_HANDOFF_ENTRIES.saturating_sub(explicit_decisions.len()),
    );
    decisions.extend(explicit_decisions);
    let mut blockers = entries_for_category(
        &selected_notes,
        "blocker",
        MAX_HANDOFF_ENTRIES.saturating_sub(explicit_blockers.len()),
    );
    blockers.extend(explicit_blockers);
    let mut next_actions = entries_for_category(
        &selected_notes,
        "next_action",
        MAX_HANDOFF_ENTRIES.saturating_sub(explicit_next.len()),
    );
    next_actions.extend(explicit_next);

    git = bound_git(git);
    let mut references = HandoffReferences {
        checkpoints: selected_checkpoints,
        notes: selected_notes,
        runs: Vec::new(),
        outputs: Vec::new(),
    };
    let mut run_ids = HashSet::with_capacity(run_states.len());
    for (state_path, state) in run_states {
        if !run_ids.insert(state.run_id.clone()) {
            return Err(format!(
                "handoff references duplicate aggregate run {}",
                state.run_id
            ));
        }
        if references.runs.len() >= MAX_HANDOFF_RUN_REFERENCES {
            return Err(format!(
                "handoff references more than {MAX_HANDOFF_RUN_REFERENCES} runs"
            ));
        }
        let (run, outputs) = run_reference(project, state_path, state)?;
        references.runs.push(run);
        references.outputs.extend(outputs);
        if references.outputs.len() > MAX_HANDOFF_OUTPUT_REFERENCES {
            return Err(format!(
                "handoff references more than {MAX_HANDOFF_OUTPUT_REFERENCES} trusted outputs"
            ));
        }
    }

    let id = Uuid::new_v4();
    let created_at = Utc::now();
    let artifact_path = format!(
        "{HANDOFF_DIRECTORY}/{}-{}.md",
        created_at.format("%Y%m%dT%H%M%S%.3fZ"),
        id
    );
    let source_session = Some(HandoffSessionSnapshot {
        id: session.id,
        goal: session_goal,
        state: session.state.clone(),
        started_at: session.started_at,
        completed_at: session.completed_at,
        last_agent: session_last_agent,
        worktree: session.initial_git.worktree.clone(),
        resumed_from: session.resumed_from,
    });
    let mut unavailable = Vec::new();
    if args.source_agent.is_none() {
        unavailable.push(HandoffUnavailable::SourceAgent);
    }
    if goal.is_none() {
        unavailable.push(HandoffUnavailable::Goal);
    }
    push_unavailable(
        &mut unavailable,
        completed.is_empty(),
        HandoffUnavailable::Completed,
    );
    push_unavailable(
        &mut unavailable,
        decisions.is_empty(),
        HandoffUnavailable::Decisions,
    );
    push_unavailable(
        &mut unavailable,
        blockers.is_empty(),
        HandoffUnavailable::Blockers,
    );
    push_unavailable(
        &mut unavailable,
        next_actions.is_empty(),
        HandoffUnavailable::NextActions,
    );
    push_unavailable(&mut unavailable, !git.available, HandoffUnavailable::Git);
    push_unavailable(
        &mut unavailable,
        references.checkpoints.is_empty(),
        HandoffUnavailable::Checkpoints,
    );
    push_unavailable(
        &mut unavailable,
        references.notes.is_empty(),
        HandoffUnavailable::Notes,
    );
    push_unavailable(
        &mut unavailable,
        references.runs.is_empty(),
        HandoffUnavailable::Runs,
    );
    push_unavailable(
        &mut unavailable,
        references.outputs.is_empty(),
        HandoffUnavailable::Outputs,
    );

    let record = HandoffRecord {
        schema_version: HANDOFF_SCHEMA_VERSION,
        id,
        artifact_path,
        created_at,
        source_session,
        source_agent: args.source_agent.clone(),
        target_agent: target,
        goal,
        completed,
        decisions,
        blockers,
        next_actions,
        references,
        source_worktree: session.initial_git.worktree.clone(),
        target_worktree,
        git,
        unavailable,
        supersedes,
    };
    validate_record(&record)?;
    let encoded = serde_json::to_vec(&record)
        .map_err(|error| format!("handoff cannot be encoded: {error}"))?;
    if encoded.len() > MAX_HANDOFF_RECORD_BYTES {
        return Err(format!(
            "handoff record exceeds {MAX_HANDOFF_RECORD_BYTES} bytes"
        ));
    }
    Ok(record)
}

fn note_references(
    events: &[Event],
    session_id: Uuid,
) -> Result<Vec<HandoffNoteReference>, String> {
    let mut notes = Vec::new();
    for event in events
        .iter()
        .filter(|event| event.session_id == session_id && event.event_type == "note.recorded")
    {
        let Some(category) = event.payload.get("category").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(
            category,
            "progress" | "decision" | "blocker" | "observation" | "next_action"
        ) {
            continue;
        }
        let Some(text) = event.payload.get("text").and_then(Value::as_str) else {
            continue;
        };
        let (text, truncated) = bound_historical_text(text, MAX_HANDOFF_ENTRY_BYTES);
        notes.push(HandoffNoteReference {
            id: event.id,
            session_id,
            timestamp: event.timestamp,
            category: category.to_owned(),
            text,
            truncated,
        });
    }
    Ok(notes)
}

fn checkpoint_references(
    events: &[Event],
    session_id: Uuid,
) -> Result<Vec<CheckpointCandidate>, String> {
    let mut checkpoints = Vec::new();
    for event in events
        .iter()
        .filter(|event| event.session_id == session_id && event.event_type == "checkpoint.created")
    {
        let checkpoint: Checkpoint =
            serde_json::from_value(event.payload.get("checkpoint").cloned().ok_or_else(|| {
                format!("checkpoint event {} is missing checkpoint data", event.id)
            })?)
            .map_err(|error| format!("checkpoint event {} is malformed: {error}", event.id))?;
        if checkpoint.schema_version != SCHEMA_VERSION || checkpoint.session_id != session_id {
            return Err(format!(
                "checkpoint event {} has invalid provenance",
                event.id
            ));
        }
        if checkpoint.git.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "checkpoint {} has unsupported Git schema",
                checkpoint.id
            ));
        }
        let path = event
            .payload
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("checkpoint event {} is missing its path", event.id))?;
        validate_relative_reference(path, CHECKPOINT_DIRECTORY)?;
        let (message, truncated) = checkpoint
            .message
            .as_deref()
            .map(|value| bound_historical_text(value, MAX_HANDOFF_ENTRY_BYTES))
            .map_or((None, false), |(value, truncated)| (Some(value), truncated));
        checkpoints.push(CheckpointCandidate {
            checkpoint: checkpoint.clone(),
            reference: HandoffCheckpointReference {
                id: checkpoint.id,
                session_id,
                timestamp: checkpoint.timestamp,
                message,
                path: path.to_owned(),
                git: bound_git(checkpoint.git),
                truncated,
            },
        });
    }
    Ok(checkpoints)
}

fn select_notes(
    candidates: &[HandoffNoteReference],
    selectors: &[String],
) -> Result<Vec<HandoffNoteReference>, String> {
    select_references(candidates, selectors, MAX_HANDOFF_NOTE_REFERENCES, |item| {
        item.id
    })
}

fn select_checkpoints(
    candidates: &[CheckpointCandidate],
    selectors: &[String],
) -> Result<Vec<CheckpointCandidate>, String> {
    select_references(
        candidates,
        selectors,
        MAX_HANDOFF_CHECKPOINT_REFERENCES,
        |item| item.reference.id,
    )
}

fn validate_checkpoint_for_handoff_creation(
    project: &Path,
    candidate: &CheckpointCandidate,
) -> Result<(), String> {
    let path = project.join(&candidate.reference.path);
    let bytes = read_handoff_bytes(&path, MAX_HANDOFF_LEDGER_BYTES)?;
    let mut expected = serde_json::to_vec_pretty(&candidate.checkpoint)
        .map_err(|error| format!("checkpoint cannot be encoded: {error}"))?;
    expected.push(b'\n');
    if bytes != expected {
        return Err(format!(
            "checkpoint {} is no longer trusted",
            candidate.reference.id
        ));
    }
    Ok(())
}

fn select_references<T: Clone>(
    candidates: &[T],
    selectors: &[String],
    limit: usize,
    id: impl Fn(&T) -> Uuid,
) -> Result<Vec<T>, String> {
    if selectors.len() > limit {
        return Err(format!("handoff selectors exceed {limit} entries"));
    }
    if selectors.is_empty() {
        let start = candidates.len().saturating_sub(limit);
        return Ok(candidates[start..].to_vec());
    }
    let mut selected = HashSet::new();
    for selector in selectors {
        let display_selector = safe_inline(Path::new(""), selector);
        let matches = if selector.eq_ignore_ascii_case("latest") {
            candidates.last().into_iter().collect::<Vec<_>>()
        } else {
            let selector = selector.to_ascii_lowercase();
            candidates
                .iter()
                .filter(|candidate| id(candidate).to_string().starts_with(&selector))
                .collect::<Vec<_>>()
        };
        match matches.as_slice() {
            [candidate] => {
                selected.insert(id(candidate));
            }
            [] => return Err(format!("no handoff reference matches `{display_selector}`")),
            _ => {
                return Err(format!(
                    "handoff reference selector `{display_selector}` is ambiguous"
                ));
            }
        }
    }
    if selected.len() > limit {
        return Err(format!("handoff references exceed {limit} entries"));
    }
    Ok(candidates
        .iter()
        .filter(|candidate| selected.contains(&id(candidate)))
        .cloned()
        .collect())
}

fn entries_for_category(
    notes: &[HandoffNoteReference],
    category: &str,
    limit: usize,
) -> Vec<HandoffEntry> {
    let mut entries = notes
        .iter()
        .filter(|note| note.category == category)
        .map(|note| HandoffEntry {
            text: note.text.clone(),
            source: HandoffEntrySource::RecordedNote,
            recorded_at: Some(note.timestamp),
            truncated: note.truncated,
        })
        .collect::<Vec<_>>();
    let omitted = entries.len().saturating_sub(limit);
    entries.drain(..omitted);
    entries
}

fn explicit_entries(
    values: &[String],
    recorded_at: Option<DateTime<Utc>>,
) -> Result<Vec<HandoffEntry>, String> {
    if values.len() > MAX_HANDOFF_ENTRIES {
        return Err(format!(
            "handoff category exceeds {MAX_HANDOFF_ENTRIES} entries"
        ));
    }
    values
        .iter()
        .map(|value| {
            validate_nonblank_bounded(value, MAX_HANDOFF_ENTRY_BYTES, "handoff entry")?;
            let (text, truncated) = bound_text(value, MAX_HANDOFF_ENTRY_BYTES);
            Ok(HandoffEntry {
                text,
                source: HandoffEntrySource::Explicit,
                recorded_at,
                truncated,
            })
        })
        .collect()
}

fn run_reference(
    project: &Path,
    state_path: &Path,
    state: &RunState,
) -> Result<(HandoffRunReference, Vec<HandoffOutputReference>), String> {
    let state_path = relative_project_path(project, state_path)?;
    let workflow = state
        .workflow
        .as_ref()
        .map(|workflow| HandoffWorkflowSnapshot {
            name: workflow.name.clone(),
            schema_version: workflow.schema_version,
            definition_sha256: workflow.definition_sha256.clone(),
            inputs: workflow.inputs.clone(),
        });
    let tasks = state
        .tasks
        .iter()
        .map(|task| {
            let latest = task.attempts.last();
            HandoffTaskSnapshot {
                id: task.id.clone(),
                status: task.status.as_str().to_owned(),
                reason: task
                    .reason
                    .as_deref()
                    .map(|reason| bound_text(reason, MAX_HANDOFF_ENTRY_BYTES).0),
                attempt: latest.map(|attempt| attempt.attempt),
                attempt_run_id: latest.map(|attempt| attempt.run_id.clone()),
                outputs: task
                    .outputs
                    .as_ref()
                    .map(|outputs| outputs.iter().map(|output| output.name.clone()).collect())
                    .unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    if tasks.len() > MAX_HANDOFF_TASKS_PER_RUN {
        return Err(format!(
            "run contains more than {MAX_HANDOFF_TASKS_PER_RUN} tasks"
        ));
    }
    let mut outputs = Vec::new();
    for task in &state.tasks {
        if task.status != TaskStatus::Succeeded {
            continue;
        }
        let Some(task_outputs) = task.outputs.as_ref() else {
            continue;
        };
        for output in task_outputs {
            outputs.push(output_reference(project, state, task.id.as_str(), output)?);
        }
    }
    Ok((
        HandoffRunReference {
            run_id: state.run_id.clone(),
            state_path,
            status: state.status.as_str().to_owned(),
            worktree: state.worktree.clone(),
            workflow,
            tasks,
        },
        outputs,
    ))
}

fn output_reference(
    project: &Path,
    state: &RunState,
    task: &str,
    output: &OutputRecord,
) -> Result<HandoffOutputReference, String> {
    let path = relative_project_path(project, Path::new(&output.path))?;
    let output_type = match output.output_type {
        OutputType::Json => HandoffOutputType::Json,
        OutputType::File => HandoffOutputType::File,
    };
    let sha256 = output
        .sha256
        .clone()
        .ok_or_else(|| format!("trusted output {task}/{} has no hash", output.name))?;
    Ok(HandoffOutputReference {
        run_id: state.run_id.clone(),
        task: task.to_owned(),
        output: output.name.clone(),
        attempt: output.attempt,
        attempt_run_id: output.attempt_run_id.clone(),
        output_type,
        path,
        size_bytes: output.size_bytes,
        sha256,
        value: output.value.clone(),
    })
}

fn bound_git(mut git: GitSnapshot) -> GitSnapshot {
    for value in [
        &mut git.repository_root,
        &mut git.branch,
        &mut git.commit,
        &mut git.git_path_encoding,
    ]
    .into_iter()
    .flatten()
    {
        let (bounded, _) = bound_text(value, MAX_HANDOFF_PATH_BYTES);
        *value = bounded;
    }
    if let Some(worktree) = git.worktree.as_mut() {
        for value in [
            &mut worktree.repository_id,
            &mut worktree.repository_root,
            &mut worktree.worktree_id,
            &mut worktree.worktree_path,
        ] {
            let (bounded, _) = bound_text(value, MAX_HANDOFF_PATH_BYTES);
            *value = bounded;
        }
        for value in [
            &mut worktree.branch,
            &mut worktree.ref_name,
            &mut worktree.head,
        ]
        .into_iter()
        .flatten()
        {
            let (bounded, _) = bound_text(value, MAX_HANDOFF_PATH_BYTES);
            *value = bounded;
        }
    }
    if git.changed_files.len() > MAX_HANDOFF_CHANGED_FILES {
        let omitted = git.changed_files.len() - MAX_HANDOFF_CHANGED_FILES;
        git.changed_files.truncate(MAX_HANDOFF_CHANGED_FILES);
        git.changed_files_truncated = true;
        git.changed_files_omitted = omitted;
    } else {
        git.changed_files_truncated = false;
        git.changed_files_omitted = 0;
    }
    git
}

fn push_unavailable(
    list: &mut Vec<HandoffUnavailable>,
    condition: bool,
    value: HandoffUnavailable,
) {
    if condition && !list.contains(&value) {
        list.push(value);
    }
}

pub fn validate_record(record: &HandoffRecord) -> Result<(), String> {
    if record.schema_version != HANDOFF_SCHEMA_VERSION {
        return Err(format!(
            "unsupported handoff schema_version {}",
            record.schema_version
        ));
    }
    validate_relative_reference(&record.artifact_path, &(HANDOFF_DIRECTORY.to_owned() + "/"))?;
    validate_display_agent(&record.target_agent, "target agent")?;
    if let Some(source_agent) = record.source_agent.as_deref() {
        validate_agent(source_agent, "source agent")?;
    }
    if let Some(goal) = record.goal.as_deref() {
        validate_nonblank_bounded(goal, MAX_HANDOFF_GOAL_BYTES, "handoff goal")?;
    }
    if let Some(session) = &record.source_session {
        validate_nonblank_bounded(&session.goal, MAX_HANDOFF_GOAL_BYTES, "session goal")?;
        if let Some(last_agent) = session.last_agent.as_deref() {
            validate_display_agent(last_agent, "session agent")?;
        }
        if session.state == SessionState::Active && session.completed_at.is_some()
            || session.state == SessionState::Completed && session.completed_at.is_none()
        {
            return Err("handoff session snapshot has inconsistent lifecycle fields".into());
        }
        if let Some(worktree) = &session.worktree {
            validate_worktree(worktree)?;
        }
    }
    if let Some(worktree) = &record.source_worktree {
        validate_worktree(worktree)?;
    }
    if let Some(worktree) = &record.target_worktree {
        validate_worktree(worktree)?;
    }
    validate_entries(&record.completed, "completed")?;
    validate_entries(&record.decisions, "decisions")?;
    validate_entries(&record.blockers, "blockers")?;
    validate_entries(&record.next_actions, "next_actions")?;
    if record.references.checkpoints.len() > MAX_HANDOFF_CHECKPOINT_REFERENCES {
        return Err(format!(
            "handoff contains more than {MAX_HANDOFF_CHECKPOINT_REFERENCES} checkpoints"
        ));
    }
    if record.references.notes.len() > MAX_HANDOFF_NOTE_REFERENCES {
        return Err(format!(
            "handoff contains more than {MAX_HANDOFF_NOTE_REFERENCES} notes"
        ));
    }
    if record.references.runs.len() > MAX_HANDOFF_RUN_REFERENCES {
        return Err(format!(
            "handoff contains more than {MAX_HANDOFF_RUN_REFERENCES} runs"
        ));
    }
    if record.references.outputs.len() > MAX_HANDOFF_OUTPUT_REFERENCES {
        return Err(format!(
            "handoff contains more than {MAX_HANDOFF_OUTPUT_REFERENCES} outputs"
        ));
    }
    validate_git(&record.git)?;
    let mut checkpoint_ids = HashSet::new();
    for checkpoint in &record.references.checkpoints {
        if !checkpoint_ids.insert(checkpoint.id) {
            return Err(format!("duplicate checkpoint id {}", checkpoint.id));
        }
        if checkpoint.session_id
            != record
                .source_session
                .as_ref()
                .map(|session| session.id)
                .unwrap_or(checkpoint.session_id)
        {
            return Err(format!(
                "checkpoint {} crosses handoff session",
                checkpoint.id
            ));
        }
        validate_relative_reference(&checkpoint.path, CHECKPOINT_DIRECTORY)?;
        if Path::new(&checkpoint.path).extension() != Some(std::ffi::OsStr::new("json")) {
            return Err(format!("checkpoint {} has an invalid path", checkpoint.id));
        }
        if let Some(message) = checkpoint.message.as_deref() {
            validate_text(message, MAX_HANDOFF_ENTRY_BYTES, "checkpoint message")?;
        }
        validate_git(&checkpoint.git)?;
    }
    let mut note_ids = HashSet::new();
    for note in &record.references.notes {
        if !note_ids.insert(note.id) {
            return Err(format!("duplicate note id {}", note.id));
        }
        validate_text(&note.category, 32, "note category")?;
        validate_nonblank_bounded(&note.text, MAX_HANDOFF_ENTRY_BYTES, "note text")?;
        if !matches!(
            note.category.as_str(),
            "progress" | "decision" | "blocker" | "observation" | "next_action"
        ) {
            return Err(format!("note {} has an unsupported category", note.id));
        }
        if record
            .source_session
            .as_ref()
            .is_some_and(|session| session.id != note.session_id)
        {
            return Err(format!("note {} crosses handoff session", note.id));
        }
    }
    let mut run_ids = HashSet::new();
    for run in &record.references.runs {
        if !run_ids.insert(run.run_id.clone()) || !valid_uuid(&run.run_id) {
            return Err(format!(
                "invalid or duplicate aggregate run id {:?}",
                run.run_id
            ));
        }
        validate_relative_reference(&run.state_path, RUN_DIRECTORY)?;
        if Path::new(&run.state_path).extension() != Some(std::ffi::OsStr::new("json")) {
            return Err(format!("run {} has an invalid state path", run.run_id));
        }
        if let Some(worktree) = &run.worktree {
            validate_worktree(worktree)?;
        }
        validate_text(&run.status, MAX_HANDOFF_STATUS_BYTES, "run status")?;
        if !matches!(
            run.status.as_str(),
            "running" | "completed" | "failed" | "cancelled" | "indeterminate"
        ) {
            return Err(format!("unsupported handoff run status {:?}", run.status));
        }
        if let Some(workflow) = &run.workflow {
            if !matches!(workflow.schema_version, 1 | 2)
                || !valid_workflow_name(&workflow.name)
                || !valid_sha256(&workflow.definition_sha256)
            {
                return Err(format!(
                    "invalid workflow provenance for run {}",
                    run.run_id
                ));
            }
            if workflow.inputs.len() > 16 {
                return Err(format!("workflow {} exceeds input bound", workflow.name));
            }
            for (name, value) in &workflow.inputs {
                if !valid_workflow_input_name(name) {
                    return Err(format!("workflow input {:?} has an invalid name", name));
                }
                if serde_json::to_vec(value)
                    .map_err(|error| error.to_string())?
                    .len()
                    > 4 * 1024
                {
                    return Err(format!("workflow input {:?} exceeds bound", name));
                }
                match value {
                    Value::String(value) if value.len() <= 4 * 1024 => {}
                    Value::Bool(_) => {}
                    Value::Number(value) if value.as_i64().is_some() => {}
                    _ => return Err(format!("workflow input {:?} is not a scalar", name)),
                }
            }
        }
        if run.tasks.len() > MAX_HANDOFF_TASKS_PER_RUN {
            return Err(format!(
                "run {} contains too many task snapshots",
                run.run_id
            ));
        }
        for task in &run.tasks {
            if !valid_task_id(&task.id) {
                return Err(format!("task {} has an invalid task id", task.id));
            }
            if task.outputs.len() > 4 {
                return Err(format!("task {} has too many output names", task.id));
            }
            validate_text(&task.status, MAX_HANDOFF_STATUS_BYTES, "task status")?;
            if let Some(attempt) = task.attempt
                && attempt == 0
            {
                return Err(format!("task {} has an invalid attempt number", task.id));
            }
            for output in &task.outputs {
                if !valid_output_name(output) {
                    return Err(format!("task {} has an invalid output name", task.id));
                }
            }
            if let Some(reason) = task.reason.as_deref() {
                validate_text(reason, MAX_HANDOFF_ENTRY_BYTES, "task reason")?;
            }
            if let Some(attempt_run_id) = task.attempt_run_id.as_deref()
                && !valid_uuid(attempt_run_id)
            {
                return Err(format!("task {} has invalid attempt identity", task.id));
            }
            if !matches!(
                task.status.as_str(),
                "pending"
                    | "running"
                    | "succeeded"
                    | "failed"
                    | "cancelled"
                    | "blocked"
                    | "indeterminate"
            ) {
                return Err(format!("unsupported task status {:?}", task.status));
            }
        }
    }
    let mut output_keys = HashSet::new();
    for output in &record.references.outputs {
        let key = format!(
            "{}:{}:{}:{}",
            output.run_id, output.task, output.output, output.attempt_run_id
        );
        if !output_keys.insert(key) || !run_ids.contains(&output.run_id) {
            return Err(format!(
                "output {} has invalid run provenance",
                output.output
            ));
        }
        if !valid_uuid(&output.run_id) || !valid_uuid(&output.attempt_run_id) || output.attempt == 0
        {
            return Err(format!(
                "output {} has invalid attempt provenance",
                output.output
            ));
        }
        if !valid_task_id(&output.task) || !valid_output_name(&output.output) {
            return Err(format!(
                "output {} has an invalid producer identity",
                output.output
            ));
        }
        validate_relative_reference(&output.path, OUTPUT_DIRECTORY)?;
        let mut expected_prefix = format!("{OUTPUT_DIRECTORY}{}", output.attempt_run_id);
        expected_prefix.push('/');
        if !output.path.starts_with(&expected_prefix) {
            return Err(format!(
                "output {} has an invalid attempt path",
                output.output
            ));
        }
        let trusted_task = record
            .references
            .runs
            .iter()
            .find(|run| run.run_id == output.run_id)
            .and_then(|run| run.tasks.iter().find(|task| task.id == output.task))
            .is_some_and(|task| {
                task.status == "succeeded"
                    && task.attempt == Some(output.attempt)
                    && task.attempt_run_id.as_deref() == Some(output.attempt_run_id.as_str())
                    && task.outputs.iter().any(|name| name == &output.output)
            });
        if !trusted_task {
            return Err(format!(
                "output {} has no matching successful task snapshot",
                output.output
            ));
        }
        if !valid_sha256(&output.sha256) {
            return Err(format!("output {} has invalid SHA-256", output.output));
        }
        let encoded = serde_json::to_vec(&output.value).map_err(|error| error.to_string())?;
        match output.output_type {
            HandoffOutputType::Json if output.size_bytes > 2 * 1024 || encoded.len() > 2 * 1024 => {
                return Err(format!("structured output {} exceeds bound", output.output));
            }
            HandoffOutputType::File
                if output.size_bytes > 64 * 1024 * 1024 || output.value != Value::Null =>
            {
                return Err(format!(
                    "file artifact {} has invalid metadata",
                    output.output
                ));
            }
            _ => {}
        }
    }
    for unavailable in &record.unavailable {
        if record
            .unavailable
            .iter()
            .filter(|candidate| *candidate == unavailable)
            .count()
            > 1
        {
            return Err("handoff unavailable list contains duplicates".into());
        }
    }
    let bytes = serde_json::to_vec(record)
        .map_err(|error| format!("handoff cannot be encoded: {error}"))?;
    if bytes.len() > MAX_HANDOFF_RECORD_BYTES {
        return Err(format!(
            "handoff record exceeds {MAX_HANDOFF_RECORD_BYTES} bytes"
        ));
    }
    Ok(())
}

fn validate_entries(entries: &[HandoffEntry], name: &str) -> Result<(), String> {
    if entries.len() > MAX_HANDOFF_ENTRIES {
        return Err(format!(
            "handoff {name} exceeds {MAX_HANDOFF_ENTRIES} entries"
        ));
    }

    for entry in entries {
        validate_nonblank_bounded(&entry.text, MAX_HANDOFF_ENTRY_BYTES, "handoff entry")?;
    }
    Ok(())
}

fn validate_worktree(worktree: &WorktreeProvenance) -> Result<(), String> {
    if worktree.schema_version != WORKTREE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported worktree provenance schema {}",
            worktree.schema_version
        ));
    }
    for (name, value) in [
        ("repository identity", &worktree.repository_id),
        ("repository root", &worktree.repository_root),
        ("worktree identity", &worktree.worktree_id),
        ("worktree path", &worktree.worktree_path),
    ] {
        validate_text(value, MAX_HANDOFF_PATH_BYTES, name)?;
        if !Path::new(value).is_absolute() {
            return Err(format!("{name} must be absolute"));
        }
    }
    for (name, value) in [
        ("worktree branch", &worktree.branch),
        ("worktree ref", &worktree.ref_name),
        ("worktree HEAD", &worktree.head),
    ] {
        if let Some(value) = value {
            validate_text(value, MAX_HANDOFF_PATH_BYTES, name)?;
        }
    }
    if worktree.detached && worktree.branch.is_some() {
        return Err("detached worktree must not have a branch".into());
    }
    Ok(())
}

fn validate_git(git: &GitSnapshot) -> Result<(), String> {
    if git.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported Git snapshot schema {}",
            git.schema_version
        ));
    }
    if let Some(worktree) = &git.worktree {
        validate_worktree(worktree)?;
    }
    for (value, name) in [
        (&git.repository_root, "Git repository root"),
        (&git.branch, "Git branch"),
        (&git.commit, "Git commit"),
        (&git.git_path_encoding, "Git path encoding"),
    ] {
        if let Some(value) = value {
            validate_text(value, MAX_HANDOFF_PATH_BYTES, name)?;
        }
    }
    if git.changed_files.len() > MAX_HANDOFF_CHANGED_FILES {
        return Err("handoff Git snapshot exceeds changed-file bound".into());
    }
    if git.changed_files_truncated != (git.changed_files_omitted > 0) {
        return Err("handoff Git truncation metadata is inconsistent".into());
    }
    for path in &git.changed_files {
        validate_git_changed_path(path)?;
    }
    Ok(())
}

fn validate_git_changed_path(value: &str) -> Result<(), String> {
    validate_text(value, MAX_HANDOFF_GIT_PATH_BYTES, "changed file")?;
    if value.is_empty() || value.starts_with('/') {
        return Err("changed file must be project-relative".into());
    }
    for component in value.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err("changed file must use normal relative components".into());
        }
    }
    Ok(())
}

fn validate_agent(value: &str, name: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_HANDOFF_AGENT_BYTES
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'@' | b':')
        })
    {
        return Err(format!(
            "{name} must match [A-Za-z0-9][A-Za-z0-9._+@:-]{{0,127}}"
        ));
    }
    Ok(())
}

fn validate_display_agent(value: &str, name: &str) -> Result<(), String> {
    validate_nonblank_bounded(value, MAX_HANDOFF_AGENT_BYTES, name)
}

fn validate_nonblank_bounded(value: &str, maximum: usize, name: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} must not be blank"));
    }
    validate_text(value, maximum, name)
}

fn validate_text(value: &str, maximum: usize, name: &str) -> Result<(), String> {
    if value.len() > maximum || value.contains('\0') {
        return Err(format!("{name} exceeds {maximum} bytes or contains NUL"));
    }
    Ok(())
}

fn validate_relative_reference(value: &str, prefix: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.len() > MAX_HANDOFF_PATH_BYTES {
        return Err(format!(
            "handoff reference path exceeds {MAX_HANDOFF_PATH_BYTES} bytes"
        ));
    }
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || !value.starts_with(prefix)
    {
        return Err(format!(
            "handoff reference path is outside {prefix}: {value:?}"
        ));
    }
    Ok(())
}

fn valid_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn valid_task_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_workflow_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
fn valid_workflow_input_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn valid_output_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn bound_text(value: &str, maximum: usize) -> (String, bool) {
    if value.len() <= maximum {
        return (value.to_owned(), false);
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

fn bound_historical_text(value: &str, maximum: usize) -> (String, bool) {
    if value.len() <= maximum {
        return (value.to_owned(), false);
    }
    let (bounded, _) = bound_text(value.trim(), maximum);
    (bounded, true)
}

fn bound_historical_nonblank(
    value: &str,
    maximum: usize,
    name: &str,
) -> Result<(String, bool), String> {
    let (bounded, truncated) = bound_historical_text(value, maximum);
    validate_nonblank_bounded(&bounded, maximum, name)?;
    Ok((bounded, truncated))
}

fn relative_project_path(project: &Path, path: &Path) -> Result<String, String> {
    let project = fs::canonicalize(project)
        .map_err(|error| format!("could not resolve project root: {error}"))?;
    let path = fs::canonicalize(path)
        .map_err(|error| format!("could not resolve handoff reference: {error}"))?;
    let relative = path
        .strip_prefix(&project)
        .map_err(|_| format!("handoff reference escapes project: {}", path.display()))?;
    let relative = relative
        .to_str()
        .ok_or_else(|| "handoff reference is not UTF-8".to_owned())?;
    if relative.is_empty()
        || Path::new(relative)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("handoff reference is not relative: {relative:?}"));
    }
    #[cfg(windows)]
    let relative = relative.replace('\\', "/");
    #[cfg(not(windows))]
    let relative = relative.to_owned();
    Ok(relative)
}

fn legacy_timestamp(path: &Path) -> Option<DateTime<Utc>> {
    let stem = path.file_stem()?.to_str()?;
    let timestamp = stem.split_once('-')?.0;
    NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%S%.3fZ")
        .ok()
        .map(|value| DateTime::<Utc>::from_naive_utc_and_offset(value, Utc))
}

fn sort_entries(entries: &mut [StoredHandoff]) {
    entries.sort_by(|left, right| {
        right
            .created_at()
            .cmp(&left.created_at())
            .then_with(|| right.id().cmp(&left.id()))
            .then_with(|| right.path().cmp(left.path()))
    });
}

pub fn render(record: &HandoffRecord, project: &Path) -> String {
    let mut markdown = String::new();
    section(
        &mut markdown,
        "Goal",
        record.goal.as_deref().unwrap_or("Not recorded"),
        project,
    );
    section_entries(&mut markdown, "Completed work", &record.completed, project);
    section_entries(
        &mut markdown,
        "Important decisions",
        &record.decisions,
        project,
    );
    section_entries(
        &mut markdown,
        "Failures or blocked work",
        &record.blockers,
        project,
    );
    markdown.push_str("## Current Git state\n\n");
    if record.git.available {
        markdown.push_str(&format!(
            "- Branch: {}\n- Commit: {}\n- Clean: {}\n",
            safe_inline(
                project,
                record.git.branch.as_deref().unwrap_or("unavailable")
            ),
            safe_inline(
                project,
                record.git.commit.as_deref().unwrap_or("unavailable")
            ),
            match record.git.clean {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unavailable",
            }
        ));
    } else {
        markdown.push_str("- Unavailable (not a Git repository)\n");
    }
    if let Some(worktree) = &record.source_worktree {
        markdown.push_str(&format!(
            "- Repository: {}\n- Worktree: {}\n- Ref: {}\n- HEAD: {}\n",
            safe_inline(project, &worktree.repository_id),
            safe_inline(project, &worktree.worktree_path),
            safe_inline(project, worktree.ref_name.as_deref().unwrap_or("detached")),
            safe_inline(project, worktree.head.as_deref().unwrap_or("unborn")),
        ));
    }
    if let Some(worktree) = &record.target_worktree {
        markdown.push_str(&format!(
            "- Target worktree: {} ({})\n",
            safe_inline(project, &worktree.worktree_path),
            safe_inline(project, worktree.branch.as_deref().unwrap_or("detached")),
        ));
    }
    markdown.push_str("\n## Changed files\n\n");
    if !record.git.available || record.git.changed_files.is_empty() {
        markdown.push_str(if record.git.available {
            "None\n"
        } else {
            "Unavailable\n"
        });
    } else {
        for path in &record.git.changed_files {
            markdown.push_str("- ");
            markdown.push_str(&safe_inline(project, path));
            markdown.push('\n');
        }
        if record.git.changed_files_truncated {
            markdown.push_str(&format!(
                "- [truncated: {} more]\n",
                record.git.changed_files_omitted
            ));
        }
    }
    markdown.push_str("\n## Relevant checkpoints\n\n");
    if record.references.checkpoints.is_empty() {
        markdown.push_str("Not recorded\n");
    } else {
        for checkpoint in &record.references.checkpoints {
            markdown.push_str("- ");
            markdown.push_str(&checkpoint.timestamp.to_rfc3339());
            markdown.push_str(" — ");
            markdown.push_str(&safe_inline(
                project,
                checkpoint.message.as_deref().unwrap_or("(no message)"),
            ));
            markdown.push_str(" (");
            markdown.push_str(&safe_inline(project, &checkpoint.path));
            markdown.push_str(")\n");
        }
    }
    markdown.push_str("\n## Suggested next action\n\n");
    if record.next_actions.is_empty() {
        markdown.push_str("Not recorded\n");
    } else {
        for entry in &record.next_actions {
            markdown.push_str("- ");
            markdown.push_str(&safe_entry(project, entry));
            markdown.push('\n');
        }
    }
    if let Some(session) = &record.source_session {
        markdown.push_str(&format!(
            "\nSource session: {} — {} — {}\n",
            session.id,
            match session.state {
                SessionState::Active => "active",
                SessionState::Completed => "completed",
            },
            session.started_at.to_rfc3339()
        ));
    }
    if let Some(source_agent) = &record.source_agent {
        markdown.push_str("Source agent: ");
        markdown.push_str(&safe_inline(project, source_agent));
        markdown.push('\n');
    }
    if !record.references.notes.is_empty()
        || !record.references.runs.is_empty()
        || !record.references.outputs.is_empty()
        || record.supersedes.is_some()
    {
        markdown.push_str("\nReferences:\n");
        for note in &record.references.notes {
            markdown.push_str(&format!(
                "- note {} ({})\n",
                safe_inline(project, &note.id.to_string()),
                safe_inline(project, &note.category)
            ));
        }
        for run in &record.references.runs {
            markdown.push_str("- run ");
            markdown.push_str(&safe_inline(project, &run.run_id));
            markdown.push_str(" (");
            markdown.push_str(&safe_inline(project, &run.status));
            markdown.push(')');
            if let Some(workflow) = &run.workflow {
                markdown.push_str(" — workflow ");
                markdown.push_str(&safe_inline(project, &workflow.name));
            }
            markdown.push_str(" [");
            markdown.push_str(&safe_inline(project, &run.state_path));
            markdown.push_str("]\n");
        }
        for output in &record.references.outputs {
            markdown.push_str("- output ");
            markdown.push_str(&safe_inline(
                project,
                &format!("{}/{}", output.task, output.output),
            ));
            markdown.push_str(" → ");
            match output.output_type {
                HandoffOutputType::Json => markdown.push_str("structured JSON"),
                HandoffOutputType::File => markdown.push_str(&safe_inline(project, &output.path)),
            }
            markdown.push_str(&format!(" (attempt {})\n", output.attempt));
        }
        if let Some(supersedes) = record.supersedes {
            markdown.push_str(&format!("- supersedes {}\n", supersedes));
        }
    }
    markdown.push_str("\n## Target agent\n\n- ");
    markdown.push_str(&safe_inline(project, &record.target_agent));
    markdown.push('\n');
    truncate_render(&markdown)
}

fn section(markdown: &mut String, title: &str, value: &str, project: &Path) {
    markdown.push_str("## ");
    markdown.push_str(title);
    markdown.push_str("\n\n");
    markdown.push_str(&safe_inline(project, value));
    markdown.push('\n');
}

fn section_entries(markdown: &mut String, title: &str, entries: &[HandoffEntry], project: &Path) {
    markdown.push_str("## ");
    markdown.push_str(title);
    markdown.push_str("\n\n");
    if entries.is_empty() {
        markdown.push_str("Not recorded\n");
        return;
    }
    for entry in entries {
        markdown.push_str("- ");
        markdown.push_str(&safe_entry(project, entry));
        markdown.push('\n');
    }
}

fn safe_entry(project: &Path, entry: &HandoffEntry) -> String {
    let mut value = safe_inline(project, &entry.text);
    if entry.truncated {
        value.push_str(" [truncated]");
    }
    value
}

fn safe_inline(project: &Path, value: &str) -> String {
    let project_root = project.to_string_lossy();
    let value = if project_root.is_empty() || project_root == "/" {
        value.to_owned()
    } else {
        value.replace(project_root.as_ref(), "[project root]")
    };
    let mut safe = String::with_capacity(value.len());
    for segment in value.split_whitespace() {
        if !safe.is_empty() {
            safe.push(' ');
        }
        for character in segment.chars() {
            if character.is_control() {
                safe.extend(character.escape_default());
            } else {
                safe.push(character);
            }
        }
    }
    safe
}

fn safe_markdown(project: &Path, value: &str) -> String {
    let project_root = project.to_string_lossy();
    let value = if project_root.is_empty() || project_root == "/" {
        value.to_owned()
    } else {
        value.replace(project_root.as_ref(), "[project root]")
    };
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => safe.push(character),
            character if character.is_control() => safe.extend(character.escape_default()),
            character => safe.push(character),
        }
    }
    safe
}

pub fn render_context_bounded(
    entry: &StoredHandoff,
    project: &Path,
    max_chars: usize,
) -> (String, bool) {
    let body = entry.render(project);
    let header = format!("# Relay context{}{}", char::from(10), char::from(10));
    let rendered = format!("{header}{body}");
    if rendered.chars().count() <= max_chars {
        return (rendered, false);
    }
    let marker = format!(
        "{}{}Truncated: yes{}",
        char::from(10),
        char::from(10),
        char::from(10)
    );
    if max_chars <= marker.chars().count() {
        let (prefix, _) = bound_text("# Relay context", max_chars);
        return (prefix, true);
    }
    let separator = format!("{}## ", char::from(10));
    let mut bounded = header;
    let mut omitted = false;
    for (index, section) in body.split(&separator).enumerate() {
        let section = if index == 0 {
            section.to_owned()
        } else {
            format!("{}## {section}", char::from(10))
        };
        if bounded.chars().count() + section.chars().count() + marker.chars().count() <= max_chars {
            bounded.push_str(&section);
        } else {
            omitted = true;
        }
    }
    if omitted {
        bounded.push_str(&marker);
    }
    (bounded, omitted)
}

fn truncate_render(value: &str) -> String {
    if value.len() <= MAX_RENDER_BYTES {
        return value.to_owned();
    }
    let marker = "\n\nTruncated: yes\n";
    let maximum = MAX_RENDER_BYTES.saturating_sub(marker.len());
    let (prefix, _) = bound_text(value, maximum);
    format!("{prefix}{marker}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serialization_is_deterministic_and_rejects_unknown_fields() {
        let record = HandoffRecord {
            schema_version: HANDOFF_SCHEMA_VERSION,
            id: Uuid::nil(),
            artifact_path: format!(".relay/handoffs/record-{}.md", Uuid::nil()),
            created_at: DateTime::UNIX_EPOCH,
            source_session: None,
            source_worktree: None,
            target_worktree: None,
            source_agent: None,
            target_agent: "agent-b".into(),
            goal: Some("goal".into()),
            completed: Vec::new(),
            decisions: Vec::new(),
            blockers: Vec::new(),
            next_actions: Vec::new(),
            references: HandoffReferences::default(),
            git: GitSnapshot::unavailable(),
            unavailable: vec![HandoffUnavailable::SourceSession],
            supersedes: None,
        };
        let first = serde_json::to_vec(&record).unwrap();
        let second = serde_json::to_vec(&record).unwrap();
        assert_eq!(first, second);
        assert!(serde_json::from_slice::<HandoffRecord>(
            br#"{"schema_version":2,"id":"00000000-0000-0000-0000-000000000000","artifact_path":".relay/handoffs/record-00000000-0000-0000-0000-000000000000.md","created_at":"1970-01-01T00:00:00Z","target_agent":"agent-b","goal":"goal","completed":[],"decisions":[],"blockers":[],"next_actions":[],"references":{"checkpoints":[],"notes":[],"runs":[],"outputs":[]},"git":{"schema_version":1,"available":false,"changed_files":[],"changed_files_truncated":false,"changed_files_omitted":0},"unavailable":[],"unknown":true}"#
        )
        .is_err());
    }

    #[test]
    fn bounded_text_preserves_utf8_and_marks_truncation() {
        let (text, truncated) = bound_text("ééé", 3);
        assert_eq!(text, "é");
        assert!(truncated);
    }
    fn minimal_record() -> HandoffRecord {
        HandoffRecord {
            schema_version: HANDOFF_SCHEMA_VERSION,
            id: Uuid::nil(),
            artifact_path: format!(".relay/handoffs/record-{}.md", Uuid::nil()),
            created_at: DateTime::UNIX_EPOCH,
            source_session: None,
            source_worktree: None,
            target_worktree: None,
            source_agent: None,
            target_agent: "agent-b".into(),
            goal: Some("goal".into()),
            completed: Vec::new(),
            decisions: Vec::new(),
            blockers: Vec::new(),
            next_actions: Vec::new(),
            references: HandoffReferences::default(),
            git: GitSnapshot::unavailable(),
            unavailable: vec![HandoffUnavailable::SourceSession],
            supersedes: None,
        }
    }

    #[test]
    fn validation_rejects_oversized_fields_entries_and_paths() {
        let mut record = minimal_record();
        record.goal = Some("x".repeat(MAX_HANDOFF_GOAL_BYTES + 1));
        assert!(validate_record(&record).is_err());

        let mut record = minimal_record();
        record.completed = (0..=MAX_HANDOFF_ENTRIES)
            .map(|_| HandoffEntry {
                text: "done".into(),
                source: HandoffEntrySource::Explicit,
                recorded_at: None,
                truncated: false,
            })
            .collect();
        assert!(validate_record(&record).is_err());

        let mut record = minimal_record();
        record.target_agent = "agent / reviewer".into();
        assert!(validate_record(&record).is_ok());
        record.target_agent = "agent\0".into();
        assert!(validate_record(&record).is_err());
        record.target_agent = "agent-b".into();
        record.artifact_path = "../outside.md".into();
        assert!(validate_record(&record).is_err());
    }

    #[test]
    fn forged_git_changed_paths_are_rejected_but_escaped_names_survive() {
        let mut record = minimal_record();
        record.git = GitSnapshot {
            schema_version: SCHEMA_VERSION,
            available: true,
            changed_files: vec!["../outside.txt".into()],
            ..GitSnapshot::unavailable()
        };
        assert!(validate_record(&record).is_err());

        record.git.changed_files = vec![r"invalid-\xFF.txt".into()];
        assert!(validate_record(&record).is_ok());

        record.git.changed_files = vec!["/tmp/outside.txt".into()];
        assert!(validate_record(&record).is_err());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("handoffs.jsonl");
        let mut bytes = serde_json::to_vec(&record).unwrap();
        bytes.push(10);
        fs::write(&path, &bytes).unwrap();
        assert!(
            read_records(&path)
                .unwrap_err()
                .contains("changed file must be project-relative")
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn long_git_paths_keep_historical_snapshots_compatible() {
        let path = format!("{}/{}", "a".repeat(300), "b".repeat(300));
        let mut record = minimal_record();
        record.git = GitSnapshot {
            schema_version: SCHEMA_VERSION,
            available: true,
            changed_files: vec![path.clone()],
            ..GitSnapshot::unavailable()
        };
        let original = record.git.clone();
        record.git = bound_git(record.git.clone());

        assert_eq!(record.git.changed_files, vec![path]);
        assert_eq!(original.changed_files, record.git.changed_files);
        assert!(validate_record(&record).is_ok());
        record.git.changed_files = vec!["a".repeat(MAX_HANDOFF_GIT_PATH_BYTES)];
        assert!(validate_record(&record).is_ok());
        record.git.changed_files = vec!["a".repeat(MAX_HANDOFF_GIT_PATH_BYTES + 1)];
        assert!(validate_record(&record).is_err());
    }
    #[test]
    fn rendering_escapes_terminal_controls_and_preserves_structure() {
        let mut record = minimal_record();
        record.goal = Some("before\u{1b}[31m after".into());
        record.completed.push(HandoffEntry {
            text: "completed".into(),
            source: HandoffEntrySource::Explicit,
            recorded_at: None,
            truncated: false,
        });
        let rendered = render(&record, Path::new("/tmp/project"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("## Goal"));
        assert!(rendered.contains("completed"));
    }
    #[test]
    fn duplicate_reference_ids_are_rejected() {
        let mut record = minimal_record();
        let checkpoint = HandoffCheckpointReference {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            timestamp: DateTime::UNIX_EPOCH,
            message: None,
            truncated: false,
            path: ".relay/checkpoints/checkpoint.json".into(),
            git: GitSnapshot::unavailable(),
        };
        record.references.checkpoints = vec![checkpoint.clone(), checkpoint];
        let error = validate_record(&record).unwrap_err();
        assert!(error.contains("duplicate checkpoint id"));

        let note = HandoffNoteReference {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            timestamp: DateTime::UNIX_EPOCH,
            category: "decision".into(),
            text: "note".into(),
            truncated: false,
        };
        record.references.checkpoints.clear();
        record.references.notes = vec![note.clone(), note];
        let error = validate_record(&record).unwrap_err();
        assert!(error.contains("duplicate note id"));
    }

    #[test]
    fn malformed_ledger_is_rejected_without_rewriting_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("handoffs.jsonl");
        let bytes = b"{\"schema_version\":2}\n";
        fs::write(&path, bytes).unwrap();
        assert!(read_records(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn duplicate_ids_are_rejected_without_mutating_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("handoffs.jsonl");
        let encoded = serde_json::to_vec(&minimal_record()).unwrap();
        let mut bytes = encoded.clone();
        bytes.push(10);
        bytes.extend_from_slice(&encoded);
        bytes.push(10);
        fs::write(&path, &bytes).unwrap();
        let error = read_records(&path).unwrap_err();
        assert!(error.contains("duplicate handoff id"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn append_rejects_duplicate_id_without_mutating_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("handoffs.jsonl");
        let record = minimal_record();
        append_record(&path, &record).unwrap();
        let before = fs::read(&path).unwrap();

        let error = append_record(&path, &record).unwrap_err();
        assert!(error.contains("duplicate handoff id"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    #[test]
    fn append_rejects_the_129th_structured_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("handoffs.jsonl");
        let mut bytes = Vec::new();
        for index in 1..=MAX_HANDOFF_COUNT {
            let mut record = minimal_record();
            record.id = Uuid::from_u128(index as u128);
            record.artifact_path = format!(".relay/handoffs/record-{}.md", record.id);
            bytes.extend_from_slice(&serde_json::to_vec(&record).unwrap());
            bytes.push(10);
        }
        fs::write(&path, &bytes).unwrap();
        assert_eq!(read_records(&path).unwrap().len(), MAX_HANDOFF_COUNT);
        let error = append_record(&path, &minimal_record()).unwrap_err();
        assert!(error.contains("reached maximum of 128 records"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    #[cfg(unix)]
    #[test]
    fn append_rejects_symlinked_ledger_without_touching_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("outside.jsonl");
        let ledger = directory.path().join("handoffs.jsonl");
        let sentinel = b"outside\n";
        fs::write(&target, sentinel).unwrap();
        std::os::unix::fs::symlink(&target, &ledger).unwrap();

        let error = append_record(&ledger, &minimal_record()).unwrap_err();
        assert!(error.contains("securely open handoff ledger"));
        assert_eq!(fs::read(&target).unwrap(), sentinel);
    }
    #[cfg(unix)]
    #[test]
    fn handoff_files_reject_symlinked_ancestor_components() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("checkpoint.json");
        fs::write(&outside_file, b"outside").unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        std::os::unix::fs::symlink(outside.path(), nested.join("link")).unwrap();

        let path = nested.join("link/checkpoint.json");
        let error = read_handoff_bytes(&path, MAX_HANDOFF_LEDGER_BYTES).unwrap_err();
        assert!(error.contains("symlinked component"));
    }
    #[cfg(unix)]
    #[test]
    fn new_ledger_is_owner_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private").join("handoffs.jsonl");
        append_record(&path, &minimal_record()).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
