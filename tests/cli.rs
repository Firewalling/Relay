use relay::model::{Event, Session, SessionState};
use std::{
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

fn relay(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(args)
        .current_dir(project)
        .output()
        .expect("run relay binary")
}

fn relay_with_env(project: &Path, args: &[&str], key: &str, value: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(args)
        .env(key, value)
        .current_dir(project)
        .output()
        .expect("run relay binary")
}

fn relay_with_stdin(project: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(args)
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
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

#[test]
fn completion_commands_emit_current_cli_without_project_state() {
    for shell in ["bash", "zsh", "fish"] {
        let directory = tempfile::tempdir().unwrap();
        let output = relay(directory.path(), &["completions", shell]);

        assert_success(&output);
        assert!(output.stderr.is_empty());
        let script = stdout(&output);
        assert!(!script.is_empty());
        for command in ["start", "doctor", "context", "completions"] {
            assert!(script.contains(command), "{shell} omitted {command}");
        }
        assert!(!directory.path().join(".relay").exists());
    }
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

fn git_fingerprint(project: &Path) -> (String, String, String, Vec<String>, String, Vec<u8>) {
    (
        git_ok(project, ["rev-parse", "HEAD"]),
        git_ok(project, ["write-tree"]),
        git_ok(project, ["diff", "--", "."]),
        user_status(project),
        git_ok(project, ["stash", "list", "--format=%H"]),
        fs::read(project.join("tracked file.txt")).unwrap(),
    )
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

#[test]
fn nested_discovery_and_monorepo_git_status_are_project_bounded() {
    let directory = init_repository();
    let root = directory.path();
    let project = root.join("packages/relay-app");
    let nested = project.join("src/deep");
    let sibling = root.join("packages/sibling");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    fs::write(sibling.join("dirty.txt"), "sibling\n").unwrap();

    assert_success(&relay(&project, &["start", "Bound scope"]));
    let nested_status = relay(&nested, &["status"]);
    assert_success(&nested_status);
    assert!(stdout(&nested_status).contains("Goal: Bound scope"));
    assert!(stdout(&nested_status).contains("Git clean: yes"));

    fs::write(project.join("local.txt"), "local\n").unwrap();
    let checkpoint = relay(&nested, &["checkpoint", "scoped"]);
    assert_success(&checkpoint);
    assert!(stdout(&checkpoint).contains("local.txt"));
    assert!(!stdout(&checkpoint).contains("sibling"));

    let current = relay(&nested, &["--scope", "current", "status"]);
    assert!(!current.status.success());
    assert!(stderr(&current).contains("no Relay session"));
}

#[test]
fn lock_contention_is_nonblocking_and_owner_metadata_is_actionable() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["start", "lock owner"])
        .env("RELAY_TEST_HOLD_LOCK_MS", "1200")
        .current_dir(directory.path())
        .spawn()
        .unwrap();
    let lock_path = directory.path().join(".relay/lock");
    for _ in 0..100 {
        if lock_path.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let contender = relay(directory.path(), &["start", "contender"]);
    assert!(!contender.status.success());
    let error = stderr(&contender);
    assert!(error.contains("busy"));
    assert!(error.contains("PID"));
    assert!(error.contains("retry"));
    assert!(owner.wait().unwrap().success());

    assert_success(&relay(directory.path(), &["finish"]));
    assert_success(&relay(directory.path(), &["start", "released normally"]));
}

#[test]
fn injected_fault_leaves_durable_effect_and_operation_marker() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "fault testing"]));

    let failed = relay_with_env(
        directory.path(),
        &["checkpoint", "interrupted"],
        "RELAY_TEST_FAIL_AFTER",
        "artifact",
    );
    assert!(!failed.status.success());
    assert!(stderr(&failed).contains("injected interruption"));
    assert_eq!(
        fs::read_dir(directory.path().join(".relay/checkpoints"))
            .unwrap()
            .count(),
        1
    );
    let marker = only_file(directory.path().join(".relay/operations"));
    let marker: serde_json::Value = serde_json::from_slice(&fs::read(marker).unwrap()).unwrap();
    assert_eq!(marker["command"], "checkpoint");
    assert_eq!(marker["effects"][0]["completed"], true);
    assert_eq!(marker["effects"][1]["completed"], false);

    // Returned errors release the kernel lock even though diagnostic intent remains.
    assert_success(&relay(directory.path(), &["checkpoint", "after error"]));
}

