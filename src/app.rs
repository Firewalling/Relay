use crate::{
    cli::{
        CheckpointArgs, Command, ContextArgs, DoctorArgs, HandoffArgs, HistoryArgs, NoteArgs,
        NoteCategory, SessionSelectorArgs, StartArgs,
    },
    git,
    lock::{self, ProjectLock},
    model::{
        Checkpoint, Diagnostic, DiagnosticSeverity, Event, GitSnapshot, OperationEffect,
        OperationMarker, SCHEMA_VERSION, Session, SessionState,
    },
    storage::{self, StorageError},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
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
    #[error(transparent)]
    Lock(#[from] lock::LockError),
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
    #[error("injected interruption after {0}")]
    InjectedFault(String),
}

pub struct RunOutput {
    pub human: String,
    pub data: Value,
}

pub fn run(command: Command, project: &Path) -> Result<RunOutput, AppError> {
    validate_state_paths(&Paths::new(project))?;
    if is_mutating(&command)
        && !matches!(command, Command::Start(_))
        && !project.join(".relay").exists()
    {
        return Err(AppError::Lifecycle(
            "no Relay session exists in the current directory; run `relay start <goal>` first"
                .into(),
        ));
    }
    let name = command_name(&command);
    let _lock = is_mutating(&command)
        .then(|| ProjectLock::acquire(project, name))
        .transpose()?;
    if is_mutating(&command) && !matches!(command, Command::Doctor(_)) {
        materialize_legacy_session(&Paths::new(project))?;
    }
    let human = match command {
        Command::Start(args) => start(project, args),
        Command::Status => status(project),
        Command::Checkpoint(args) => checkpoint(project, args),
        Command::Finish => finish(project),
        Command::Handoff(args) => handoff(project, args),
        Command::List => return list_output(project),
        Command::Show(args) => return show_output(project, args),
        Command::History(args) => return history_output(project, args),
        Command::Resume(args) => resume(project, args),
        Command::Doctor(args) => return doctor(project, args),
        Command::Note(args) => note(project, args),
        Command::Context(args) => return context(project, args),
        Command::Completions(_) => unreachable!("completions are handled before project discovery"),
    }?;
    Ok(RunOutput {
        data: command_data(project, name, &human)?,
        human,
    })
}

fn list_output(project: &Path) -> Result<RunOutput, AppError> {
    let sessions = read_all_sessions(&Paths::new(project))?;
    Ok(RunOutput {
        human: if sessions.is_empty() {
            "No Relay sessions recorded".into()
        } else {
            sessions
                .iter()
                .map(|session| {
                    format!(
                        "{}  {}  {}  {}",
                        session.id,
                        state_name(&session.state),
                        session.started_at.to_rfc3339(),
                        session.goal
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
        data: json!({"sessions": sessions}),
    })
}

fn show_output(project: &Path, args: SessionSelectorArgs) -> Result<RunOutput, AppError> {
    let sessions = read_all_sessions(&Paths::new(project))?;
    let session = select_session(&sessions, &args.session)?.clone();
    Ok(RunOutput {
        human: show(
            project,
            SessionSelectorArgs {
                session: session.id.to_string(),
            },
        )?,
        data: json!({"session": session}),
    })
}

fn history_output(project: &Path, args: HistoryArgs) -> Result<RunOutput, AppError> {
    let paths = Paths::new(project);
    let all = read_events(&paths.events)?;
    let selected_id = args
        .session
        .as_deref()
        .map(|selector| {
            select_session(&read_all_sessions(&paths)?, selector).map(|session| session.id)
        })
        .transpose()?;
    let mut events = all
        .into_iter()
        .filter(|event| selected_id.is_none_or(|id| event.session_id == id))
        .filter(|event| {
            args.event_type
                .as_deref()
                .is_none_or(|kind| event.event_type == kind)
        })
        .collect::<Vec<_>>();
    let truncated = events.len() > args.limit;
    events.truncate(args.limit);
    let mut lines = events
        .iter()
        .map(|event| {
            format!(
                "{}  {}  {}  {}",
                event.timestamp.to_rfc3339(),
                event.session_id,
                event.event_type,
                event.id
            )
        })
        .collect::<Vec<_>>();
    lines.push(format!(
        "Truncated: {}",
        if truncated { "yes" } else { "no" }
    ));
    Ok(RunOutput {
        human: lines.join("\n"),
        data: json!({"events": events, "truncated": truncated}),
    })
}

fn is_mutating(command: &Command) -> bool {
    matches!(
        command,
        Command::Start(_)
            | Command::Checkpoint(_)
            | Command::Handoff(_)
            | Command::Finish
            | Command::Resume(_)
            | Command::Note(_)
            | Command::Doctor(DoctorArgs { repair: true })
    )
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Start(_) => "start",
        Command::Status => "status",
        Command::Checkpoint(_) => "checkpoint",
        Command::Handoff(_) => "handoff",
        Command::Finish => "finish",
        Command::List => "list",
        Command::Show(_) => "show",
        Command::History(_) => "history",
        Command::Resume(_) => "resume",
        Command::Doctor(_) => "doctor",
        Command::Note(_) => "note",
        Command::Context(_) => "context",
        Command::Completions(_) => "completions",
    }
}

fn command_data(project: &Path, command: &str, human: &str) -> Result<Value, AppError> {
    let paths = Paths::new(project);
    let sessions = read_all_sessions(&paths)?;
    let events = read_events(&paths.events)?;
    let current = if paths.session.exists() {
        Some(read_session(&paths.session)?)
    } else {
        None
    };
    let latest = events.last();
    Ok(match command {
        "list" => json!({ "sessions": sessions }),
        "show" => json!({ "session": sessions.first() }),
        "history" => json!({ "events": events, "truncated": human.ends_with("Truncated: yes") }),
        "handoff" => json!({
            "session": current,
            "markdown": human.rsplit_once("\nSaved: ").map(|v| v.0).unwrap_or(human),
            "path": latest.and_then(|e| e.payload.get("path")),
        }),
        "checkpoint" => {
            json!({ "session": current, "checkpoint": latest.and_then(|e| e.payload.get("checkpoint")), "path": latest.and_then(|e| e.payload.get("path")) })
        }
        "finish" => {
            json!({ "session": current, "summary_path": latest.and_then(|e| e.payload.get("summary_path")) })
        }
        "resume" | "start" | "status" => json!({
            "session": current,
            "git": git::capture(project)?,
            "event_count": current.as_ref().map(|s| events.iter().filter(|e| e.session_id == s.id).count()),
            "project": ".",
        }),
        "note" => json!({ "session": current, "note": latest.map(|e| &e.payload) }),
        _ => json!({}),
    })
}

fn start(project: &Path, args: StartArgs) -> Result<String, AppError> {
    let paths = Paths::new(project);
    if read_all_sessions(&paths)?
        .iter()
        .any(|session| session.state == SessionState::Active)
    {
        return Err(AppError::Lifecycle(
            "an active Relay session already exists; finish it before starting another".into(),
        ));
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
        resumed_from: None,
    };
    let mut operation = Operation::begin(
        &paths,
        "start",
        Some(session.id),
        vec![
            (
                "session_record",
                relative_to_project(project, &paths.session_record(session.id)),
            ),
            (
                "current_session",
                relative_to_project(project, &paths.session),
            ),
            ("event", relative_to_project(project, &paths.events)),
        ],
    )?;
    storage::write_json_atomic(&paths.session_record(session.id), &session)?;
    operation.advance("session_record")?;
    storage::write_json_atomic(&paths.session, &session)?;
    operation.advance("current_session")?;
    append_event(
        &paths.events,
        &session,
        now,
        "session.started",
        json!({ "goal": session.goal, "git": session.initial_git }),
    )?;
    operation.advance("event")?;
    operation.complete()?;

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
        "Goal: {}\nSession: {}\nState: {}\nStarted: {}\nResumed from: {}\nProject: .\nGit branch: {}\nGit commit: {}\nGit clean: {}\nEvents: {}\nLatest checkpoint: {}\nLast agent: {}",
        session.goal,
        session.id,
        state_name(&session.state),
        session.started_at.to_rfc3339(),
        session
            .resumed_from
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".into()),
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
    let mut operation = Operation::begin(
        &paths,
        "checkpoint",
        Some(session.id),
        vec![
            ("artifact", relative_to_project(project, &path)),
            ("event", relative_to_project(project, &paths.events)),
        ],
    )?;
    storage::write_json_atomic(&path, &checkpoint)?;
    operation.advance("artifact")?;
    let relative = relative_to_project(project, &path);
    append_event(
        &paths.events,
        &session,
        checkpoint.timestamp,
        "checkpoint.created",
        json!({ "checkpoint": checkpoint, "path": relative }),
    )?;
    operation.advance("event")?;
    operation.complete()?;

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
    let (checkpoints, _) = relevant_checkpoints(&events, session.id, 5)?;
    let session_notes = recorded_notes(&events, session.id);
    let markdown = render_handoff(
        project,
        &session,
        &args,
        &live_git,
        &checkpoints,
        &session_notes,
    );
    let filename = format!(
        "{}-{}.md",
        timestamp.format("%Y%m%dT%H%M%S%.3fZ"),
        Uuid::new_v4()
    );
    let path = paths.handoffs.join(filename);
    let relative = relative_to_project(project, &path);
    let mut operation = Operation::begin(
        &paths,
        "handoff",
        Some(session.id),
        vec![
            ("artifact", relative.clone()),
            ("event", relative_to_project(project, &paths.events)),
            (
                "session_record",
                relative_to_project(project, &paths.session_record(session.id)),
            ),
            (
                "current_session",
                relative_to_project(project, &paths.session),
            ),
        ],
    )?;

    // Keep the handoff recoverable before recording it, then record history
    // before advancing mutable agent context.
    storage::write_markdown_atomic(&path, &markdown)?;
    operation.advance("artifact")?;
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
    operation.advance("event")?;
    session.last_agent = Some(args.agent);
    storage::write_json_atomic(&paths.session_record(session.id), &session)?;
    operation.advance("session_record")?;
    storage::write_json_atomic(&paths.session, &session)?;
    operation.advance("current_session")?;
    operation.complete()?;

    Ok(format!("{markdown}\nSaved: {relative}"))
}

fn relevant_checkpoints(
    events: &[Event],
    session_id: Uuid,
    limit: usize,
) -> Result<(Vec<Checkpoint>, usize), AppError> {
    let mut checkpoint_events = events
        .iter()
        .filter(|event| event.session_id == session_id && event.event_type == "checkpoint.created")
        .collect::<Vec<_>>();
    let omitted = retain_latest(&mut checkpoint_events, limit);
    let mut checkpoints = Vec::with_capacity(checkpoint_events.len());
    for event in checkpoint_events {
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
    Ok((checkpoints, omitted))
}

fn render_handoff(
    project: &Path,
    session: &Session,
    args: &HandoffArgs,
    git: &GitSnapshot,
    checkpoints: &[Checkpoint],
    recorded: &[(NoteCategory, String, DateTime<Utc>)],
) -> String {
    let notes = |values: &[String], category: NoteCategory| {
        let mut combined = recorded
            .iter()
            .filter(|(candidate, _, _)| *candidate == category)
            .map(|(_, text, _)| text.as_str())
            .collect::<Vec<_>>();
        combined.extend(values.iter().map(String::as_str));
        if combined.is_empty() {
            "Not recorded".into()
        } else {
            combined
                .iter()
                .map(|value| format!("- {}", safe_markdown_value(project, value)))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let next_values = args.next_action.iter().cloned().collect::<Vec<_>>();
    let next_action = notes(&next_values, NoteCategory::NextAction);
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
        notes(&args.completed_work, NoteCategory::Progress),
        notes(&args.decision, NoteCategory::Decision),
        notes(&args.failure, NoteCategory::Blocker),
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
    let events = read_events(&paths.events)?;
    create_state_directories(&paths)?;
    let completed_at = Utc::now();
    let final_git = git::capture(project)?;
    let summary_path = paths.relay.join(format!("summary-{}.md", session.id));
    let summary_relative = relative_to_project(project, &summary_path);
    let notes = recorded_notes(&events, session.id);
    let markdown = render_summary(project, &session, completed_at, &final_git, &notes);
    let mut operation = Operation::begin(
        &paths,
        "finish",
        Some(session.id),
        vec![
            ("artifact", summary_relative.clone()),
            ("event", relative_to_project(project, &paths.events)),
            (
                "session_record",
                relative_to_project(project, &paths.session_record(session.id)),
            ),
            (
                "current_session",
                relative_to_project(project, &paths.session),
            ),
        ],
    )?;

    // Preserve recoverability: durable artifact and history precede the state transition.
    storage::write_markdown_atomic(&summary_path, &markdown)?;
    operation.advance("artifact")?;
    append_event(
        &paths.events,
        &session,
        completed_at,
        "session.finished",
        json!({ "git": final_git, "summary_path": summary_relative }),
    )?;
    operation.advance("event")?;
    session.state = SessionState::Completed;
    session.completed_at = Some(completed_at);
    session.final_git = Some(final_git);
    storage::write_json_atomic(&paths.session_record(session.id), &session)?;
    operation.advance("session_record")?;
    storage::write_json_atomic(&paths.session, &session)?;
    operation.advance("current_session")?;
    operation.complete()?;

    Ok(format!("Session finished\nSummary: {summary_relative}"))
}

fn render_summary(
    project: &Path,
    session: &Session,
    completed_at: DateTime<Utc>,
    git: &GitSnapshot,
    notes: &[(NoteCategory, String, DateTime<Utc>)],
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
    let category = |wanted: NoteCategory| {
        let values = notes
            .iter()
            .filter(|(category, _, _)| *category == wanted)
            .map(|(_, text, _)| format!("- {}", safe_markdown_value(project, text)))
            .collect::<Vec<_>>();
        if values.is_empty() {
            "Not recorded".into()
        } else {
            values.join("\n")
        }
    };
    format!(
        "# Relay session summary\n\n\
         ## Goal\n\n{}\n\n\
         ## Session\n\n- Started: {}\n- Finished: {}\n- Last agent: {}\n\n\
         ## Final Git state\n\n- Branch: {}\n- Commit: {}\n- Clean: {}\n\n\
         ## Changed files\n\n{}\n\n\
         ## Progress\n\n{}\n\n\
         ## Important decisions\n\n{}\n\n\
         ## Blockers\n\n{}\n\n\
         ## Observations\n\n{}\n\n\
         ## Suggested next actions\n\n{}\n",
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
        category(NoteCategory::Progress),
        category(NoteCategory::Decision),
        category(NoteCategory::Blocker),
        category(NoteCategory::Observation),
        category(NoteCategory::NextAction),
    )
}

fn show(project: &Path, args: SessionSelectorArgs) -> Result<String, AppError> {
    let sessions = read_all_sessions(&Paths::new(project))?;
    let session = select_session(&sessions, &args.session)?;
    Ok(format!(
        "Session: {}\nGoal: {}\nState: {}\nStarted: {}\nCompleted: {}\nResumed from: {}\nLast agent: {}",
        session.id,
        session.goal,
        state_name(&session.state),
        session.started_at.to_rfc3339(),
        session
            .completed_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| "not completed".into()),
        session
            .resumed_from
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".into()),
        session.last_agent.as_deref().unwrap_or("unknown")
    ))
}

fn resume(project: &Path, args: SessionSelectorArgs) -> Result<String, AppError> {
    let paths = Paths::new(project);
    if read_all_sessions(&paths)?
        .iter()
        .any(|session| session.state == SessionState::Active)
    {
        return Err(AppError::Lifecycle(
            "an active Relay session already exists; finish it before resuming another".into(),
        ));
    }
    let sessions = read_all_sessions(&paths)?;
    let source = select_session(&sessions, &args.session)?.clone();
    if source.state != SessionState::Completed {
        return Err(AppError::Lifecycle(format!(
            "session {} is active; only completed sessions can be resumed",
            source.id
        )));
    }
    read_events(&paths.events)?;
    create_state_directories(&paths)?;
    let now = Utc::now();
    let session = Session {
        schema_version: SCHEMA_VERSION,
        id: Uuid::new_v4(),
        goal: source.goal.clone(),
        state: SessionState::Active,
        started_at: now,
        completed_at: None,
        initial_git: git::capture(project)?,
        final_git: None,
        last_agent: None,
        resumed_from: Some(source.id),
    };
    let mut operation = Operation::begin(
        &paths,
        "resume",
        Some(session.id),
        vec![
            (
                "session_record",
                relative_to_project(project, &paths.session_record(session.id)),
            ),
            (
                "current_session",
                relative_to_project(project, &paths.session),
            ),
            ("event", relative_to_project(project, &paths.events)),
        ],
    )?;
    storage::write_json_atomic(&paths.session_record(session.id), &session)?;
    operation.advance("session_record")?;
    storage::write_json_atomic(&paths.session, &session)?;
    operation.advance("current_session")?;
    append_event(
        &paths.events,
        &session,
        now,
        "session.resumed",
        json!({ "goal": session.goal, "resumed_from": source.id, "git": session.initial_git }),
    )?;
    operation.advance("event")?;
    operation.complete()?;
    Ok(format!(
        "Resumed Relay session\nSource: {}\nSession: {}\nGoal: {}",
        source.id, session.id, session.goal
    ))
}

const MAX_NOTE_BYTES: usize = 64 * 1024;

fn note(project: &Path, args: NoteArgs) -> Result<String, AppError> {
    let paths = Paths::new(project);
    let session = require_active(&paths)?;
    let text = read_note_text(args.text, args.stdin, args.editor)?;
    if text.trim().is_empty() {
        return Err(AppError::Lifecycle("note text must not be blank".into()));
    }
    if text.len() > MAX_NOTE_BYTES {
        return Err(AppError::Lifecycle(format!(
            "note exceeds the {MAX_NOTE_BYTES}-byte limit"
        )));
    }
    let timestamp = Utc::now();
    append_event(
        &paths.events,
        &session,
        timestamp,
        "note.recorded",
        json!({"category": args.category.as_str(), "text": text}),
    )?;
    Ok(format!(
        "Recorded {} note for session {}",
        args.category.as_str(),
        session.id
    ))
}

fn read_note_text(
    positional: Option<String>,
    explicit_stdin: bool,
    editor: bool,
) -> Result<String, AppError> {
    if let Some(text) = positional {
        return Ok(text);
    }
    if explicit_stdin {
        let mut bytes = Vec::new();
        io::stdin()
            .take((MAX_NOTE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                AppError::Lifecycle(format!("could not read note from stdin: {error}"))
            })?;
        if bytes.len() > MAX_NOTE_BYTES {
            return Err(AppError::Lifecycle(format!(
                "note exceeds the {MAX_NOTE_BYTES}-byte limit"
            )));
        }
        return String::from_utf8(bytes)
            .map_err(|_| AppError::Lifecycle("note input must be valid UTF-8".into()));
    }
    if editor || io::stdin().is_terminal() {
        let editor_command = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());
        let temporary = tempfile::NamedTempFile::new().map_err(|error| {
            AppError::Lifecycle(format!("could not create editor note file: {error}"))
        })?;
        let mut parts = editor_command.split_whitespace();
        let executable = parts
            .next()
            .ok_or_else(|| AppError::Lifecycle("configured editor is blank".into()))?;
        let status = ProcessCommand::new(executable)
            .args(parts)
            .arg(temporary.path())
            .status()
            .map_err(|error| AppError::Lifecycle(format!("could not launch editor: {error}")))?;
        if !status.success() {
            return Err(AppError::Lifecycle(format!(
                "editor exited unsuccessfully with {status}"
            )));
        }
        let bytes = fs::read(temporary.path()).map_err(|error| {
            AppError::Lifecycle(format!("could not read editor note file: {error}"))
        })?;
        if bytes.len() > MAX_NOTE_BYTES {
            return Err(AppError::Lifecycle(format!(
                "note exceeds the {MAX_NOTE_BYTES}-byte limit"
            )));
        }
        return String::from_utf8(bytes)
            .map_err(|_| AppError::Lifecycle("editor note must be valid UTF-8".into()));
    }
    if !io::stdin().is_terminal() {
        let mut bytes = Vec::new();
        io::stdin()
            .take((MAX_NOTE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                AppError::Lifecycle(format!("could not read note from stdin: {error}"))
            })?;
        if bytes.len() > MAX_NOTE_BYTES {
            return Err(AppError::Lifecycle(format!(
                "note exceeds the {MAX_NOTE_BYTES}-byte limit"
            )));
        }
        return String::from_utf8(bytes)
            .map_err(|_| AppError::Lifecycle("note input must be valid UTF-8".into()));
    }
    Err(AppError::Lifecycle(
        "provide note text, pipe stdin, or use --editor".into(),
    ))
}

fn recorded_notes(
    events: &[Event],
    session_id: Uuid,
) -> Vec<(NoteCategory, String, DateTime<Utc>)> {
    events
        .iter()
        .filter(|event| event.session_id == session_id && event.event_type == "note.recorded")
        .filter_map(|event| {
            let category = match event.payload.get("category")?.as_str()? {
                "progress" => NoteCategory::Progress,
                "decision" => NoteCategory::Decision,
                "blocker" => NoteCategory::Blocker,
                "observation" => NoteCategory::Observation,
                "next_action" => NoteCategory::NextAction,
                _ => return None,
            };
            Some((
                category,
                event.payload.get("text")?.as_str()?.to_owned(),
                event.timestamp,
            ))
        })
        .collect()
}

fn retain_latest<T>(items: &mut Vec<T>, limit: usize) -> usize {
    let omitted = items.len().saturating_sub(limit);
    drop(items.drain(..omitted));
    omitted
}

fn render_context_event(project: &Path, event: &Event) -> String {
    let mut rendered = format!(
        "\n- Event {} — {}",
        event.timestamp.to_rfc3339(),
        event.event_type
    );
    if event.event_type != "handoff.generated" {
        return rendered;
    }

    if let Some(agent) = event.payload.get("agent").and_then(Value::as_str) {
        let agent = safe_markdown_value(project, agent);
        if !agent.is_empty() {
            rendered.push_str("\n  - Target agent: ");
            rendered.push_str(&agent);
        }
    }

    for (key, label) in [
        ("completed_work", "Completed work"),
        ("decisions", "Decisions"),
        ("failures", "Failures"),
    ] {
        let Some(values) = event.payload.get(key).and_then(Value::as_array) else {
            continue;
        };
        let mut wrote_label = false;
        for value in values.iter().filter_map(Value::as_str) {
            let value = safe_markdown_value(project, value);
            if value.is_empty() {
                continue;
            }
            if !wrote_label {
                rendered.push_str("\n  - ");
                rendered.push_str(label);
                rendered.push(':');
                wrote_label = true;
            }
            rendered.push_str("\n    - ");
            rendered.push_str(&value);
        }
    }

    if let Some(next_action) = event.payload.get("next_action").and_then(Value::as_str) {
        let next_action = safe_markdown_value(project, next_action);
        if !next_action.is_empty() {
            rendered.push_str("\n  - Next actions:\n    - ");
            rendered.push_str(&next_action);
        }
    }

    rendered
}

// The CLI writes human output with `println!`, which appends one Unicode character.
const CONTEXT_TRANSPORT_NEWLINE_CHARS: usize = 1;
const CONTEXT_FOOTER_TRUNCATED: &str = "\n\nTruncated: ";
const CONTEXT_FOOTER_COUNT_NOTES: &str = "\nCount-omitted notes: ";
const CONTEXT_FOOTER_COUNT_CHECKPOINTS: &str = "\nCount-omitted checkpoints: ";
const CONTEXT_FOOTER_COUNT_EVENTS: &str = "\nCount-omitted events: ";
const CONTEXT_FOOTER_BUDGET_NOTES: &str = "\nBudget-omitted notes: ";
const CONTEXT_FOOTER_BUDGET_CHECKPOINTS: &str = "\nBudget-omitted checkpoints: ";
const CONTEXT_FOOTER_BUDGET_EVENTS: &str = "\nBudget-omitted events: ";

#[derive(Clone, Copy, Default)]
struct ContextOmissions {
    notes: usize,
    checkpoints: usize,
    events: usize,
}

impl ContextOmissions {
    fn any(self) -> bool {
        self.notes > 0 || self.checkpoints > 0 || self.events > 0
    }

    fn decrement(&mut self, kind: ContextItemKind) {
        let count = match kind {
            ContextItemKind::Note => &mut self.notes,
            ContextItemKind::Checkpoint => &mut self.checkpoints,
            ContextItemKind::Event => &mut self.events,
        };
        *count = count.saturating_sub(1);
    }
}

#[derive(Clone, Copy)]
enum ContextItemKind {
    Note,
    Checkpoint,
    Event,
}

struct ContextItem {
    kind: ContextItemKind,
    rendered: String,
    chars: usize,
}

impl ContextItem {
    fn new(kind: ContextItemKind, rendered: String) -> Self {
        let chars = rendered.chars().count();
        Self {
            kind,
            rendered,
            chars,
        }
    }
}

fn decimal_chars(mut value: usize) -> usize {
    let mut chars = 1;
    while value >= 10 {
        value /= 10;
        chars += 1;
    }
    chars
}

fn context_footer_chars(
    truncated: bool,
    count_omitted: ContextOmissions,
    budget_omitted: ContextOmissions,
) -> usize {
    [
        CONTEXT_FOOTER_TRUNCATED,
        CONTEXT_FOOTER_COUNT_NOTES,
        CONTEXT_FOOTER_COUNT_CHECKPOINTS,
        CONTEXT_FOOTER_COUNT_EVENTS,
        CONTEXT_FOOTER_BUDGET_NOTES,
        CONTEXT_FOOTER_BUDGET_CHECKPOINTS,
        CONTEXT_FOOTER_BUDGET_EVENTS,
    ]
    .iter()
    .map(|text| text.chars().count())
    .sum::<usize>()
        + if truncated { 3 } else { 2 }
        + decimal_chars(count_omitted.notes)
        + decimal_chars(count_omitted.checkpoints)
        + decimal_chars(count_omitted.events)
        + decimal_chars(budget_omitted.notes)
        + decimal_chars(budget_omitted.checkpoints)
        + decimal_chars(budget_omitted.events)
        + 1
}

fn context_footer(
    truncated: bool,
    count_omitted: ContextOmissions,
    budget_omitted: ContextOmissions,
) -> String {
    format!(
        "{CONTEXT_FOOTER_TRUNCATED}{}{CONTEXT_FOOTER_COUNT_NOTES}{}{CONTEXT_FOOTER_COUNT_CHECKPOINTS}{}{CONTEXT_FOOTER_COUNT_EVENTS}{}{CONTEXT_FOOTER_BUDGET_NOTES}{}{CONTEXT_FOOTER_BUDGET_CHECKPOINTS}{}{CONTEXT_FOOTER_BUDGET_EVENTS}{}\n",
        if truncated { "yes" } else { "no" },
        count_omitted.notes,
        count_omitted.checkpoints,
        count_omitted.events,
        budget_omitted.notes,
        budget_omitted.checkpoints,
        budget_omitted.events,
    )
}

fn compact_context(count_omitted: ContextOmissions, budget_omitted: ContextOmissions) -> String {
    format!(
        "# Relay\nTruncated: yes\nCount N/C/E: {}/{}/{}\nBudget N/C/E: {}/{}/{}\n",
        count_omitted.notes,
        count_omitted.checkpoints,
        count_omitted.events,
        budget_omitted.notes,
        budget_omitted.checkpoints,
        budget_omitted.events,
    )
}

fn budget_context(
    header: String,
    items: &[ContextItem],
    count_omitted: ContextOmissions,
    max_chars: usize,
) -> (String, ContextOmissions, bool) {
    let mut budget_omitted = ContextOmissions::default();
    for item in items {
        match item.kind {
            ContextItemKind::Note => budget_omitted.notes += 1,
            ContextItemKind::Checkpoint => budget_omitted.checkpoints += 1,
            ContextItemKind::Event => budget_omitted.events += 1,
        }
    }

    let header_chars = header.chars().count();
    let footer_chars = context_footer_chars(
        count_omitted.any() || budget_omitted.any(),
        count_omitted,
        budget_omitted,
    );
    if header_chars.saturating_add(footer_chars) > max_chars {
        let compact = compact_context(count_omitted, budget_omitted);
        let human = if compact.chars().count() <= max_chars {
            compact
        } else {
            "# Relay\nTruncated: yes\n".into()
        };
        return (human, budget_omitted, true);
    }

    let mut included = vec![false; items.len()];
    let mut included_chars = 0usize;
    loop {
        let mut changed = false;
        for (index, item) in items.iter().enumerate() {
            if included[index] {
                continue;
            }
            let mut tentative_omitted = budget_omitted;
            tentative_omitted.decrement(item.kind);
            let tentative_footer_chars = context_footer_chars(
                count_omitted.any() || tentative_omitted.any(),
                count_omitted,
                tentative_omitted,
            );
            let item_chars = item.chars;
            let total_chars = header_chars
                .saturating_add(included_chars)
                .saturating_add(item_chars)
                .saturating_add(tentative_footer_chars);
            if total_chars <= max_chars {
                included[index] = true;
                included_chars = included_chars.saturating_add(item_chars);
                budget_omitted = tentative_omitted;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let budget_truncated = budget_omitted.any();
    let mut markdown = header;
    for (item, included) in items.iter().zip(included) {
        if included {
            markdown.push_str(&item.rendered);
        }
    }
    markdown.push_str(&context_footer(
        count_omitted.any() || budget_truncated,
        count_omitted,
        budget_omitted,
    ));
    (markdown, budget_omitted, budget_truncated)
}

fn context(project: &Path, args: ContextArgs) -> Result<RunOutput, AppError> {
    if args.max_chars < 64 {
        return Err(AppError::Lifecycle(
            "--max-chars must be at least 64".into(),
        ));
    }
    let paths = Paths::new(project);
    let sessions = read_all_sessions(&paths)?;
    let session = if let Some(selector) = args.session.as_deref() {
        select_session(&sessions, selector)?.clone()
    } else {
        require_session(&paths)?
    };
    let all_events = read_events(&paths.events)?;
    let selected_categories: HashSet<_> =
        args.categories.iter().map(|value| value.as_str()).collect();
    let selected_kinds: HashSet<_> = args.kinds.iter().map(String::as_str).collect();
    let mut notes = recorded_notes(&all_events, session.id)
        .into_iter()
        .filter(|(category, _, _)| {
            selected_categories.is_empty() || selected_categories.contains(category.as_str())
        })
        .collect::<Vec<_>>();
    let omitted_notes = retain_latest(&mut notes, args.notes);
    let mut events = all_events
        .iter()
        .filter(|event| event.session_id == session.id)
        .filter(|event| {
            selected_kinds.is_empty() || selected_kinds.contains(event.event_type.as_str())
        })
        .collect::<Vec<_>>();
    let omitted_events = retain_latest(&mut events, args.events);
    let (checkpoints, omitted_checkpoints) =
        relevant_checkpoints(&all_events, session.id, args.checkpoints)?;
    let live_git = git::capture(project)?;

    let header = format!(
        "# Relay context\n\n## Session\n\n- ID: {}\n- Goal: {}\n- State: {}\n- Resumed from: {}\n- Last agent: {}\n\n## Current Git state\n\n- Branch: {}\n- Commit: {}\n- Clean: {}\n",
        session.id,
        safe_markdown_value(project, &session.goal),
        state_name(&session.state),
        session
            .resumed_from
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".into()),
        safe_markdown_value(
            project,
            session.last_agent.as_deref().unwrap_or("not recorded")
        ),
        safe_markdown_value(
            project,
            display_option(live_git.branch.as_deref(), live_git.available)
        ),
        safe_markdown_value(
            project,
            display_option(live_git.commit.as_deref(), live_git.available)
        ),
        display_clean(&live_git),
    );
    let items = notes
        .iter()
        .map(|(category, text, time)| {
            ContextItem::new(
                ContextItemKind::Note,
                format!(
                    "\n- Note [{}] {} — {}",
                    category.as_str(),
                    time.to_rfc3339(),
                    safe_markdown_value(project, text)
                ),
            )
        })
        .chain(checkpoints.iter().map(|checkpoint| {
            ContextItem::new(
                ContextItemKind::Checkpoint,
                format!(
                    "\n- Checkpoint {} — {}",
                    checkpoint.timestamp.to_rfc3339(),
                    safe_markdown_value(
                        project,
                        checkpoint.message.as_deref().unwrap_or("(no message)")
                    )
                ),
            )
        }))
        .chain(events.iter().map(|event| {
            ContextItem::new(ContextItemKind::Event, render_context_event(project, event))
        }))
        .collect::<Vec<_>>();
    let count_omitted = ContextOmissions {
        notes: omitted_notes,
        checkpoints: omitted_checkpoints,
        events: omitted_events,
    };
    let (markdown, budget_omitted, budget_truncated) = budget_context(
        header,
        &items,
        count_omitted,
        args.max_chars - CONTEXT_TRANSPORT_NEWLINE_CHARS,
    );
    let truncated = count_omitted.any() || budget_truncated;

    Ok(RunOutput {
        human: markdown,
        data: json!({
            "session": session,
            "git": live_git,
            "notes": notes.iter().map(|(category, text, timestamp)| json!({"category": category.as_str(), "text": text, "timestamp": timestamp})).collect::<Vec<_>>(),
            "checkpoints": checkpoints,
            "events": events,
            "truncated": truncated,
            "omitted": {"notes": omitted_notes, "checkpoints": omitted_checkpoints, "events": omitted_events},
            "budget_omitted": {"notes": budget_omitted.notes, "checkpoints": budget_omitted.checkpoints, "events": budget_omitted.events},
            "budget_truncated": budget_truncated,
            "max_chars": args.max_chars,
            "project": ".",
        }),
    })
}

fn doctor(project: &Path, args: DoctorArgs) -> Result<RunOutput, AppError> {
    let paths = Paths::new(project);
    let mut diagnostics = inspect_state(project, &paths);
    let repairable = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.repairable)
        .map(|diagnostic| diagnostic.path.clone())
        .collect::<Vec<_>>();
    let unsafe_count = diagnostics
        .iter()
        .filter(|diagnostic| !diagnostic.repairable)
        .count();
    let mut repaired = Vec::new();
    if args.repair {
        if unsafe_count > 0 {
            return Err(AppError::Lifecycle(
                "doctor refused repair because state contains ambiguous or destructive problems; no repairs were performed".into(),
            ));
        }
        for relative in repairable {
            let path = project.join(&relative);
            storage::remove_file_synced(&path)?;
            repaired.push(relative);
        }
        if !repaired.is_empty() {
            if let Ok(session) = require_active(&paths)
                && read_events(&paths.events).is_ok()
            {
                append_event(
                    &paths.events,
                    &session,
                    Utc::now(),
                    "doctor.repaired",
                    json!({"actions": repaired.iter().map(|path| json!({"action": "remove_completed_operation_marker", "path": path})).collect::<Vec<_>>()}),
                )?;
            }
            diagnostics = inspect_state(project, &paths);
        }
    }
    let error_count = diagnostics
        .iter()
        .filter(|value| value.severity == DiagnosticSeverity::Error)
        .count();
    let warning_count = diagnostics.len() - error_count;
    let healthy = error_count == 0;
    let human = if diagnostics.is_empty() {
        "Relay state is healthy\nErrors: 0\nWarnings: 0".into()
    } else {
        let lines = diagnostics
            .iter()
            .map(|item| {
                format!(
                    "{} [{}] {}: {}\n  Guidance: {}",
                    match item.severity {
                        DiagnosticSeverity::Error => "ERROR",
                        DiagnosticSeverity::Warning => "WARNING",
                    },
                    item.code,
                    item.path,
                    item.message,
                    item.guidance
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Relay state diagnostics\nErrors: {error_count}\nWarnings: {warning_count}\n{lines}"
        )
    };
    Ok(RunOutput {
        human,
        data: json!({
            "healthy": healthy,
            "counts": {"errors": error_count, "warnings": warning_count},
            "diagnostics": diagnostics,
            "repaired": repaired,
        }),
    })
}

fn inspect_state(project: &Path, paths: &Paths) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut events = Vec::new();
    if !paths.relay.exists() {
        push_diagnostic(
            &mut diagnostics,
            "state_missing",
            DiagnosticSeverity::Error,
            ".relay",
            "Relay state directory is missing",
            "Run `relay start <goal>` at the intended scope.",
            false,
        );
        return diagnostics;
    }
    if paths.events.exists() {
        match fs::read(&paths.events) {
            Ok(bytes) if !bytes.is_empty() && !bytes.ends_with(b"\n") => push_diagnostic(
                &mut diagnostics,
                "events_partial_line",
                DiagnosticSeverity::Error,
                ".relay/events.jsonl",
                "the final JSONL record is not newline-terminated",
                "Restore the complete line manually; Relay will not truncate the log.",
                false,
            ),
            Ok(bytes) => {
                for (index, line) in bytes
                    .strip_suffix(b"\n")
                    .unwrap_or(&bytes)
                    .split(|byte| *byte == b'\n')
                    .enumerate()
                {
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_slice::<Event>(line) {
                        Ok(event) if event.schema_version != SCHEMA_VERSION => push_diagnostic(
                            &mut diagnostics,
                            "unsupported_event_schema",
                            DiagnosticSeverity::Error,
                            ".relay/events.jsonl",
                            &format!(
                                "event at line {} uses schema version {}",
                                index + 1,
                                event.schema_version
                            ),
                            "Use a Relay version supporting this schema; it will not be rewritten.",
                            false,
                        ),
                        Ok(event) => events.push(event),
                        Err(error) => push_diagnostic(
                            &mut diagnostics,
                            "events_malformed_json",
                            DiagnosticSeverity::Error,
                            ".relay/events.jsonl",
                            &format!("line {} is malformed: {error}", index + 1),
                            "Repair the source record manually; Relay will not discard it.",
                            false,
                        ),
                    }
                }
            }
            Err(error) => push_diagnostic(
                &mut diagnostics,
                "events_unreadable",
                DiagnosticSeverity::Error,
                ".relay/events.jsonl",
                &error.to_string(),
                "Correct filesystem permissions or damage manually.",
                false,
            ),
        }
    } else {
        push_diagnostic(
            &mut diagnostics,
            "events_missing",
            DiagnosticSeverity::Error,
            ".relay/events.jsonl",
            "event log is missing",
            "Restore it from backup; Relay will not invent history.",
            false,
        );
    }

    let mut sessions = Vec::new();
    for path in state_json_files(paths) {
        let relative = relative_to_project(project, &path);
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Session>(&bytes) {
                Ok(session) if session.schema_version != SCHEMA_VERSION => push_diagnostic(
                    &mut diagnostics,
                    "unsupported_session_schema",
                    DiagnosticSeverity::Error,
                    &relative,
                    &format!("session uses schema version {}", session.schema_version),
                    "Use a Relay version supporting this schema; it will not be rewritten.",
                    false,
                ),
                Ok(session) => {
                    let invalid = (session.state == SessionState::Active
                        && (session.completed_at.is_some() || session.final_git.is_some()))
                        || (session.state == SessionState::Completed
                            && (session.completed_at.is_none() || session.final_git.is_none()));
                    if invalid {
                        push_diagnostic(
                            &mut diagnostics,
                            "invalid_session_lifecycle",
                            DiagnosticSeverity::Error,
                            &relative,
                            "session state disagrees with completion fields",
                            "Resolve lifecycle truth manually from durable evidence.",
                            false,
                        );
                    }
                    sessions.push((relative, session));
                }
                Err(error) => push_diagnostic(
                    &mut diagnostics,
                    "session_malformed_json",
                    DiagnosticSeverity::Error,
                    &relative,
                    &error.to_string(),
                    "Repair or restore this document manually.",
                    false,
                ),
            },
            Err(error) => push_diagnostic(
                &mut diagnostics,
                "session_unreadable",
                DiagnosticSeverity::Error,
                &relative,
                &error.to_string(),
                "Correct filesystem permissions or damage manually.",
                false,
            ),
        }
    }
    if !paths.session.exists() {
        push_diagnostic(
            &mut diagnostics,
            "current_session_missing",
            DiagnosticSeverity::Error,
            ".relay/session.json",
            "current session record is missing",
            "Restore it from valid durable evidence; Relay will not guess which session is current.",
            false,
        );
    }
    if let Some((_, current)) = sessions
        .iter()
        .find(|(path, _)| path == ".relay/session.json")
    {
        if let Some((relative, durable)) = sessions
            .iter()
            .find(|(path, session)| path != ".relay/session.json" && session.id == current.id)
        {
            if durable != current {
                push_diagnostic(
                    &mut diagnostics,
                    "current_session_disagrees",
                    DiagnosticSeverity::Error,
                    relative,
                    "current and per-session records disagree",
                    "Compare durable events and resolve the records manually.",
                    false,
                );
            }
        } else {
            push_diagnostic(
                &mut diagnostics,
                "session_record_missing",
                DiagnosticSeverity::Error,
                &format!(".relay/sessions/{}.json", current.id),
                "current session has no per-session record",
                "Run a normal locked mutation only if the current record is known valid.",
                false,
            );
        }
    }
    for (_, session) in &sessions {
        let starts = events
            .iter()
            .filter(|event| {
                event.session_id == session.id
                    && matches!(
                        event.event_type.as_str(),
                        "session.started" | "session.resumed"
                    )
            })
            .count();
        let finishes = events
            .iter()
            .filter(|event| {
                event.session_id == session.id && event.event_type == "session.finished"
            })
            .count();
        if starts != 1
            || finishes > 1
            || (session.state == SessionState::Completed && finishes != 1)
        {
            push_diagnostic(
                &mut diagnostics,
                "lifecycle_event_inconsistent",
                DiagnosticSeverity::Error,
                ".relay/events.jsonl",
                &format!(
                    "session {} has {starts} start/resume and {finishes} finish events",
                    session.id
                ),
                "Resolve duplicate or missing lifecycle evidence manually.",
                false,
            );
        }
    }

    let referenced = events
        .iter()
        .filter_map(|event| {
            event
                .payload
                .get("path")
                .or_else(|| event.payload.get("summary_path"))
                .and_then(Value::as_str)
        })
        .collect::<HashSet<_>>();
    inspect_artifacts(
        project,
        &paths.checkpoints,
        "checkpoint",
        &referenced,
        &mut diagnostics,
    );
    inspect_artifacts(
        project,
        &paths.handoffs,
        "handoff",
        &referenced,
        &mut diagnostics,
    );
    for path in referenced {
        if !project.join(path).exists() {
            push_diagnostic(
                &mut diagnostics,
                "referenced_artifact_missing",
                DiagnosticSeverity::Error,
                path,
                "an event references a missing artifact",
                "Restore the referenced artifact; Relay will not reconstruct user content.",
                false,
            );
        }
    }
    if paths.operations.exists()
        && let Ok(entries) = fs::read_dir(&paths.operations)
    {
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let relative = relative_to_project(project, &entry.path());
            match storage::read_json::<OperationMarker>(&entry.path()) {
                Ok(marker) if marker.schema_version != SCHEMA_VERSION => push_diagnostic(
                    &mut diagnostics,
                    "unsupported_operation_schema",
                    DiagnosticSeverity::Error,
                    &relative,
                    &format!(
                        "operation marker uses schema version {}",
                        marker.schema_version
                    ),
                    "Use a compatible Relay version; do not rewrite it.",
                    false,
                ),
                Ok(marker) => {
                    let complete = operation_is_provably_complete(project, &marker, &events);
                    if complete {
                        push_diagnostic(
                            &mut diagnostics,
                            "completed_operation_marker",
                            DiagnosticSeverity::Warning,
                            &relative,
                            "all declared effects are complete but the marker remains",
                            "Run `relay doctor --repair` to remove only this stale marker.",
                            true,
                        );
                    } else {
                        push_diagnostic(
                            &mut diagnostics,
                            "incomplete_operation",
                            DiagnosticSeverity::Error,
                            &relative,
                            "one or more declared effects are incomplete or missing",
                            "Inspect the listed durable effects and resolve manually; Relay will not guess.",
                            false,
                        );
                    }
                }
                Err(error) => push_diagnostic(
                    &mut diagnostics,
                    "operation_malformed_json",
                    DiagnosticSeverity::Error,
                    &relative,
                    &error.to_string(),
                    "Preserve and inspect this interruption evidence manually.",
                    false,
                ),
            }
        }
    }
    diagnostics.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.code.cmp(&right.code))
    });
    diagnostics
}

fn state_json_files(paths: &Paths) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if paths.session.exists() {
        files.push(paths.session.clone());
    }
    if let Ok(entries) = fs::read_dir(&paths.sessions) {
        files.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("json")),
        );
    }
    files.sort();
    files
}

