use crate::{
    cli::{CheckpointArgs, Command, HandoffArgs, StartArgs},
    git,
    model::{Checkpoint, Event, GitSnapshot, SCHEMA_VERSION, Session, SessionState},
    storage::{self, StorageError},
};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Lifecycle(String),
    #[error(
        "unsupported schema version {found} in {kind}; this relay supports version {supported}"
    )]
    UnsupportedSchema {
        kind: &'static str,
        found: u32,
        supported: u32,
    },
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Git(#[from] git::GitError),
    #[error("could not access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("corrupt session state in {path}: {message}; repair or remove the file")]
    CorruptSession { path: PathBuf, message: String },
    #[error(
        "refusing to use symlinked Relay state path {path}; replace it with a real file or directory inside the project"
    )]
    SymlinkState { path: PathBuf },
}

pub fn run(command: Command, project: &Path) -> Result<String, AppError> {
    validate_state_paths(&Paths::new(project))?;
    match command {
        Command::Start(args) => start(project, args),
        Command::Status => status(project),
        Command::Checkpoint(args) => checkpoint(project, args),
        Command::Finish => finish(project),
        Command::Handoff(args) => handoff(project, args),
    }
}

fn start(project: &Path, args: StartArgs) -> Result<String, AppError> {
    let paths = Paths::new(project);
    if paths.session.exists() {
        let existing = read_session(&paths.session)?;
        if existing.state == SessionState::Active {
            return Err(AppError::Lifecycle(
                "an active Relay session already exists; finish it before starting another".into(),
            ));
        }
    }
    read_events(&paths.events)?;

    create_state_directories(&paths)?;
    let now = Utc::now();
    let session = Session {
        schema_version: SCHEMA_VERSION,
        id: Uuid::new_v4(),
        goal: args.goal,
        state: SessionState::Active,
        started_at: now,
        completed_at: None,
        initial_git: git::capture(project)?,
        final_git: None,
        last_agent: None,
    };
    storage::write_json_atomic(&paths.session, &session)?;
    append_event(
        &paths.events,
        &session,
        now,
        "session.started",
        json!({ "goal": session.goal, "git": session.initial_git }),
    )?;

    Ok(format!(
        "Started Relay session\nGoal: {}\nGit: {}",
        session.goal,
        format_git(&session.initial_git)
    ))
}

fn status(project: &Path) -> Result<String, AppError> {
    let paths = Paths::new(project);
    let session = require_session(&paths)?;
    let events = read_events(&paths.events)?;
    let live_git = git::capture(project)?;
    let session_events: Vec<_> = events
        .iter()
        .filter(|event| event.session_id == session.id)
        .collect();
    let latest_checkpoint = session_events
        .iter()
        .rev()
        .find(|event| event.event_type == "checkpoint.created")
        .map(|event| {
            let checkpoint: Checkpoint = serde_json::from_value(
                event.payload.get("checkpoint").cloned().ok_or_else(|| {
                    AppError::Lifecycle(
                        "checkpoint.created event is missing its checkpoint payload".into(),
                    )
                })?,
            )
            .map_err(|error| {
                AppError::Lifecycle(format!(
                    "checkpoint.created event has a malformed checkpoint payload: {error}"
                ))
            })?;
            validate_version("checkpoint", checkpoint.schema_version)?;
            validate_git_version(&checkpoint.git)?;
            Ok::<Checkpoint, AppError>(checkpoint)
        })
        .transpose()?;
    let latest = latest_checkpoint
        .map(|checkpoint| {
            let message = checkpoint.message.as_deref().unwrap_or("(no message)");
            format!("{} — {}", checkpoint.timestamp.to_rfc3339(), message)
        })
        .unwrap_or_else(|| "none".into());

    Ok(format!(
        "Goal: {}\nState: {}\nStarted: {}\nGit branch: {}\nGit commit: {}\nGit clean: {}\nEvents: {}\nLatest checkpoint: {}\nLast agent: {}",
        session.goal,
        state_name(&session.state),
        session.started_at.to_rfc3339(),
        display_option(live_git.branch.as_deref(), live_git.available),
        display_option(live_git.commit.as_deref(), live_git.available),
        display_clean(&live_git),
        session_events.len(),
        latest,
        session.last_agent.as_deref().unwrap_or("unknown"),
    ))
}