#[test]
fn sessions_remain_addressable_and_resume_creates_lineage() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "first goal"]));
    let first = read_session(directory.path());
    assert_success(&relay(directory.path(), &["finish"]));
    let source_bytes = fs::read(
        directory
            .path()
            .join(format!(".relay/sessions/{}.json", first.id)),
    )
    .unwrap();

    let resumed = relay(directory.path(), &["resume", &first.id.to_string()[..8]]);
    assert_success(&resumed);
    let second = read_session(directory.path());
    assert_ne!(second.id, first.id);
    assert_eq!(second.resumed_from, Some(first.id));
    assert_eq!(second.goal, first.goal);
    assert_eq!(
        fs::read(
            directory
                .path()
                .join(format!(".relay/sessions/{}.json", first.id))
        )
        .unwrap(),
        source_bytes
    );

    let listed = relay(directory.path(), &["list"]);
    assert_success(&listed);
    assert!(stdout(&listed).contains(&first.id.to_string()));
    assert!(stdout(&listed).contains(&second.id.to_string()));
    let shown = relay(directory.path(), &["show", &first.id.to_string()[..8]]);
    assert_success(&shown);
    assert!(stdout(&shown).contains("State: completed"));
    let history = relay(
        directory.path(),
        &[
            "history",
            "--session",
            &second.id.to_string()[..8],
            "--limit",
            "1",
        ],
    );
    assert_success(&history);
    assert!(stdout(&history).contains("session.resumed"));

    let invalid = relay(directory.path(), &["resume", &first.id.to_string()]);
    assert!(!invalid.status.success());
    assert!(stderr(&invalid).contains("active Relay session already exists"));
}

#[test]
fn valid_legacy_session_is_read_only_until_locked_mutation_materializes_it() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "legacy"]));
    let session = read_session(directory.path());
    fs::remove_file(
        directory
            .path()
            .join(format!(".relay/sessions/{}.json", session.id)),
    )
    .unwrap();
    let pointer_before = fs::read(directory.path().join(".relay/session.json")).unwrap();

    assert_success(&relay(directory.path(), &["list"]));
    assert!(
        !directory
            .path()
            .join(format!(".relay/sessions/{}.json", session.id))
            .exists()
    );
    assert_success(&relay(directory.path(), &["checkpoint", "materialize"]));
    assert!(
        directory
            .path()
            .join(format!(".relay/sessions/{}.json", session.id))
            .exists()
    );
    assert_eq!(
        fs::read(directory.path().join(".relay/session.json")).unwrap(),
        pointer_before
    );
}

#[test]
fn ambiguous_session_prefix_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "seed"]));
    let template = read_session(directory.path());
    assert_success(&relay(directory.path(), &["finish"]));
    let sessions = directory.path().join(".relay/sessions");
    for id in [
        "aaaaaaaa-0000-4000-8000-000000000001",
        "aaaaaaaa-0000-4000-8000-000000000002",
    ] {
        let mut session = template.clone();
        session.id = id.parse().unwrap();
        session.state = SessionState::Completed;
        session.completed_at = Some(session.started_at);
        session.final_git = Some(session.initial_git.clone());
        fs::write(
            sessions.join(format!("{id}.json")),
            serde_json::to_vec_pretty(&session).unwrap(),
        )
        .unwrap();
    }
    let output = relay(directory.path(), &["show", "aaaaaaaa"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("ambiguous"));
}