fn operation_is_provably_complete(
    project: &Path,
    marker: &OperationMarker,
    events: &[Event],
) -> bool {
    let paths_safe = marker.effects.iter().all(|effect| {
        let path = Path::new(&effect.path);
        effect.completed
            && !path.is_absolute()
            && path.components().all(|component| {
                matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
            && effect.path.starts_with(".relay/")
            && project.join(path).exists()
    });
    if !paths_safe {
        return false;
    }
    let expected_event = match marker.command.as_str() {
        "start" => Some("session.started"),
        "resume" => Some("session.resumed"),
        "checkpoint" => Some("checkpoint.created"),
        "handoff" => Some("handoff.generated"),
        "finish" => Some("session.finished"),
        "note" => Some("note.recorded"),
        "history.materialize" => None,
        _ => return false,
    };
    expected_event.is_none_or(|kind| {
        marker.session_id.is_some_and(|session_id| {
            events
                .iter()
                .any(|event| event.session_id == session_id && event.event_type == kind)
        })
    })
}

fn inspect_artifacts(
    project: &Path,
    directory: &Path,
    kind: &str,
    referenced: &HashSet<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let relative = relative_to_project(project, &entry.path());
        if kind == "checkpoint" && storage::read_json::<Checkpoint>(&entry.path()).is_err() {
            push_diagnostic(
                diagnostics,
                "artifact_malformed",
                DiagnosticSeverity::Error,
                &relative,
                "checkpoint artifact is malformed",
                "Restore or repair it manually; Relay will not discard it.",
                false,
            );
        }
        if !referenced.contains(relative.as_str()) {
            push_diagnostic(
                diagnostics,
                "orphaned_artifact",
                DiagnosticSeverity::Warning,
                &relative,
                &format!("{kind} artifact has no matching event reference"),
                "Preserve it while investigating the interrupted operation.",
                false,
            );
        }
    }
}

fn push_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    code: &str,
    severity: DiagnosticSeverity,
    path: &str,
    message: &str,
    guidance: &str,
    repairable: bool,
) {
    diagnostics.push(Diagnostic {
        code: code.into(),
        severity,
        path: path.into(),
        message: message.into(),
        guidance: guidance.into(),
        repairable,
    });
}