fn checkpoint(project: &Path, args: CheckpointArgs) -> Result<String, AppError> {
    let paths = Paths::new(project);
    let session = require_active(&paths)?;
    read_events(&paths.events)?;
    create_state_directories(&paths)?;
    let checkpoint = Checkpoint {
        schema_version: SCHEMA_VERSION,
        id: Uuid::new_v4(),
        session_id: session.id,
        timestamp: Utc::now(),
        message: args.message,
        git: git::capture(project)?,
    };
    let filename = format!(
        "{}-{}.json",
        checkpoint.timestamp.format("%Y%m%dT%H%M%S%.3fZ"),
        checkpoint.id
    );
    let path = paths.checkpoints.join(filename);
    storage::write_json_atomic(&path, &checkpoint)?;
    let relative = relative_to_project(project, &path);
    append_event(
        &paths.events,
        &session,
        checkpoint.timestamp,
        "checkpoint.created",
        json!({ "checkpoint": checkpoint, "path": relative }),
    )?;

    Ok(format!(
        "Checkpoint created: {}\nChanged files: {}",
        relative,
        format_changed_files(&checkpoint.git)
    ))
}

fn handoff(project: &Path, args: HandoffArgs) -> Result<String, AppError> {
    let paths = Paths::new(project);
    let mut session = require_active(&paths)?;
    let events = read_events(&paths.events)?;
    create_state_directories(&paths)?;

    let timestamp = Utc::now();
    let live_git = git::capture(project)?;
    let checkpoints = relevant_checkpoints(&events, session.id)?;
    let markdown = render_handoff(project, &session, &args, &live_git, &checkpoints);
    let filename = format!(
        "{}-{}.md",
        timestamp.format("%Y%m%dT%H%M%S%.3fZ"),
        Uuid::new_v4()
    );
    let path = paths.handoffs.join(filename);
    let relative = relative_to_project(project, &path);

    // Keep the handoff recoverable before recording it, then record history
    // before advancing mutable agent context.
    storage::write_markdown_atomic(&path, &markdown)?;
    append_event(
        &paths.events,
        &session,
        timestamp,
        "handoff.generated",
        json!({
            "agent": args.agent,
            "completed_work": args.completed_work,
            "decisions": args.decision,
            "failures": args.failure,
            "next_action": args.next_action,
            "git": live_git,
            "path": relative,
        }),
    )?;
    session.last_agent = Some(args.agent);
    storage::write_json_atomic(&paths.session, &session)?;

    Ok(format!("{markdown}\nSaved: {relative}"))
}

fn relevant_checkpoints(events: &[Event], session_id: Uuid) -> Result<Vec<Checkpoint>, AppError> {
    let checkpoint_events = events
        .iter()
        .filter(|event| event.session_id == session_id && event.event_type == "checkpoint.created")
        .rev()
        .take(5)
        .collect::<Vec<_>>();
    let mut checkpoints = Vec::with_capacity(checkpoint_events.len());
    for event in checkpoint_events.into_iter().rev() {
        let checkpoint: Checkpoint =
            serde_json::from_value(event.payload.get("checkpoint").cloned().ok_or_else(|| {
                AppError::Lifecycle(
                    "checkpoint.created event is missing its checkpoint payload".into(),
                )
            })?)
            .map_err(|error| {
                AppError::Lifecycle(format!(
                    "checkpoint.created event has a malformed checkpoint payload: {error}"
                ))
            })?;
        validate_version("checkpoint", checkpoint.schema_version)?;
        validate_git_version(&checkpoint.git)?;
        checkpoints.push(checkpoint);
    }
    Ok(checkpoints)
}