#[test]
fn every_lifecycle_and_history_command_preserves_git_state_and_stashes() {
    let directory = init_repository();
    let project = directory.path();
    fs::write(project.join("stashed.txt"), "stash\n").unwrap();
    assert!(
        git(
            project,
            ["stash", "push", "--include-untracked", "-m", "keep"]
        )
        .status
        .success()
    );
    fs::write(project.join("tracked file.txt"), "unstaged\n").unwrap();
    fs::write(project.join("untracked.txt"), "untracked\n").unwrap();
    let before = git_fingerprint(project);

    let commands = [
        vec!["start", "immutable git"],
        vec!["status"],
        vec!["checkpoint", "snapshot"],
        vec!["handoff", "codex"],
        vec!["list"],
        vec!["history", "--limit", "2"],
        vec!["finish"],
    ];
    for args in commands {
        let output = relay(project, &args);
        assert_success(&output);
        assert_eq!(
            git_fingerprint(project),
            before,
            "Git changed after {args:?}"
        );
    }
    let completed = read_session(project);
    let completed_id = completed.id.to_string();
    for args in [
        vec!["show", completed_id.as_str()],
        vec!["resume", completed_id.as_str()],
    ] {
        let output = relay(project, &args);
        assert_success(&output);
        assert_eq!(
            git_fingerprint(project),
            before,
            "Git changed after {args:?}"
        );
    }
}

#[test]
fn abandoned_process_releases_kernel_lock_and_metadata_is_refreshed() {
    let directory = tempfile::tempdir().unwrap();
    let mut owner = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(["start", "abandoned"])
        .env("RELAY_TEST_HOLD_LOCK_MS", "10000")
        .current_dir(directory.path())
        .spawn()
        .unwrap();
    let lock = directory.path().join(".relay/lock");
    for _ in 0..100 {
        if lock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    owner.kill().unwrap();
    owner.wait().unwrap();

    let recovered = relay(directory.path(), &["start", "recovered"]);
    assert_success(&recovered);
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(lock).unwrap()).unwrap();
    assert_ne!(metadata["pid"], serde_json::json!(owner.id()));
    assert_eq!(metadata["command"], "start");
}

#[test]
fn faults_after_event_and_session_record_leave_precise_progress() {
    for fault in ["event", "session_record"] {
        let directory = tempfile::tempdir().unwrap();
        assert_success(&relay(directory.path(), &["start", "handoff fault"]));
        let failed = relay_with_env(
            directory.path(),
            &["handoff", "codex"],
            "RELAY_TEST_FAIL_AFTER",
            fault,
        );
        assert!(!failed.status.success());
        let marker = only_file(directory.path().join(".relay/operations"));
        let marker: serde_json::Value = serde_json::from_slice(&fs::read(marker).unwrap()).unwrap();
        let effects = marker["effects"].as_array().unwrap();
        assert!(
            effects
                .iter()
                .find(|effect| effect["kind"] == fault)
                .unwrap()["completed"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(
            read_events(directory.path())
                .iter()
                .filter(|event| event.event_type == "handoff.generated")
                .count(),
            1
        );
        if fault == "session_record" {
            let session = read_session(directory.path());
            let durable: Session = serde_json::from_slice(
                &fs::read(
                    directory
                        .path()
                        .join(format!(".relay/sessions/{}.json", session.id)),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(durable.last_agent.as_deref(), Some("codex"));
            assert_eq!(session.last_agent, None);
        }
    }
}

#[test]
fn list_recovers_older_supported_legacy_session_from_events() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "older"]));
    let older = read_session(directory.path());
    assert_success(&relay(directory.path(), &["finish"]));
    assert_success(&relay(directory.path(), &["start", "newer"]));
    fs::remove_file(
        directory
            .path()
            .join(format!(".relay/sessions/{}.json", older.id)),
    )
    .unwrap();

    let output = relay(directory.path(), &["list"]);
    assert_success(&output);
    assert!(stdout(&output).contains(&older.id.to_string()));
    assert!(stdout(&output).contains("older"));
}