fn read_all_sessions(paths: &Paths) -> Result<Vec<Session>, AppError> {
    let mut sessions = Vec::new();
    if paths.sessions.exists() {
        let mut entries = fs::read_dir(&paths.sessions)
            .map_err(|source| AppError::Io {
                path: paths.sessions.clone(),
                source,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| AppError::Io {
                path: paths.sessions.clone(),
                source,
            })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
                sessions.push(read_session(&entry.path())?);
            }
        }
    }
    if paths.session.exists() {
        let current = read_session(&paths.session)?;
        if !sessions.iter().any(|session| session.id == current.id) {
            sessions.push(current);
        }
    }
    let history = read_events(&paths.events)?;
    for event in &history {
        if sessions
            .iter()
            .any(|session| session.id == event.session_id)
        {
            continue;
        }
        if event.event_type != "session.started" && event.event_type != "session.resumed" {
            continue;
        }
        let goal = event.payload["goal"]
            .as_str()
            .ok_or_else(|| {
                AppError::Lifecycle(format!(
                    "{} event {} is missing a goal",
                    event.event_type, event.id
                ))
            })?
            .to_owned();
        let initial_git: GitSnapshot = serde_json::from_value(event.payload["git"].clone())
            .map_err(|error| {
                AppError::Lifecycle(format!(
                    "{} event {} has malformed Git data: {error}",
                    event.event_type, event.id
                ))
            })?;
        validate_git_version(&initial_git)?;
        let resumed_from = event.payload["resumed_from"]
            .as_str()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|error| {
                AppError::Lifecycle(format!(
                    "{} event {} has malformed lineage: {error}",
                    event.event_type, event.id
                ))
            })?;
        let finished = history.iter().find(|candidate| {
            candidate.session_id == event.session_id && candidate.event_type == "session.finished"
        });
        let (state, completed_at, final_git) = if let Some(finished) = finished {
            let git: GitSnapshot = serde_json::from_value(finished.payload["git"].clone())
                .map_err(|error| {
                    AppError::Lifecycle(format!(
                        "session.finished event {} has malformed Git data: {error}",
                        finished.id
                    ))
                })?;
            validate_git_version(&git)?;
            (SessionState::Completed, Some(finished.timestamp), Some(git))
        } else {
            (SessionState::Active, None, None)
        };
        let last_agent = history
            .iter()
            .rev()
            .find(|candidate| {
                candidate.session_id == event.session_id
                    && candidate.event_type == "handoff.generated"
            })
            .and_then(|candidate| candidate.payload["agent"].as_str())
            .map(str::to_owned);
        sessions.push(Session {
            schema_version: SCHEMA_VERSION,
            id: event.session_id,
            goal,
            state,
            started_at: event.timestamp,
            completed_at,
            initial_git,
            final_git,
            last_agent,
            resumed_from,
        });
    }
    sessions.sort_by(|left, right| {
        right
            .started_at
            .cmp(&left.started_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(sessions)
}

fn select_session<'a>(sessions: &'a [Session], selector: &str) -> Result<&'a Session, AppError> {
    let selector = selector.to_ascii_lowercase();
    let matches = sessions
        .iter()
        .filter(|session| session.id.to_string().starts_with(&selector))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [session] => Ok(session),
        [] => Err(AppError::Lifecycle(format!(
            "no Relay session matches `{selector}`"
        ))),
        _ => Err(AppError::Lifecycle(format!(
            "session selector `{selector}` is ambiguous; use more UUID characters"
        ))),
    }
}