fn render_handoff(
    project: &Path,
    session: &Session,
    args: &HandoffArgs,
    git: &GitSnapshot,
    checkpoints: &[Checkpoint],
) -> String {
    let notes = |values: &[String]| {
        if values.is_empty() {
            "Not recorded".into()
        } else {
            values
                .iter()
                .map(|value| format!("- {}", safe_markdown_value(project, value)))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let next_action = args
        .next_action
        .as_ref()
        .map(|value| format!("- {}", safe_markdown_value(project, value)))
        .unwrap_or_else(|| "Not recorded".into());
    let changed_files = if !git.available {
        "Unavailable (not a Git repository)".into()
    } else if git.changed_files.is_empty() {
        "None".into()
    } else {
        git.changed_files
            .iter()
            .map(|path| format!("- {}", safe_markdown_value(project, path)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let checkpoint_context = if checkpoints.is_empty() {
        "Not recorded".into()
    } else {
        checkpoints
            .iter()
            .map(|checkpoint| {
                let message = checkpoint
                    .message
                    .as_deref()
                    .map(|value| safe_markdown_value(project, value))
                    .unwrap_or_else(|| "(no message)".into());
                format!("- {} — {message}", checkpoint.timestamp.to_rfc3339())
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "## Goal\n\n{}\n\n\
         ## Completed work\n\n{}\n\n\
         ## Important decisions\n\n{}\n\n\
         ## Failures or blocked work\n\n{}\n\n\
         ## Current Git state\n\n- Branch: {}\n- Commit: {}\n- Clean: {}\n\n\
         ## Changed files\n\n{}\n\n\
         ## Relevant checkpoints\n\n{}\n\n\
         ## Suggested next action\n\n{}\n\n\
         ## Target agent\n\n{}\n",
        safe_markdown_value(project, &session.goal),
        notes(&args.completed_work),
        notes(&args.decision),
        notes(&args.failure),
        safe_markdown_value(
            project,
            display_option(git.branch.as_deref(), git.available)
        ),
        safe_markdown_value(
            project,
            display_option(git.commit.as_deref(), git.available)
        ),
        display_clean(git),
        changed_files,
        checkpoint_context,
        next_action,
        safe_markdown_value(project, &args.agent),
    )
}

fn safe_markdown_value(project: &Path, value: &str) -> String {
    let concise = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let project_root = project.to_string_lossy();
    if project_root.is_empty() || project_root == "/" {
        concise
    } else {
        concise.replace(project_root.as_ref(), "[project root]")
    }
}

fn finish(project: &Path) -> Result<String, AppError> {
    let paths = Paths::new(project);
    let mut session = require_active(&paths)?;
    read_events(&paths.events)?;
    create_state_directories(&paths)?;
    let completed_at = Utc::now();
    let final_git = git::capture(project)?;
    let summary_path = paths.relay.join(format!("summary-{}.md", session.id));
    let summary_relative = relative_to_project(project, &summary_path);
    let markdown = render_summary(project, &session, completed_at, &final_git);

    // Preserve recoverability: durable artifact and history precede the state transition.
    storage::write_markdown_atomic(&summary_path, &markdown)?;
    append_event(
        &paths.events,
        &session,
        completed_at,
        "session.finished",
        json!({ "git": final_git, "summary_path": summary_relative }),
    )?;
    session.state = SessionState::Completed;
    session.completed_at = Some(completed_at);
    session.final_git = Some(final_git);
    storage::write_json_atomic(&paths.session, &session)?;

    Ok(format!("Session finished\nSummary: {summary_relative}"))
}

fn render_summary(
    project: &Path,
    session: &Session,
    completed_at: DateTime<Utc>,
    git: &GitSnapshot,
) -> String {
    let changed = if git.available && git.changed_files.is_empty() {
        "None".into()
    } else if git.available {
        git.changed_files
            .iter()
            .map(|path| format!("- {}", safe_markdown_value(project, path)))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        "Unavailable (not a Git repository)".into()
    };
    format!(
        "# Relay session summary\n\n\
         ## Goal\n\n{}\n\n\
         ## Session\n\n- Started: {}\n- Finished: {}\n- Last agent: {}\n\n\
         ## Final Git state\n\n- Branch: {}\n- Commit: {}\n- Clean: {}\n\n\
         ## Changed files\n\n{}\n",
        safe_markdown_value(project, &session.goal),
        session.started_at.to_rfc3339(),
        completed_at.to_rfc3339(),
        safe_markdown_value(project, session.last_agent.as_deref().unwrap_or("Unknown")),
        safe_markdown_value(
            project,
            display_option(git.branch.as_deref(), git.available)
        ),
        safe_markdown_value(
            project,
            display_option(git.commit.as_deref(), git.available)
        ),
        display_clean(git),
        changed,
    )
}

fn require_session(paths: &Paths) -> Result<Session, AppError> {
    if !paths.session.exists() {
        return Err(AppError::Lifecycle(
            "no Relay session exists in the current directory; run `relay start <goal>` first"
                .into(),
        ));
    }
    read_session(&paths.session)
}

fn require_active(paths: &Paths) -> Result<Session, AppError> {
    let session = require_session(paths)?;
    if session.state == SessionState::Completed {
        return Err(AppError::Lifecycle(
            "the Relay session is already finished; start a new session to continue".into(),
        ));
    }
    Ok(session)
}

fn read_session(path: &Path) -> Result<Session, AppError> {
    let session: Session = storage::read_json(path)?;
    validate_version("session state", session.schema_version)?;
    validate_git_version(&session.initial_git)?;
    if let Some(git) = &session.final_git {
        validate_git_version(git)?;
    }
    match session.state {
        SessionState::Active if session.completed_at.is_some() || session.final_git.is_some() => {
            return Err(AppError::CorruptSession {
                path: path.to_owned(),
                message: "an active session must not have completed_at or final_git".into(),
            });
        }
        SessionState::Completed
            if session.completed_at.is_none() || session.final_git.is_none() =>
        {
            return Err(AppError::CorruptSession {
                path: path.to_owned(),
                message: "a completed session must have both completed_at and final_git".into(),
            });
        }
        SessionState::Active | SessionState::Completed => {}
    }
    Ok(session)
}

fn read_events(path: &Path) -> Result<Vec<Event>, AppError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let events: Vec<Event> = storage::read_json_lines(path)?;
    for event in &events {
        validate_version("event log", event.schema_version)?;
    }
    Ok(events)
}

fn append_event(
    path: &Path,
    session: &Session,
    timestamp: DateTime<Utc>,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<(), AppError> {
    storage::append_json_line(
        path,
        &Event {
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            session_id: session.id,
            timestamp,
            event_type: event_type.into(),
            payload,
        },
    )?;
    Ok(())
}

fn validate_version(kind: &'static str, found: u32) -> Result<(), AppError> {
    if found != SCHEMA_VERSION {
        return Err(AppError::UnsupportedSchema {
            kind,
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn validate_git_version(git: &GitSnapshot) -> Result<(), AppError> {
    validate_version("Git snapshot", git.schema_version)
}

fn create_state_directories(paths: &Paths) -> Result<(), AppError> {
    for path in [&paths.checkpoints, &paths.handoffs] {
        fs::create_dir_all(path).map_err(|source| AppError::Io {
            path: path.to_owned(),
            source,
        })?;
    }
    Ok(())
}

fn validate_state_paths(paths: &Paths) -> Result<(), AppError> {
    for path in [
        &paths.relay,
        &paths.session,
        &paths.events,
        &paths.checkpoints,
        &paths.handoffs,
    ] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::SymlinkState {
                    path: path.to_owned(),
                });
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(AppError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        }
    }
    Ok(())
}

fn relative_to_project(project: &Path, path: &Path) -> String {
    path.strip_prefix(project)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn state_name(state: &SessionState) -> &'static str {
    match state {
        SessionState::Active => "active",
        SessionState::Completed => "completed",
    }
}

fn display_option(value: Option<&str>, available: bool) -> &str {
    if available {
        value.unwrap_or("unavailable")
    } else {
        "unavailable"
    }
}

fn display_clean(git: &GitSnapshot) -> &'static str {
    match git.clean {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unavailable",
    }
}

fn format_git(git: &GitSnapshot) -> String {
    if !git.available {
        return "unavailable".into();
    }
    format!(
        "branch {}, commit {}, clean {}",
        display_option(git.branch.as_deref(), true),
        display_option(git.commit.as_deref(), true),
        display_clean(git)
    )
}

fn format_changed_files(git: &GitSnapshot) -> String {
    if !git.available {
        "unavailable".into()
    } else if git.changed_files.is_empty() {
        "none".into()
    } else {
        git.changed_files.join(", ")
    }
}

struct Paths {
    relay: PathBuf,
    session: PathBuf,
    events: PathBuf,
    checkpoints: PathBuf,
    handoffs: PathBuf,
}

impl Paths {
    fn new(project: &Path) -> Self {
        let relay = project.join(".relay");
        Self {
            session: relay.join("session.json"),
            events: relay.join("events.jsonl"),
            checkpoints: relay.join("checkpoints"),
            handoffs: relay.join("handoffs"),
            relay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff_args(agent: &str) -> HandoffArgs {
        HandoffArgs {
            agent: agent.into(),
            completed_work: Vec::new(),
            decision: Vec::new(),
            failure: Vec::new(),
            next_action: None,
        }
    }

    #[test]
    fn non_git_lifecycle_persists_artifacts_and_events() {
        let directory = tempfile::tempdir().unwrap();
        start(
            directory.path(),
            StartArgs {
                goal: "ship safely".into(),
            },
        )
        .unwrap();
        assert!(directory.path().join(".relay/checkpoints").is_dir());
        assert!(directory.path().join(".relay/handoffs").is_dir());

        checkpoint(
            directory.path(),
            CheckpointArgs {
                message: Some("first".into()),
            },
        )
        .unwrap();
        let status_output = status(directory.path()).unwrap();
        assert!(status_output.contains("Events: 2"));
        assert!(status_output.contains("first"));
        assert!(status_output.contains("Git branch: unavailable"));

        let finish_output = finish(directory.path()).unwrap();
        let session = read_session(&directory.path().join(".relay/session.json")).unwrap();
        assert_eq!(session.state, SessionState::Completed);
        assert!(finish_output.contains("Summary: .relay/summary-"));
        let events = read_events(&directory.path().join(".relay/events.jsonl")).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["session.started", "checkpoint.created", "session.finished"]
        );
    }

    #[test]
    fn duplicate_start_and_finish_are_actionable() {
        let directory = tempfile::tempdir().unwrap();
        start(directory.path(), StartArgs { goal: "one".into() }).unwrap();
        let duplicate = start(directory.path(), StartArgs { goal: "two".into() })
            .unwrap_err()
            .to_string();
        assert!(duplicate.contains("active Relay session already exists"));

        finish(directory.path()).unwrap();
        let finished = finish(directory.path()).unwrap_err().to_string();
        assert!(finished.contains("already finished"));
    }

    #[test]
    fn unsupported_session_version_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let relay = directory.path().join(".relay");
        fs::create_dir(&relay).unwrap();
        let path = relay.join("session.json");
        let contents = r#"{"schema_version":99,"id":"00000000-0000-0000-0000-000000000000","goal":"old","state":"completed","started_at":"2020-01-01T00:00:00Z","completed_at":null,"initial_git":{"schema_version":1,"available":false,"repository_root":null,"branch":null,"commit":null,"clean":null,"changed_files":[]},"final_git":null,"last_agent":null}"#;
        fs::write(&path, contents).unwrap();

        let error = start(directory.path(), StartArgs { goal: "new".into() })
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported schema version 99"));
        assert_eq!(fs::read_to_string(path).unwrap(), contents);
    }

    #[test]
    fn rejects_inconsistent_session_lifecycle_fields() {
        let directory = tempfile::tempdir().unwrap();
        start(directory.path(), StartArgs { goal: "one".into() }).unwrap();
        let path = directory.path().join(".relay/session.json");
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        for (state, completed_at, final_git, expected) in [
            (
                "active",
                json!("2020-01-01T00:00:00Z"),
                original["initial_git"].clone(),
                "active session must not have",
            ),
            (
                "completed",
                serde_json::Value::Null,
                serde_json::Value::Null,
                "completed session must have both",
            ),
        ] {
            let mut corrupted = original.clone();
            corrupted["state"] = json!(state);
            corrupted["completed_at"] = completed_at;
            corrupted["final_git"] = final_git;
            fs::write(&path, serde_json::to_vec(&corrupted).unwrap()).unwrap();

            let error = read_session(&path).unwrap_err().to_string();
            assert!(error.contains("corrupt session state"));
            assert!(error.contains(expected));
            assert!(error.contains("repair or remove"));
        }
    }

    #[test]
    fn handoff_renders_required_sections_and_not_recorded_values() {
        let directory = tempfile::tempdir().unwrap();
        start(
            directory.path(),
            StartArgs {
                goal: "deliver context".into(),
            },
        )
        .unwrap();
        checkpoint(
            directory.path(),
            CheckpointArgs {
                message: Some("validated behavior".into()),
            },
        )
        .unwrap();

        let output = handoff(directory.path(), handoff_args("omp")).unwrap();
        let headings = output
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect::<Vec<_>>();
        assert_eq!(
            headings,
            [
                "## Goal",
                "## Completed work",
                "## Important decisions",
                "## Failures or blocked work",
                "## Current Git state",
                "## Changed files",
                "## Relevant checkpoints",
                "## Suggested next action",
                "## Target agent",
            ]
        );
        assert_eq!(output.matches("Not recorded").count(), 4);
        assert!(output.contains("validated behavior"));
        assert!(output.contains("\nSaved: .relay/handoffs/"));
    }

    #[test]
    fn handoff_persists_explicit_notes_event_and_last_agent() {
        let directory = tempfile::tempdir().unwrap();
        start(
            directory.path(),
            StartArgs {
                goal: "handoff cleanly".into(),
            },
        )
        .unwrap();
        let args = HandoffArgs {
            agent: "codex".into(),
            completed_work: vec!["implemented flow".into(), "ran tests".into()],
            decision: vec!["keep JSONL".into()],
            failure: vec!["network unavailable".into()],
            next_action: Some("review diff".into()),
        };

        let output = handoff(directory.path(), args).unwrap();
        assert!(output.contains("- implemented flow\n- ran tests"));
        assert!(output.contains("- keep JSONL"));
        assert!(output.contains("- network unavailable"));
        assert!(output.contains("- review diff"));

        let session = read_session(&directory.path().join(".relay/session.json")).unwrap();
        assert_eq!(session.last_agent.as_deref(), Some("codex"));
        let events = read_events(&directory.path().join(".relay/events.jsonl")).unwrap();
        let event = events.last().unwrap();
        assert_eq!(event.event_type, "handoff.generated");
        assert_eq!(event.payload["agent"], "codex");
        assert_eq!(event.payload["completed_work"][0], "implemented flow");
        assert_eq!(event.payload["decisions"][0], "keep JSONL");
        assert_eq!(event.payload["failures"][0], "network unavailable");
        assert_eq!(event.payload["next_action"], "review diff");
        let relative = event.payload["path"].as_str().unwrap();
        assert!(relative.starts_with(".relay/handoffs/"));
        assert!(directory.path().join(relative).is_file());
    }

    #[test]
    fn handoff_excludes_private_root_and_agent_cannot_control_filename() {
        let directory = tempfile::tempdir().unwrap();
        let private_root = directory.path().to_string_lossy().into_owned();
        start(
            directory.path(),
            StartArgs {
                goal: format!("work in {private_root}"),
            },
        )
        .unwrap();
        let mut args = handoff_args("../../escape\n## Injected");
        args.completed_work = vec![format!("inspected {private_root}")];

        let output = handoff(directory.path(), args).unwrap();
        assert!(!output.contains(&private_root));
        assert!(!output.contains("\n## Injected"));
        let events = read_events(&directory.path().join(".relay/events.jsonl")).unwrap();
        let relative = events.last().unwrap().payload["path"].as_str().unwrap();
        assert!(!relative.contains("escape"));
        assert!(relative.starts_with(".relay/handoffs/"));
    }
}