#[test]
fn json_envelopes_are_single_typed_documents_and_failures_stay_on_stderr() {
    let directory = tempfile::tempdir().unwrap();
    for args in [
        vec!["--json", "start", "json goal"],
        vec!["--json", "status"],
        vec!["--json", "checkpoint", "typed"],
        vec!["--json", "handoff", "codex"],
        vec!["--json", "list"],
        vec!["--json", "history", "--limit", "2"],
        vec!["--json", "finish"],
    ] {
        let output = relay(directory.path(), &args);
        assert_success(&output);
        assert!(output.stderr.is_empty());
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["output_schema_version"], 1);
        assert_eq!(document["ok"], true);
        assert!(document["data"].is_object());
        assert!(
            !String::from_utf8_lossy(&output.stdout)
                .contains(directory.path().to_string_lossy().as_ref())
        );
    }
    let failed = relay(directory.path(), &["--json", "finish"]);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&failed.stderr).unwrap();
    assert_eq!(document["ok"], false);
    assert_eq!(document["error"]["code"], "lifecycle_error");
}

#[test]
fn doctor_detects_damage_without_changing_any_bytes_and_refuses_unsafe_repair() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "diagnose"]));
    let events = directory.path().join(".relay/events.jsonl");
    fs::write(&events, b"{\"partial\":true}").unwrap();
    let before = fs::read(&events).unwrap();

    let diagnosed = relay(directory.path(), &["--json", "doctor"]);
    assert_success(&diagnosed);
    let document: serde_json::Value = serde_json::from_slice(&diagnosed.stdout).unwrap();
    assert_eq!(document["data"]["healthy"], false);
    assert!(
        document["data"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "events_partial_line")
    );
    assert_eq!(fs::read(&events).unwrap(), before);

    let repair = relay(directory.path(), &["doctor", "--repair"]);
    assert!(!repair.status.success());
    assert!(stderr(&repair).contains("refused repair"));
    assert_eq!(fs::read(&events).unwrap(), before);
}

#[test]
fn doctor_repairs_only_a_provably_complete_stale_marker() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "repair marker"]));
    let session = read_session(directory.path());
    let marker_id = uuid::Uuid::new_v4();
    let marker = serde_json::json!({
        "schema_version": 1,
        "id": marker_id,
        "command": "start",
        "session_id": session.id,
        "started_at": session.started_at,
        "effects": [
            {"kind":"session_record","path":format!(".relay/sessions/{}.json", session.id),"completed":true},
            {"kind":"current_session","path":".relay/session.json","completed":true},
            {"kind":"event","path":".relay/events.jsonl","completed":true}
        ]
    });
    let marker_path = directory
        .path()
        .join(format!(".relay/operations/{marker_id}.json"));
    fs::write(&marker_path, serde_json::to_vec_pretty(&marker).unwrap()).unwrap();

    let repaired = relay(directory.path(), &["--json", "doctor", "--repair"]);
    assert_success(&repaired);
    assert!(!marker_path.exists());
    let document: serde_json::Value = serde_json::from_slice(&repaired.stdout).unwrap();
    assert_eq!(document["data"]["repaired"].as_array().unwrap().len(), 1);
    assert!(
        read_events(directory.path())
            .iter()
            .any(|event| event.event_type == "doctor.repaired")
    );
}

#[test]
fn categorized_notes_support_argument_stdin_editor_and_generated_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "notes"]));
    assert_success(&relay(
        directory.path(),
        &["note", "decision", "Keep JSONL"],
    ));
    assert_success(&relay_with_stdin(
        directory.path(),
        &["note", "blocker", "--stdin"],
        b"Waiting on review\n",
    ));
    let editor = directory.path().join("editor.sh");
    fs::write(&editor, "#!/bin/sh\nprintf 'Implemented parser' > \"$1\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let edited = relay_with_env(
        directory.path(),
        &["note", "progress", "--editor"],
        "VISUAL",
        editor.to_str().unwrap(),
    );
    assert_success(&edited);
    let conflict = relay(directory.path(), &["note", "decision", "x", "--stdin"]);
    assert!(!conflict.status.success());
    let blank = relay(directory.path(), &["note", "decision", "   "]);
    assert!(!blank.status.success());

    let handoff = relay(directory.path(), &["handoff", "codex"]);
    assert_success(&handoff);
    assert!(stdout(&handoff).contains("Keep JSONL"));
    assert!(stdout(&handoff).contains("Waiting on review"));
    assert!(stdout(&handoff).contains("Implemented parser"));
    assert_success(&relay(directory.path(), &["finish"]));
    let summary = fs::read_to_string(directory.path().join(format!(
        ".relay/summary-{}.md",
        read_session(directory.path()).id
    )))
    .unwrap();
    assert!(summary.contains("Keep JSONL"));
    assert!(summary.contains("Waiting on review"));
}