fn materialize_legacy_session(paths: &Paths) -> Result<(), AppError> {
    if !paths.session.exists() {
        return Ok(());
    }
    let session = read_session(&paths.session)?;
    let record = paths.session_record(session.id);
    if record.exists() {
        let existing = read_session(&record)?;
        if existing != session {
            return Err(AppError::Lifecycle(format!(
                "current session {} disagrees with its durable session record; run `relay doctor`",
                session.id
            )));
        }
        return Ok(());
    }
    fs::create_dir_all(&paths.sessions).map_err(|source| AppError::Io {
        path: paths.sessions.clone(),
        source,
    })?;
    let mut operation = Operation::begin(
        paths,
        "history.materialize",
        Some(session.id),
        vec![(
            "session_record",
            format!(".relay/sessions/{}.json", session.id),
        )],
    )?;
    storage::write_json_atomic(&record, &session)?;
    operation.advance("session_record")?;
    operation.complete()
}

struct Operation {
    path: PathBuf,
    marker: OperationMarker,
}

impl Operation {
    fn begin(
        paths: &Paths,
        command: &str,
        session_id: Option<Uuid>,
        effects: Vec<(&str, String)>,
    ) -> Result<Self, AppError> {
        fs::create_dir_all(&paths.operations).map_err(|source| AppError::Io {
            path: paths.operations.clone(),
            source,
        })?;
        let marker = OperationMarker {
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            command: command.into(),
            session_id,
            started_at: Utc::now(),
            effects: effects
                .into_iter()
                .map(|(kind, path)| OperationEffect {
                    kind: kind.into(),
                    path,
                    completed: false,
                })
                .collect(),
        };
        let path = paths.operations.join(format!("{}.json", marker.id));
        storage::write_json_atomic(&path, &marker)?;
        Ok(Self { path, marker })
    }

