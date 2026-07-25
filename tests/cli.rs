use relay::model::{Event, Session, SessionState};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn relay(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(args)
        .current_dir(project)
        .output()
        .expect("run relay binary")
}

fn git<I, S>(project: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .args(args)
        .current_dir(project)
        .output()
        .expect("run git")
}

fn git_ok<I, S>(project: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git(project, args);
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("Git output is UTF-8")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "relay failed: {}", stderr(output));
}

fn init_repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("create temporary repository");
    git_ok(
        directory.path(),
        ["init", "--quiet", "--initial-branch=main"],
    );
    git_ok(directory.path(), ["config", "user.name", "Relay Tests"]);
    git_ok(
        directory.path(),
        ["config", "user.email", "relay-tests@example.invalid"],
    );
    fs::write(directory.path().join("tracked file.txt"), "original\n").unwrap();
    git_ok(directory.path(), ["add", "tracked file.txt"]);
    git_ok(directory.path(), ["commit", "--quiet", "-m", "initial"]);
    directory
}

fn read_session(project: &Path) -> Session {
    serde_json::from_slice(&fs::read(project.join(".relay/session.json")).unwrap()).unwrap()
}

fn read_events(project: &Path) -> Vec<Event> {
    fs::read_to_string(project.join(".relay/events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn only_file(directory: PathBuf) -> PathBuf {
    let files = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    files.into_iter().next().unwrap()
}

fn user_status(project: &Path) -> Vec<String> {
    git_ok(
        project,
        [
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--",
            ".",
        ],
    )
    .lines()
    .filter(|line| {
        !line
            .get(3..)
            .is_some_and(|path| path.starts_with(".relay/"))
    })
    .map(str::to_owned)
    .collect()
}

#[test]
fn full_git_lifecycle_preserves_repository_and_records_structured_artifacts() {
    let directory = init_repository();
    let project = directory.path();
    let start = relay(project, &["start", "Ship the integration milestone"]);
    assert_success(&start);
    assert!(stdout(&start).contains("Git: branch main"));

    fs::write(project.join("tracked file.txt"), "working change\n").unwrap();
    fs::write(project.join("new file.txt"), "untracked\n").unwrap();
    let head_before = git_ok(project, ["rev-parse", "HEAD"]);
    let index_before = git_ok(project, ["write-tree"]);
    let diff_before = git_ok(project, ["diff", "--", "."]);
    let status_before = user_status(project);
    assert!(
        git_ok(
            project,
            [
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--",
                ".relay"
            ]
        )
        .contains("?? .relay/")
    );

    let checkpoint = relay(project, &["checkpoint", "Captured dirty work"]);
    assert_success(&checkpoint);
    assert!(stdout(&checkpoint).contains("tracked file.txt"));
    assert!(stdout(&checkpoint).contains("new file.txt"));
    assert_eq!(git_ok(project, ["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_ok(project, ["write-tree"]), index_before);
    assert_eq!(git_ok(project, ["diff", "--", "."]), diff_before);
    assert_eq!(user_status(project), status_before);

    let status = relay(project, &["status"]);
    assert_success(&status);
    let status_text = stdout(&status);
    assert!(status_text.contains("State: active"));
    assert!(status_text.contains("Git branch: main"));
    assert!(status_text.contains("Git clean: no"));
    assert!(status_text.contains("Events: 2"));
    assert!(status_text.contains("Captured dirty work"));

    let handoff = relay(
        project,
        &[
            "handoff",
            "codex",
            "--completed-work",
            "Added black-box lifecycle coverage",
            "--decision",
            "Use the built binary",
            "--failure",
            "No blockers",
            "--next-action",
            "Review the saved summary",
        ],
    );
    assert_success(&handoff);
    let handoff_text = stdout(&handoff);
    for section in [
        "## Goal",
        "## Completed work",
        "## Important decisions",
        "## Failures or blocked work",
        "## Current Git state",
        "## Changed files",
        "## Relevant checkpoints",
        "## Suggested next action",
        "## Target agent",
    ] {
        assert!(handoff_text.contains(section), "missing {section}");
    }
    for note in [
        "Added black-box lifecycle coverage",
        "Use the built binary",
        "No blockers",
        "Review the saved summary",
        "Captured dirty work",
        "codex",
    ] {
        assert!(handoff_text.contains(note), "missing {note}");
    }
    assert!(!handoff_text.contains(project.to_string_lossy().as_ref()));
    assert_eq!(git_ok(project, ["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_ok(project, ["write-tree"]), index_before);
    assert_eq!(git_ok(project, ["diff", "--", "."]), diff_before);
    assert_eq!(user_status(project), status_before);

    let handoff_path = only_file(project.join(".relay/handoffs"));
    let saved_handoff = fs::read_to_string(handoff_path).unwrap();
    assert!(saved_handoff.contains("Added black-box lifecycle coverage"));
    assert!(!saved_handoff.contains(project.to_string_lossy().as_ref()));
    assert_eq!(read_session(project).last_agent.as_deref(), Some("codex"));

    let after_handoff = relay(project, &["status"]);
    assert_success(&after_handoff);
    assert!(stdout(&after_handoff).contains("Last agent: codex"));

    let finish = relay(project, &["finish"]);
    assert_success(&finish);
    assert!(stdout(&finish).contains("Summary: .relay/summary-"));
    let session = read_session(project);
    assert_eq!(session.state, SessionState::Completed);
    assert!(session.completed_at.is_some());
    let summary =
        fs::read_to_string(project.join(format!(".relay/summary-{}.md", session.id))).unwrap();
    assert!(summary.contains("Last agent: codex"));
    assert!(!summary.contains(project.to_string_lossy().as_ref()));

    let events = read_events(project);
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            "session.started",
            "checkpoint.created",
            "handoff.generated",
            "session.finished"
        ]
    );
    for event in &events {
        assert_eq!(event.schema_version, 1);
        assert_eq!(event.session_id, session.id);
        assert!(!event.id.is_nil());
        assert!(event.payload.is_object());
    }
    let handoff_event = &events[2];
    assert_eq!(handoff_event.payload["agent"], "codex");
    assert_eq!(
        handoff_event.payload["completed_work"][0],
        "Added black-box lifecycle coverage"
    );
    assert_eq!(
        handoff_event.payload["decisions"][0],
        "Use the built binary"
    );
    assert_eq!(handoff_event.payload["failures"][0], "No blockers");
    assert_eq!(
        handoff_event.payload["next_action"],
        "Review the saved summary"
    );
}

#[test]
fn lifecycle_works_outside_git() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    for args in [
        vec!["start", "Work without Git"],
        vec!["checkpoint", "Non-Git checkpoint"],
    ] {
        let output = relay(project, &args);
        assert_success(&output);
    }
    let status = relay(project, &["status"]);
    assert_success(&status);
    assert!(stdout(&status).contains("Git branch: unavailable"));
    assert!(stdout(&status).contains("Git clean: unavailable"));
    assert!(!read_session(project).initial_git.available);
    let finish = relay(project, &["finish"]);
    assert_success(&finish);
    assert_eq!(read_session(project).state, SessionState::Completed);
}

#[test]
fn duplicate_start_preserves_session_and_events() {
    let directory = tempfile::tempdir().unwrap();
    let first = relay(directory.path(), &["start", "Original goal"]);
    assert_success(&first);
    let session_before = fs::read(directory.path().join(".relay/session.json")).unwrap();
    let events_before = fs::read(directory.path().join(".relay/events.jsonl")).unwrap();

    let duplicate = relay(directory.path(), &["start", "Replacement goal"]);
    assert!(!duplicate.status.success());
    assert!(stderr(&duplicate).contains("active Relay session already exists"));
    assert_eq!(
        fs::read(directory.path().join(".relay/session.json")).unwrap(),
        session_before
    );
    assert_eq!(
        fs::read(directory.path().join(".relay/events.jsonl")).unwrap(),
        events_before
    );
}

#[test]
fn malformed_session_is_not_replaced() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".relay")).unwrap();
    let malformed = b"{ this is not a session }\n";
    fs::write(directory.path().join(".relay/session.json"), malformed).unwrap();

    let output = relay(directory.path(), &["start", "Must not overwrite"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("corrupt JSON"));
    assert_eq!(
        fs::read(directory.path().join(".relay/session.json")).unwrap(),
        malformed
    );
    assert!(!directory.path().join(".relay/events.jsonl").exists());
}

#[test]
fn invalid_finish_is_rejected_without_creating_or_replacing_state() {
    let directory = tempfile::tempdir().unwrap();
    let missing = relay(directory.path(), &["finish"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("no Relay session exists"));
    assert!(!directory.path().join(".relay").exists());

    assert_success(&relay(directory.path(), &["start", "Finish once"]));
    assert_success(&relay(directory.path(), &["finish"]));
    let session_before = fs::read(directory.path().join(".relay/session.json")).unwrap();
    let events_before = fs::read(directory.path().join(".relay/events.jsonl")).unwrap();
    let duplicate = relay(directory.path(), &["finish"]);
    assert!(!duplicate.status.success());
    assert!(stderr(&duplicate).contains("already finished"));
    assert_eq!(
        fs::read(directory.path().join(".relay/session.json")).unwrap(),
        session_before
    );
    assert_eq!(
        fs::read(directory.path().join(".relay/events.jsonl")).unwrap(),
        events_before
    );
}

#[cfg(unix)]
#[test]
fn relay_directory_symlink_cannot_write_outside_project() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), project.path().join(".relay")).unwrap();

    let output = relay(project.path(), &["start", "Stay contained"]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("refusing to use symlinked Relay state path"));
    assert!(stderr(&output).contains("replace it with a real file or directory"));
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
}