#[test]
fn context_filters_deterministically_respects_item_budget_and_preserves_privacy() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "bounded context"]));
    assert_success(&relay(
        directory.path(),
        &["note", "progress", "private progress"],
    ));
    assert_success(&relay(
        directory.path(),
        &["note", "decision", "chosen format"],
    ));
    assert_success(&relay(
        directory.path(),
        &["note", "next_action", "write tests"],
    ));
    assert_success(&relay(directory.path(), &["checkpoint", "context point"]));

    let args = [
        "context",
        "--category",
        "decision",
        "--category",
        "next_action",
        "--kind",
        "note.recorded",
        "--notes",
        "2",
        "--events",
        "1",
        "--checkpoints",
        "1",
        "--max-chars",
        "700",
    ];
    let first = relay(directory.path(), &args);
    let second = relay(directory.path(), &args);
    assert_success(&first);
    assert_eq!(first.stdout, second.stdout);
    let text = stdout(&first);
    assert!(text.chars().count() <= 700);
    assert!(text.contains("chosen format"));
    assert!(text.contains("write tests"));
    assert!(!text.contains("private progress"));
    assert!(!text.contains(directory.path().to_string_lossy().as_ref()));

    let json_output = relay(
        directory.path(),
        &[
            "--json",
            "context",
            "--category",
            "decision",
            "--notes",
            "1",
        ],
    );
    assert_success(&json_output);
    let document: serde_json::Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(document["data"]["notes"][0]["category"], "decision");
    assert!(
        !String::from_utf8_lossy(&json_output.stdout)
            .contains(directory.path().to_string_lossy().as_ref())
    );
}

#[test]
fn context_limits_keep_latest_items_in_recorded_order() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "recent context"]));
    assert_success(&relay(
        directory.path(),
        &["note", "progress", "oldest note"],
    ));
    for index in 0..7 {
        let message = format!("checkpoint {index}");
        assert_success(&relay(directory.path(), &["checkpoint", &message]));
    }
    assert_success(&relay(
        directory.path(),
        &["note", "progress", "middle note"],
    ));
    assert_success(&relay(
        directory.path(),
        &["note", "progress", "newest note"],
    ));

    let json_output = relay(
        directory.path(),
        &[
            "--json",
            "context",
            "--notes",
            "2",
            "--events",
            "2",
            "--checkpoints",
            "6",
        ],
    );
    assert_success(&json_output);
    let document: serde_json::Value = serde_json::from_slice(&json_output.stdout).unwrap();
    let data = &document["data"];
    assert_eq!(data["notes"].as_array().unwrap().len(), 2);
    assert_eq!(data["notes"][0]["text"], "middle note");
    assert_eq!(data["notes"][1]["text"], "newest note");
    assert_eq!(data["events"].as_array().unwrap().len(), 2);
    assert_eq!(data["events"][0]["payload"]["text"], "middle note");
    assert_eq!(data["events"][1]["payload"]["text"], "newest note");
    assert_eq!(data["checkpoints"].as_array().unwrap().len(), 6);
    assert_eq!(data["checkpoints"][0]["message"], "checkpoint 1");
    assert_eq!(data["checkpoints"][5]["message"], "checkpoint 6");
    assert_eq!(data["omitted"]["notes"], 1);
    assert_eq!(data["omitted"]["events"], 9);
    assert_eq!(data["omitted"]["checkpoints"], 1);

    let human_output = relay(
        directory.path(),
        &[
            "context",
            "--notes",
            "1",
            "--events",
            "0",
            "--checkpoints",
            "1",
        ],
    );
    assert_success(&human_output);
    let text = stdout(&human_output);
    assert!(text.contains("newest note"));
    assert!(text.contains("checkpoint 6"));
    assert!(!text.contains("oldest note"));
    assert!(!text.contains("middle note"));
    assert!(!text.contains("checkpoint 0"));
}