    fn advance(&mut self, kind: &str) -> Result<(), AppError> {
        let effect = self
            .marker
            .effects
            .iter_mut()
            .find(|effect| effect.kind == kind && !effect.completed)
            .ok_or_else(|| {
                AppError::Lifecycle(format!("operation has no pending {kind} effect"))
            })?;
        effect.completed = true;
        storage::write_json_atomic(&self.path, &self.marker)?;
        #[cfg(debug_assertions)]
        if std::env::var("RELAY_TEST_FAIL_AFTER").as_deref() == Ok(kind) {
            return Err(AppError::InjectedFault(kind.into()));
        }
        Ok(())
    }

    fn complete(self) -> Result<(), AppError> {
        storage::remove_file_synced(&self.path)?;
        Ok(())
    }
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
    for path in [
        &paths.checkpoints,
        &paths.handoffs,
        &paths.sessions,
        &paths.operations,
    ] {
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
        &paths.sessions,
        &paths.operations,
        &paths.lock,
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
    sessions: PathBuf,
    operations: PathBuf,
    lock: PathBuf,
}

impl Paths {
    fn new(project: &Path) -> Self {
        let relay = project.join(".relay");
        Self {
            session: relay.join("session.json"),
            events: relay.join("events.jsonl"),
            checkpoints: relay.join("checkpoints"),
            handoffs: relay.join("handoffs"),
            sessions: relay.join("sessions"),
            operations: relay.join("operations"),
            lock: relay.join("lock"),
            relay,
        }
    }

    fn session_record(&self, id: Uuid) -> PathBuf {
        self.sessions.join(format!("{id}.json"))
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
