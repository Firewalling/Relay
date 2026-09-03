use relay::{
    handoff::MAX_HANDOFF_GOAL_BYTES,
    model::{Checkpoint, Event, ProtocolManifest, Session, SessionState},
};
use serde_json::json;
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
        for command in ["start", "doctor", "context", "completions", "protocol"] {
            assert!(script.contains(command), "{shell} omitted {command}");
        }
        assert!(!directory.path().join(".relay").exists());
    }
}

#[test]
fn protocol_manifest_is_versioned_project_independent_and_stable() {
    const HUMAN_OUTPUT: &str = concat!(
        "Relay orchestration protocol: 1\n",
        "Transport: direct argv, one JSON document per process\n",
        "Output schema: 1\n",
        "State owner: Rust (schema 1)\n",
        "Built-in run coordinator: Rust (bounded parallel DAG)\n",
        "Go one-child supervisor: implemented\n",
        "Manifest-v1 external orchestrator (owner Go): not implemented\n",
        "Lua workflow: not implemented\n",
        "Operations: status, context, note, checkpoint, handoff, finish\n",
    );

    let expected_data: serde_json::Value =
        serde_json::from_str(include_str!("../protocol/manifest-v1.json")).unwrap();
    assert_eq!(expected_data["relay_version"], "0.2.0");
    let mut expected_runtime_data = expected_data.clone();
    expected_runtime_data["relay_version"] = env!("CARGO_PKG_VERSION").into();
    let directory = tempfile::tempdir().unwrap();
    let sentinel = directory.path().join("sentinel");
    fs::write(&sentinel, b"unchanged\n").unwrap();
    let mut entries_before = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries_before.sort();

    let human = relay(directory.path(), &["protocol"]);
    assert_success(&human);
    assert!(human.stderr.is_empty());
    assert_eq!(human.stdout, HUMAN_OUTPUT.as_bytes());

    let json_before = relay_with_env(
        directory.path(),
        &["--json", "--scope", "repository", "protocol"],
        "GIT_CEILING_DIRECTORIES",
        directory.path().to_str().unwrap(),
    );
    assert_success(&json_before);
    assert!(json_before.stderr.is_empty());
    let json_repeat = relay_with_env(
        directory.path(),
        &["--json", "--scope", "repository", "protocol"],
        "GIT_CEILING_DIRECTORIES",
        directory.path().to_str().unwrap(),
    );
    assert_success(&json_repeat);
    assert!(json_repeat.stderr.is_empty());
    assert_eq!(json_repeat.stdout, json_before.stdout);

    let json_after = relay_with_env(
        directory.path(),
        &["protocol", "--scope", "repository", "--json"],
        "GIT_CEILING_DIRECTORIES",
        directory.path().to_str().unwrap(),
    );
    assert_success(&json_after);
    assert!(json_after.stderr.is_empty());
    assert_eq!(json_after.stdout, json_before.stdout);

    let document: serde_json::Value = serde_json::from_slice(&json_before.stdout).unwrap();
    assert_eq!(
        document,
        serde_json::json!({
            "command": "protocol",
            "data": expected_runtime_data.clone(),
            "ok": true,
            "output_schema_version": 1
        })
    );
    let manifest: ProtocolManifest =
        serde_json::from_value(document["data"].clone()).expect("deserialize protocol manifest");
    assert_eq!(
        serde_json::to_value(manifest).expect("serialize protocol manifest"),
        expected_runtime_data
    );
    let mut unknown_data = expected_runtime_data.clone();
    unknown_data
        .as_object_mut()
        .unwrap()
        .insert("unknown".into(), serde_json::Value::Null);
    assert!(serde_json::from_value::<ProtocolManifest>(unknown_data).is_err());

    let mut entries_after = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries_after.sort();
    assert_eq!(entries_after, entries_before);
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged\n");
    assert!(!directory.path().join(".relay").exists());

    #[cfg(unix)]
    {
        let symlink_project = tempfile::tempdir().unwrap();
        let symlink_target = tempfile::tempdir().unwrap();
        let target_sentinel = symlink_target.path().join("sentinel");
        fs::write(&target_sentinel, b"target unchanged\n").unwrap();
        std::os::unix::fs::symlink(symlink_target.path(), symlink_project.path().join(".relay"))
            .unwrap();

        let output = relay_with_env(
            symlink_project.path(),
            &["protocol", "--json", "--scope", "repository"],
            "GIT_CEILING_DIRECTORIES",
            symlink_project.path().to_str().unwrap(),
        );
        assert_success(&output);
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, json_before.stdout);
        assert_eq!(
            fs::read_link(symlink_project.path().join(".relay")).unwrap(),
            symlink_target.path()
        );
        assert_eq!(fs::read(&target_sentinel).unwrap(), b"target unchanged\n");
        assert_eq!(fs::read_dir(symlink_project.path()).unwrap().count(), 1);
        assert_eq!(fs::read_dir(symlink_target.path()).unwrap().count(), 1);
    }
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "relay failed: {}", stderr(output));
}

fn json_success_data(output: &Output) -> serde_json::Value {
    assert_success(output);
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_json_object_has_required_keys(
        &document,
        &["command", "data", "ok", "output_schema_version"],
    );
    assert_eq!(document["output_schema_version"], 1);
    assert_eq!(document["ok"], true);
    assert!(document["data"].is_object());
    document["data"].clone()
}

fn assert_json_object_has_required_keys(value: &serde_json::Value, required: &[&str]) {
    let object = value.as_object().expect("JSON value is an object");
    for key in required {
        assert!(
            object.contains_key(*key),
            "JSON object is missing required key `{key}`"
        );
    }
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
fn handoff_fault_boundaries_remain_durable_and_conservative() {
    let effects = [
        "record",
        "artifact",
        "event",
        "session_record",
        "current_session",
    ];
    for (fault_index, fault) in effects.iter().enumerate() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path();
        assert_success(&relay(project, &["start", "handoff fault"]));
        let failed = relay_with_env(
            project,
            &["handoff", "codex"],
            "RELAY_TEST_FAIL_AFTER",
            fault,
        );
        assert!(!failed.status.success());

        let marker_path = only_file(project.join(".relay/operations"));
        let marker: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        let marker_effects = marker["effects"].as_array().unwrap();
        assert_eq!(marker_effects.len(), effects.len());
        for (index, effect) in marker_effects.iter().enumerate() {
            assert_eq!(effect["kind"], effects[index]);
            assert_eq!(effect["completed"], index <= fault_index);
        }

        let ledger = project.join(".relay/handoffs.jsonl");
        assert!(ledger.is_file());
        let ledger_before = fs::read(&ledger).unwrap();
        assert_eq!(
            ledger_before.iter().filter(|byte| **byte == b'\n').count(),
            1
        );
        assert_eq!(
            read_events(project)
                .iter()
                .filter(|event| event.event_type == "handoff.generated")
                .count(),
            usize::from(fault_index >= 2)
        );
        let artifact = fs::read_dir(project.join(".relay/handoffs"))
            .unwrap()
            .next()
            .map(|entry| entry.unwrap().path());
        assert_eq!(artifact.is_some(), fault_index >= 1);

        let session = read_session(project);
        let durable: Session = serde_json::from_slice(
            &fs::read(
                project
                    .join(".relay/sessions")
                    .join(format!("{}.json", session.id)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            durable.last_agent.as_deref(),
            (fault_index >= 3).then_some("codex")
        );
        assert_eq!(
            session.last_agent.as_deref(),
            (fault_index >= 4).then_some("codex")
        );

        let diagnosed = relay(project, &["--json", "doctor"]);
        assert_success(&diagnosed);
        let diagnosed: serde_json::Value = serde_json::from_slice(&diagnosed.stdout).unwrap();
        assert_eq!(diagnosed["data"]["healthy"], fault_index == 4);
        let expected_code = if fault_index == 4 {
            "completed_operation_marker"
        } else {
            "incomplete_operation"
        };
        assert!(
            diagnosed["data"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["code"] == expected_code)
        );

        let repair = relay(project, &["doctor", "--repair"]);
        if fault_index == 4 {
            assert_success(&repair);
            assert!(!marker_path.exists());
        } else {
            assert!(!repair.status.success());
            assert!(marker_path.exists());
        }
        assert_eq!(fs::read(&ledger).unwrap(), ledger_before);
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
fn mutation_results_do_not_rescan_corrupt_unrelated_session_records() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();

    assert_success(&relay(project, &["start", "completed historical work"]));
    assert_success(&relay(project, &["finish"]));
    let old_session = read_session(project);
    assert_eq!(old_session.state, SessionState::Completed);
    assert_success(&relay(project, &["start", "current typed results"]));
    let current_session = read_session(project);
    assert_eq!(current_session.state, SessionState::Active);
    assert_ne!(current_session.id, old_session.id);
    let current_record: Session = serde_json::from_slice(
        &fs::read(project.join(format!(".relay/sessions/{}.json", current_session.id))).unwrap(),
    )
    .unwrap();
    assert_eq!(current_record, current_session);

    let old_record = project.join(format!(".relay/sessions/{}.json", old_session.id));
    let corrupt_bytes = b"{ unrelated historical corruption\n";
    fs::write(&old_record, corrupt_bytes).unwrap();

    let note_text = "Typed results use the completed mutation";
    let note = relay(project, &["note", "progress", note_text]);
    assert_success(&note);
    assert!(note.stderr.is_empty());
    assert_eq!(
        stdout(&note).trim_end(),
        format!("Recorded progress note for session {}", current_session.id)
    );
    let note_event = read_events(project).pop().unwrap();
    assert_eq!(note_event.session_id, current_session.id);
    assert_eq!(note_event.event_type, "note.recorded");
    assert_eq!(
        note_event.payload,
        serde_json::json!({"category": "progress", "text": note_text})
    );
    assert_eq!(fs::read(&old_record).unwrap(), corrupt_bytes);

    let checkpoint_output = relay(project, &["--json", "checkpoint", "typed checkpoint"]);
    let checkpoint_data = json_success_data(&checkpoint_output);
    assert_json_object_has_required_keys(&checkpoint_data, &["checkpoint", "path", "session"]);
    let checkpoint_session: Session =
        serde_json::from_value(checkpoint_data["session"].clone()).unwrap();
    assert_eq!(checkpoint_session, current_session);
    let checkpoint: Checkpoint =
        serde_json::from_value(checkpoint_data["checkpoint"].clone()).unwrap();
    assert_eq!(checkpoint.session_id, current_session.id);
    assert_eq!(checkpoint.message.as_deref(), Some("typed checkpoint"));
    let checkpoint_path = checkpoint_data["path"].as_str().unwrap();
    assert!(Path::new(checkpoint_path).is_relative());
    let saved_checkpoint: Checkpoint =
        serde_json::from_slice(&fs::read(project.join(checkpoint_path)).unwrap()).unwrap();
    assert_eq!(saved_checkpoint, checkpoint);
    let checkpoint_event = read_events(project).pop().unwrap();
    assert_eq!(checkpoint_event.session_id, current_session.id);
    assert_eq!(checkpoint_event.event_type, "checkpoint.created");
    assert_eq!(
        checkpoint_event.payload["checkpoint"],
        checkpoint_data["checkpoint"]
    );
    assert_eq!(checkpoint_event.payload["path"], checkpoint_data["path"]);
    assert_eq!(fs::read(&old_record).unwrap(), corrupt_bytes);

    let handoff_output = relay(
        project,
        &[
            "--json",
            "handoff",
            "next-agent",
            "--completed-work",
            "Returned the durable handoff",
            "--next-action",
            "Finish the current session",
        ],
    );
    let handoff_data = json_success_data(&handoff_output);
    assert_json_object_has_required_keys(&handoff_data, &["markdown", "path", "session"]);
    let handed_off_session: Session =
        serde_json::from_value(handoff_data["session"].clone()).unwrap();
    assert_eq!(handed_off_session.id, current_session.id);
    assert_eq!(handed_off_session.last_agent.as_deref(), Some("next-agent"));
    assert_eq!(handed_off_session, read_session(project));
    let handoff_path = handoff_data["path"].as_str().unwrap();
    assert!(Path::new(handoff_path).is_relative());
    assert_eq!(
        fs::read(project.join(handoff_path)).unwrap(),
        handoff_data["markdown"].as_str().unwrap().as_bytes()
    );
    assert_eq!(fs::read(&old_record).unwrap(), corrupt_bytes);

    let finish_output = relay(project, &["--json", "finish"]);
    let finish_data = json_success_data(&finish_output);
    assert_json_object_has_required_keys(&finish_data, &["session", "summary_path"]);
    let finished_session: Session = serde_json::from_value(finish_data["session"].clone()).unwrap();
    assert_eq!(finished_session.id, current_session.id);
    assert_eq!(finished_session.state, SessionState::Completed);
    assert_eq!(finished_session, read_session(project));
    let summary_path = finish_data["summary_path"].as_str().unwrap();
    assert!(Path::new(summary_path).is_relative());
    assert!(project.join(summary_path).is_file());
    let finish_event = read_events(project).pop().unwrap();
    assert_eq!(finish_event.event_type, "session.finished");
    assert_eq!(finish_event.session_id, current_session.id);
    assert_eq!(
        finish_event.payload["summary_path"],
        finish_data["summary_path"]
    );
    assert_eq!(fs::read(&old_record).unwrap(), corrupt_bytes);
}

#[test]
fn parser_failures_honor_json_mode_without_mutating_project_state() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();

    let assert_json_argument_error = |args: &[&str], message_fragments: &[&str]| {
        let output = relay(project, args);

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr.iter().filter(|&&byte| byte == b'\n').count(),
            1,
            "JSON parser failure must be exactly one newline-terminated document"
        );
        assert_eq!(output.stderr.last(), Some(&b'\n'));

        let document: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_json_object_has_required_keys(
            &document,
            &["command", "error", "ok", "output_schema_version"],
        );
        assert_eq!(document.as_object().unwrap().len(), 4);
        assert_json_object_has_required_keys(&document["error"], &["code", "message"]);
        assert_eq!(document["error"].as_object().unwrap().len(), 2);
        assert_eq!(document["output_schema_version"], 1);
        assert_eq!(document["ok"], false);
        assert_eq!(document["command"], "cli");
        assert_eq!(document["error"]["code"], "argument_error");

        let message = document["error"]["message"].as_str().unwrap();
        assert!(!message.starts_with("error: "));
        for fragment in message_fragments {
            assert!(
                message.contains(*fragment),
                "argument error message omitted actionable fragment `{fragment}`: {message}"
            );
        }
        assert!(!project.join(".relay").exists());
    };

    assert_json_argument_error(
        &["--json", "--scope", "not-a-scope", "status"],
        &["invalid value", "not-a-scope", "--scope"],
    );
    assert_json_argument_error(
        &["status", "--json", "--scope", "not-a-scope"],
        &["invalid value", "not-a-scope", "--scope"],
    );
    assert_json_argument_error(
        &["--json", "not-a-command"],
        &["unrecognized subcommand", "not-a-command"],
    );
    assert_json_argument_error(
        &["completions", "--json"],
        &["required arguments", "<SHELL>"],
    );

    let human = relay(project, &["--scope", "not-a-scope", "status"]);
    assert_eq!(human.status.code(), Some(2));
    assert!(human.stdout.is_empty());
    let human_error = stderr(&human);
    assert!(human_error.starts_with("error: "));
    assert!(human_error.contains("invalid value"));
    assert!(human_error.contains("not-a-scope"));
    assert!(human_error.contains("--scope"));
    assert!(serde_json::from_slice::<serde_json::Value>(&human.stderr).is_err());
    assert!(!project.join(".relay").exists());

    let help = relay(project, &["--json", "--help"]);
    assert_success(&help);
    assert!(help.stderr.is_empty());
    let help_text = stdout(&help);
    assert!(help_text.contains("Usage: relay"));
    assert!(help_text.contains("--json"));
    assert!(serde_json::from_slice::<serde_json::Value>(&help.stdout).is_err());
    assert!(!project.join(".relay").exists());

    let after_delimiter = relay(project, &["protocol", "--", "--json"]);
    assert_eq!(after_delimiter.status.code(), Some(2));
    assert!(after_delimiter.stdout.is_empty());
    let after_delimiter_error = stderr(&after_delimiter);
    assert!(after_delimiter_error.starts_with("error: "));
    assert!(after_delimiter_error.contains("--json"));
    assert!(serde_json::from_slice::<serde_json::Value>(&after_delimiter.stderr).is_err());
    assert!(!project.join(".relay").exists());
    #[cfg(unix)]
    {
        use std::os::{fd::OwnedFd, unix::net::UnixStream};

        let closed_stdio = || {
            let (stream, peer) = UnixStream::pair().unwrap();
            drop(peer);
            Stdio::from(OwnedFd::from(stream))
        };

        let json_closed_stderr = Command::new(env!("CARGO_BIN_EXE_relay"))
            .args(["--json", "--scope", "not-a-scope", "status"])
            .current_dir(project)
            .stderr(closed_stdio())
            .output()
            .expect("run relay binary with closed stderr");
        assert_eq!(json_closed_stderr.status.code(), Some(2));
        assert!(json_closed_stderr.stdout.is_empty());
        assert!(!project.join(".relay").exists());

        let human_closed_stderr = Command::new(env!("CARGO_BIN_EXE_relay"))
            .args(["--scope", "not-a-scope", "status"])
            .current_dir(project)
            .stderr(closed_stdio())
            .output()
            .expect("run relay binary with closed stderr");
        assert_eq!(human_closed_stderr.status.code(), Some(2));
        assert!(human_closed_stderr.stdout.is_empty());

        let help_closed_stdout = Command::new(env!("CARGO_BIN_EXE_relay"))
            .arg("--help")
            .current_dir(project)
            .stdout(closed_stdio())
            .output()
            .expect("run relay binary with closed stdout");
        assert_eq!(help_closed_stdout.status.code(), Some(0));
        assert!(help_closed_stdout.stderr.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn successful_output_ignores_closed_stdout() {
    use std::os::{fd::OwnedFd, unix::net::UnixStream};

    let directory = tempfile::tempdir().unwrap();
    for args in [&["protocol"][..], &["--json", "protocol"][..]] {
        let (stream, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let output = Command::new(env!("CARGO_BIN_EXE_relay"))
            .args(args)
            .current_dir(directory.path())
            .stdout(Stdio::from(OwnedFd::from(stream)))
            .output()
            .expect("run relay binary with closed stdout");

        assert_eq!(output.status.code(), Some(0), "args: {args:?}");
        assert!(output.stderr.is_empty(), "args: {args:?}");
        assert!(!directory.path().join(".relay").exists());
    }
}

#[test]
fn json_envelopes_are_single_typed_documents_and_failures_stay_on_stderr() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();

    let started = relay(project, &["--json", "start", "json goal"]);
    let start_data = json_success_data(&started);
    assert_json_object_has_required_keys(
        &start_data,
        &["event_count", "git", "project", "session"],
    );
    let started_session: Session = serde_json::from_value(start_data["session"].clone()).unwrap();
    assert_eq!(started_session, read_session(project));
    assert_eq!(
        start_data["git"],
        serde_json::to_value(&started_session.initial_git).unwrap()
    );
    assert_eq!(start_data["event_count"], 1);
    assert_eq!(start_data["project"], ".");
    assert!(!String::from_utf8_lossy(&started.stdout).contains(project.to_string_lossy().as_ref()));

    let note = relay(project, &["--json", "note", "decision", "Keep typed data"]);
    let note_data = json_success_data(&note);
    assert_json_object_has_required_keys(&note_data, &["note", "session"]);
    let note_session: Session = serde_json::from_value(note_data["session"].clone()).unwrap();
    assert_eq!(note_session, read_session(project));
    assert_json_object_has_required_keys(&note_data["note"], &["category", "text"]);
    assert_eq!(
        note_data["note"],
        serde_json::json!({"category": "decision", "text": "Keep typed data"})
    );
    assert_eq!(
        read_events(project).last().unwrap().payload,
        note_data["note"]
    );

    for args in [
        vec!["--json", "status"],
        vec!["--json", "checkpoint", "typed"],
        vec!["--json", "handoff", "codex"],
        vec!["--json", "list"],
        vec!["--json", "history", "--limit", "2"],
    ] {
        let output = relay(project, &args);
        let data = json_success_data(&output);
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(project.to_string_lossy().as_ref())
        );
        assert!(data.is_object());
    }

    let finished = relay(project, &["--json", "finish"]);
    let finish_data = json_success_data(&finished);
    let completed_session: Session =
        serde_json::from_value(finish_data["session"].clone()).unwrap();
    assert_eq!(completed_session.state, SessionState::Completed);
    assert!(
        !String::from_utf8_lossy(&finished.stdout).contains(project.to_string_lossy().as_ref())
    );

    let completed_id = completed_session.id.to_string();
    let resumed = relay(project, &["--json", "resume", &completed_id]);
    let resume_data = json_success_data(&resumed);
    assert_json_object_has_required_keys(
        &resume_data,
        &["event_count", "git", "project", "session"],
    );
    let resumed_session: Session = serde_json::from_value(resume_data["session"].clone()).unwrap();
    assert_eq!(resumed_session, read_session(project));
    assert_eq!(resumed_session.resumed_from, Some(completed_session.id));
    assert_eq!(
        resume_data["git"],
        serde_json::to_value(&resumed_session.initial_git).unwrap()
    );
    assert_eq!(resume_data["event_count"], 1);
    assert_eq!(resume_data["project"], ".");
    assert!(!String::from_utf8_lossy(&resumed.stdout).contains(project.to_string_lossy().as_ref()));

    assert_success(&relay(project, &["finish"]));
    let failed = relay(project, &["--json", "finish"]);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&failed.stderr).unwrap();
    assert_json_object_has_required_keys(
        &document,
        &["command", "error", "ok", "output_schema_version"],
    );
    assert_json_object_has_required_keys(&document["error"], &["code", "message"]);
    assert_eq!(document["output_schema_version"], 1);
    assert_eq!(document["command"], "finish");
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
fn doctor_does_not_repair_empty_operation_marker() {
    let directory = tempfile::tempdir().unwrap();
    assert_success(&relay(directory.path(), &["start", "empty marker"]));
    let marker_path = directory.path().join(".relay/operations/empty.json");
    let marker = serde_json::json!({
        "schema_version": 1,
        "id": uuid::Uuid::new_v4(),
        "command": "history.materialize",
        "session_id": serde_json::Value::Null,
        "started_at": "2026-01-01T00:00:00Z",
        "effects": []
    });
    let bytes = serde_json::to_vec(&marker).unwrap();
    fs::write(&marker_path, &bytes).unwrap();

    let repair = relay(directory.path(), &["doctor", "--repair"]);
    assert!(!repair.status.success());
    assert!(stderr(&repair).contains("refused repair"));
    assert_eq!(fs::read(&marker_path).unwrap(), bytes);
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
fn context_renders_structured_handoff_details_without_duplicating_separate_items() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    let project_path = project.to_string_lossy().into_owned();
    let note_text = "Documented renderer progress";
    let checkpoint_message = "Captured renderer checkpoint";
    let target_agent = "context-specialist";
    let completed_work = "Rendered structured handoff details";
    let decision = format!("Keep artifacts under {project_path}");
    let redacted_decision = "Keep artifacts under [project root]";
    let failure = "Renderer omitted detail labels";
    let next_action = "Verify context presentation";

    assert_success(&relay(project, &["start", "structured context"]));
    assert_success(&relay(project, &["note", "progress", note_text]));
    assert_success(&relay(project, &["checkpoint", checkpoint_message]));
    assert_success(&relay(
        project,
        &[
            "handoff",
            target_agent,
            "--completed-work",
            completed_work,
            "--decision",
            decision.as_str(),
            "--failure",
            failure,
            "--next-action",
            next_action,
        ],
    ));

    let human_output = relay(
        project,
        &[
            "context",
            "--notes",
            "1",
            "--events",
            "4",
            "--checkpoints",
            "1",
            "--max-chars",
            "4000",
        ],
    );
    assert_success(&human_output);
    let text = stdout(&human_output);
    assert!(text.contains("handoff.generated"));
    let labels = [
        "Target agent",
        "Completed work",
        "Decisions",
        "Failures",
        "Next actions",
    ];
    let label_positions = labels.map(|label| {
        text.find(label)
            .unwrap_or_else(|| panic!("missing handoff detail label {label}"))
    });
    assert!(
        label_positions.windows(2).all(|pair| pair[0] < pair[1]),
        "handoff detail labels are not in semantic order"
    );
    for value in [
        target_agent,
        completed_work,
        redacted_decision,
        failure,
        next_action,
    ] {
        assert!(text.contains(value), "missing handoff detail value {value}");
    }
    assert!(!text.contains(project_path.as_str()));
    assert!(text.contains("[project root]"));
    assert_eq!(text.matches(note_text).count(), 1);
    assert_eq!(text.matches(checkpoint_message).count(), 1);

    let json_output = relay(
        project,
        &[
            "--json",
            "context",
            "--notes",
            "1",
            "--events",
            "4",
            "--checkpoints",
            "1",
            "--max-chars",
            "4000",
        ],
    );
    assert_success(&json_output);
    let document: serde_json::Value = serde_json::from_slice(&json_output.stdout).unwrap();
    let handoff_event = document["data"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "handoff.generated")
        .expect("handoff event is present");
    let payload = &handoff_event["payload"];
    assert_eq!(payload["agent"], target_agent);
    assert_eq!(
        payload["completed_work"],
        serde_json::json!([completed_work])
    );
    assert_eq!(payload["decisions"], serde_json::json!([decision]));
    assert_eq!(payload["failures"], serde_json::json!([failure]));
    assert_eq!(payload["next_action"], next_action);
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

#[test]
fn context_strict_character_budget_is_deterministic_and_reports_omission_causes() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    let goal = format!(
        "GOAL_UNICODE_界🙂é {}",
        "A deliberately long session goal 界🙂é ".repeat(24)
    );
    let notes = [
        format!(
            "NOTE_ALPHA_界🙂é {}",
            "alpha content with Unicode 界🙂é ".repeat(12)
        ),
        format!(
            "NOTE_BETA_界🙂é {}",
            "beta content with Unicode 界🙂é ".repeat(12)
        ),
        format!(
            "NOTE_GAMMA_界🙂é {}",
            "gamma content with Unicode 界🙂é ".repeat(12)
        ),
    ];
    let checkpoints = [
        format!(
            "CHECKPOINT_ALPHA_界🙂é {}",
            "alpha checkpoint detail 界🙂é ".repeat(10)
        ),
        format!(
            "CHECKPOINT_BETA_界🙂é {}",
            "beta checkpoint detail 界🙂é ".repeat(10)
        ),
        format!(
            "CHECKPOINT_GAMMA_界🙂é {}",
            "gamma checkpoint detail 界🙂é ".repeat(10)
        ),
    ];
    let next_action = format!(
        "NEXT_ACTION_界🙂é {}",
        "finish the deterministic budget review 界🙂é ".repeat(12)
    );

    assert_success(&relay(project, &["start", goal.as_str()]));
    for (category, note) in ["progress", "decision", "next_action"]
        .into_iter()
        .zip(notes.iter())
    {
        assert_success(&relay(project, &["note", category, note.as_str()]));
    }
    for checkpoint in &checkpoints {
        assert_success(&relay(project, &["checkpoint", checkpoint.as_str()]));
    }
    assert_success(&relay(
        project,
        &[
            "handoff",
            "budget-reviewer",
            "--next-action",
            next_action.as_str(),
        ],
    ));

    let limited_args = [
        "context",
        "--notes",
        "2",
        "--checkpoints",
        "2",
        "--events",
        "3",
        "--max-chars",
        "64",
    ];
    let minimum = relay(project, &limited_args);
    let repeated_minimum = relay(project, &limited_args);
    assert_success(&minimum);
    assert_success(&repeated_minimum);
    assert_eq!(minimum.stdout, repeated_minimum.stdout);
    let minimum_text = stdout(&minimum);
    assert!(
        minimum_text.chars().count() <= 64,
        "64-character output used {} characters:\n{minimum_text}",
        minimum_text.chars().count()
    );
    assert_eq!(
        minimum_text,
        "# Relay\nTruncated: yes\nCount N/C/E: 1/1/5\nBudget N/C/E: 2/2/3\n\n"
    );
    for value in [
        goal.as_str(),
        notes[1].as_str(),
        notes[2].as_str(),
        checkpoints[1].as_str(),
        checkpoints[2].as_str(),
        next_action.as_str(),
    ] {
        assert!(
            !minimum_text.contains(value),
            "minimum output unexpectedly contained a budget-omitted item"
        );
    }

    let generous_args = [
        "context",
        "--notes",
        "2",
        "--checkpoints",
        "2",
        "--events",
        "3",
        "--max-chars",
        "12000",
    ];
    let generous = relay(project, &generous_args);
    assert_success(&generous);
    let generous_text = stdout(&generous);
    let exact_budget = generous_text.chars().count();
    assert!(exact_budget < 12000);
    for line in [
        "Count-omitted notes: 1",
        "Count-omitted checkpoints: 1",
        "Count-omitted events: 5",
        "Budget-omitted notes: 0",
        "Budget-omitted checkpoints: 0",
        "Budget-omitted events: 0",
    ] {
        assert!(
            generous_text.lines().any(|actual| actual == line),
            "missing detailed omission metadata {line:?}:\n{generous_text}"
        );
    }

    let exact_budget_arg = exact_budget.to_string();
    let exact = relay(
        project,
        &[
            "context",
            "--notes",
            "2",
            "--checkpoints",
            "2",
            "--events",
            "3",
            "--max-chars",
            exact_budget_arg.as_str(),
        ],
    );
    assert_success(&exact);
    assert_eq!(exact.stdout, generous.stdout);

    let one_less_budget = (exact_budget - 1).to_string();
    let one_less = relay(
        project,
        &[
            "context",
            "--notes",
            "2",
            "--checkpoints",
            "2",
            "--events",
            "3",
            "--max-chars",
            one_less_budget.as_str(),
        ],
    );
    assert_success(&one_less);
    let one_less_text = stdout(&one_less);
    assert!(one_less_text.chars().count() < exact_budget);
    assert_ne!(one_less.stdout, generous.stdout);
    for line in [
        "Count-omitted notes: 1",
        "Count-omitted checkpoints: 1",
        "Count-omitted events: 5",
        "Budget-omitted notes: 0",
        "Budget-omitted checkpoints: 0",
        "Budget-omitted events: 1",
    ] {
        assert!(
            one_less_text.lines().any(|actual| actual == line),
            "missing one-less omission metadata {line:?}:\n{one_less_text}"
        );
    }
    let indivisible_values = [
        ("NOTE_BETA_", notes[1].as_str()),
        ("NOTE_GAMMA_", notes[2].as_str()),
        ("CHECKPOINT_BETA_", checkpoints[1].as_str()),
        ("CHECKPOINT_GAMMA_", checkpoints[2].as_str()),
        ("NEXT_ACTION_", next_action.as_str()),
    ];
    for (marker, value) in indivisible_values {
        let normalized_value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            one_less_text.contains(marker),
            one_less_text.contains(&normalized_value),
            "item beginning with {marker:?} was rendered partially"
        );
    }
    assert!(
        indivisible_values
            .iter()
            .any(|(marker, _)| !one_less_text.contains(marker)),
        "one-less output did not omit a whole item"
    );

    let minimum_json = relay(
        project,
        &[
            "--json",
            "context",
            "--notes",
            "2",
            "--checkpoints",
            "2",
            "--events",
            "3",
            "--max-chars",
            "64",
        ],
    );
    assert_success(&minimum_json);
    assert!(
        String::from_utf8(minimum_json.stdout.clone())
            .unwrap()
            .chars()
            .count()
            > 64,
        "--max-chars unexpectedly bounded the JSON envelope"
    );
    let document: serde_json::Value = serde_json::from_slice(&minimum_json.stdout).unwrap();
    let data = &document["data"];
    assert_eq!(data["notes"].as_array().unwrap().len(), 2);
    assert_eq!(data["notes"][0]["text"], notes[1]);
    assert_eq!(data["notes"][1]["text"], notes[2]);
    assert_eq!(data["checkpoints"].as_array().unwrap().len(), 2);
    assert_eq!(data["checkpoints"][0]["message"], checkpoints[1]);
    assert_eq!(data["checkpoints"][1]["message"], checkpoints[2]);
    assert_eq!(data["events"].as_array().unwrap().len(), 3);
    assert_eq!(
        data["omitted"],
        serde_json::json!({"notes": 1, "checkpoints": 1, "events": 5})
    );
    assert_eq!(
        data["budget_omitted"],
        serde_json::json!({"notes": 2, "checkpoints": 2, "events": 3})
    );
    assert_eq!(data["budget_truncated"], true);
    assert_eq!(data["truncated"], true);
    assert_eq!(data["max_chars"], 64);
    let handoff_event = data["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "handoff.generated")
        .expect("count-limited events retain the handoff");
    assert_eq!(handoff_event["payload"]["next_action"], next_action);
    assert!(handoff_event["payload"]["next_action"].is_string());

    let generous_json = relay(
        project,
        &[
            "--json",
            "context",
            "--notes",
            "2",
            "--checkpoints",
            "2",
            "--events",
            "3",
            "--max-chars",
            exact_budget_arg.as_str(),
        ],
    );
    assert_success(&generous_json);
    let document: serde_json::Value = serde_json::from_slice(&generous_json.stdout).unwrap();
    let data = &document["data"];
    assert_eq!(
        data["omitted"],
        serde_json::json!({"notes": 1, "checkpoints": 1, "events": 5})
    );
    assert_eq!(
        data["budget_omitted"],
        serde_json::json!({"notes": 0, "checkpoints": 0, "events": 0})
    );
    assert_eq!(data["budget_truncated"], false);
    assert_eq!(data["truncated"], true);
}

#[test]
fn navigation_is_deterministic_across_history_and_shared_session_selectors() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();

    assert_success(&relay(project, &["start", "first navigation session"]));
    let first = read_session(project);
    assert_success(&relay(project, &["note", "progress", "first oldest note"]));
    assert_success(&relay(project, &["checkpoint", "first checkpoint"]));
    assert_success(&relay(project, &["note", "decision", "first newest note"]));
    assert_success(&relay(project, &["finish"]));

    let resumed = relay(project, &["--json", "resume", "LaTeSt"]);
    let resumed_data = json_success_data(&resumed);
    let second = read_session(project);
    assert_eq!(resumed_data["session"]["id"], second.id.to_string());
    assert_eq!(second.resumed_from, Some(first.id));
    assert_success(&relay(project, &["note", "progress", "second oldest note"]));
    assert_success(&relay(project, &["checkpoint", "second checkpoint"]));
    assert_success(&relay(project, &["note", "decision", "second newest note"]));
    assert_success(&relay(project, &["finish"]));

    let events_path = project.join(".relay/events.jsonl");
    let original_bytes = fs::read(&events_path).unwrap();
    assert_eq!(original_bytes.last(), Some(&b'\n'));
    let generated_events = read_events(project);
    assert_eq!(generated_events.len(), 10);
    let event_kinds = generated_events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    for kind in ["note.recorded", "checkpoint.created", "session.finished"] {
        assert!(event_kinds.contains(&kind));
    }

    let original_values = String::from_utf8(original_bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(original_values.len(), generated_events.len());
    let fixed_timestamps = (0..original_values.len())
        .map(|index| format!("2024-01-02T03:04:{index:02}Z"))
        .collect::<Vec<_>>();
    let mut fixed_values = original_values.clone();
    for ((before, after), timestamp) in original_values
        .iter()
        .zip(&mut fixed_values)
        .zip(&fixed_timestamps)
    {
        after["timestamp"] = serde_json::Value::String(timestamp.clone());
        let mut before_without_timestamp = before.clone();
        let mut after_without_timestamp = after.clone();
        before_without_timestamp
            .as_object_mut()
            .unwrap()
            .remove("timestamp");
        after_without_timestamp
            .as_object_mut()
            .unwrap()
            .remove("timestamp");
        assert_eq!(after_without_timestamp, before_without_timestamp);
    }
    let mut fixed_bytes = Vec::new();
    for event in &fixed_values {
        serde_json::to_writer(&mut fixed_bytes, event).unwrap();
        fixed_bytes.push(b'\n');
    }
    fs::write(&events_path, &fixed_bytes).unwrap();
    assert_eq!(fixed_bytes.last(), Some(&b'\n'));

    let fixed_events = read_events(project);
    assert_eq!(fixed_events.len(), generated_events.len());
    for ((before, after), timestamp) in generated_events
        .iter()
        .zip(&fixed_events)
        .zip(&fixed_timestamps)
    {
        assert_eq!(after.schema_version, before.schema_version);
        assert_eq!(after.id, before.id);
        assert_eq!(after.session_id, before.session_id);
        assert_eq!(after.event_type, before.event_type);
        assert_eq!(after.payload, before.payload);
        assert_eq!(
            serde_json::to_value(after.timestamp).unwrap(),
            serde_json::json!(timestamp)
        );
    }

    let newest = if first.started_at > second.started_at
        || (first.started_at == second.started_at && first.id > second.id)
    {
        &first
    } else {
        &second
    };
    let other = if newest.id == first.id {
        &second
    } else {
        &first
    };
    let newest_id = newest.id.to_string();
    let other_id = other.id.to_string();
    let prefix_len = (8..=newest_id.len())
        .find(|&length| !other_id.starts_with(&newest_id[..length]))
        .unwrap();
    let newest_prefix = &newest_id[..prefix_len];
    let newest_events = fixed_events
        .iter()
        .filter(|event| event.session_id == newest.id)
        .collect::<Vec<_>>();
    assert_eq!(newest_events.len(), 5);

    let default_json = relay(
        project,
        &[
            "--json",
            "history",
            "--session",
            newest_prefix,
            "--limit",
            "2",
        ],
    );
    let default_data = json_success_data(&default_json);
    assert_json_object_has_required_keys(
        &default_data,
        &["events", "truncated", "order", "since", "until", "limit"],
    );
    assert_eq!(default_data["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        default_data["events"][0]["id"],
        newest_events[0].id.to_string()
    );
    assert_eq!(
        default_data["events"][1]["id"],
        newest_events[1].id.to_string()
    );
    assert_eq!(default_data["truncated"], true);
    assert_eq!(default_data["order"], "oldest_first");
    assert!(default_data["since"].is_null());
    assert!(default_data["until"].is_null());
    assert_eq!(default_data["limit"], 2);

    let default_human = relay(
        project,
        &["history", "--session", newest_prefix, "--limit", "2"],
    );
    assert_success(&default_human);
    let default_text = stdout(&default_human);
    let default_lines = default_text.lines().collect::<Vec<_>>();
    assert_eq!(default_lines.len(), 3);
    for (line, event) in default_lines[..2].iter().zip(&newest_events[..2]) {
        assert_eq!(
            *line,
            format!(
                "{}  {}  {}  {}",
                event.timestamp.to_rfc3339(),
                event.session_id,
                event.event_type,
                event.id
            )
        );
    }
    assert_eq!(default_lines[2], "Truncated: yes");

    let reverse_json = relay(
        project,
        &[
            "--json",
            "history",
            "--session",
            newest_prefix,
            "--reverse",
            "--limit",
            "2",
        ],
    );
    let reverse_data = json_success_data(&reverse_json);
    assert_json_object_has_required_keys(
        &reverse_data,
        &["events", "truncated", "order", "since", "until", "limit"],
    );
    assert_eq!(reverse_data["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        reverse_data["events"][0]["id"],
        newest_events[4].id.to_string()
    );
    assert_eq!(
        reverse_data["events"][1]["id"],
        newest_events[3].id.to_string()
    );
    assert_eq!(reverse_data["truncated"], true);
    assert_eq!(reverse_data["order"], "newest_first");
    assert!(reverse_data["since"].is_null());
    assert!(reverse_data["until"].is_null());
    assert_eq!(reverse_data["limit"], 2);

    let since = fixed_timestamps[fixed_events
        .iter()
        .position(|event| event.id == newest_events[1].id)
        .unwrap()]
    .clone();
    let until = fixed_timestamps[fixed_events
        .iter()
        .position(|event| event.id == newest_events[3].id)
        .unwrap()]
    .clone();
    assert_eq!(newest_events[1].event_type, "note.recorded");
    assert_eq!(newest_events[3].event_type, "note.recorded");
    let bounded_json = relay(
        project,
        &[
            "--json",
            "history",
            "--session",
            newest_prefix,
            "--type",
            "note.recorded",
            "--since",
            &since,
            "--until",
            &until,
            "--limit",
            "2",
        ],
    );
    let bounded_data = json_success_data(&bounded_json);
    assert_json_object_has_required_keys(
        &bounded_data,
        &["events", "truncated", "order", "since", "until", "limit"],
    );
    assert_eq!(bounded_data["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        bounded_data["events"][0]["id"],
        newest_events[1].id.to_string()
    );
    assert_eq!(
        bounded_data["events"][1]["id"],
        newest_events[3].id.to_string()
    );
    assert_eq!(bounded_data["truncated"], false);
    assert_eq!(bounded_data["order"], "oldest_first");
    assert_eq!(bounded_data["since"].as_str(), Some(since.as_str()));
    assert_eq!(bounded_data["until"].as_str(), Some(until.as_str()));
    assert_eq!(bounded_data["limit"], 2);

    let bytes_before_invalid_bounds = fs::read(&events_path).unwrap();
    let invalid_bounds = relay(
        project,
        &[
            "history",
            "--session",
            newest_prefix,
            "--since",
            &until,
            "--until",
            &since,
        ],
    );
    assert!(!invalid_bounds.status.success());
    let invalid_error = stderr(&invalid_bounds);
    assert!(invalid_error.contains("--since"));
    assert!(invalid_error.contains("--until"));
    assert_eq!(fs::read(&events_path).unwrap(), bytes_before_invalid_bounds);

    let shown = relay(project, &["--json", "show", "LATEST"]);
    let shown_data = json_success_data(&shown);
    assert_eq!(shown_data["session"]["id"], newest_id);

    let context = relay(
        project,
        &[
            "--json",
            "context",
            "--session",
            "laTESt",
            "--notes",
            "10",
            "--events",
            "10",
            "--checkpoints",
            "10",
        ],
    );
    let context_data = json_success_data(&context);
    assert_eq!(context_data["session"]["id"], newest.id.to_string());
    assert_eq!(context_data["events"].as_array().unwrap().len(), 5);
    assert!(
        context_data["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["session_id"].as_str() == Some(newest_id.as_str()))
    );

    let latest_history = relay(
        project,
        &["--json", "history", "--session", "lAtEsT", "--limit", "100"],
    );
    let latest_history_data = json_success_data(&latest_history);
    assert_eq!(
        latest_history_data["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        newest_events
            .iter()
            .map(|event| event.id.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn search_is_deterministic_filterable_and_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    let project_path = project.to_string_lossy().into_owned();
    let goal = "first needle-goal for resumed search";
    let progress_text = format!(
        "needle-human progress\npath {project_path}\twith collapsed whitespace \u{1b}control"
    );
    let checkpoint_message = "nested checkpoint needle-checkpoint message";
    let completed_match = "needle-handoff completed array element";
    let later_completed_match = "needle-handoff alphabetically-earlier later array element";
    let decision_match = "second needle-decision note";

    assert_success(&relay(project, &["start", goal]));
    let first = read_session(project);
    assert_success(&relay_with_stdin(
        project,
        &["note", "progress", "--stdin"],
        progress_text.as_bytes(),
    ));
    assert_success(&relay(project, &["checkpoint", checkpoint_message]));
    assert_success(&relay(
        project,
        &[
            "handoff",
            "search-reader",
            "--completed-work",
            "plain completed array element",
            "--completed-work",
            completed_match,
            "--completed-work",
            later_completed_match,
            "--decision",
            "needle-handoff later decision array element",
            "--failure",
            "Needle-handoff mixed-case decoy",
            "--next-action",
            "plain next action",
        ],
    ));
    assert_success(&relay(project, &["finish"]));

    assert_success(&relay(project, &["resume", "latest"]));
    let resumed = read_session(project);
    assert_eq!(resumed.resumed_from, Some(first.id));
    assert_success(&relay(project, &["note", "decision", decision_match]));
    assert_success(&relay(project, &["finish"]));

    let relay_root = project.join(".relay");
    let sessions_root = relay_root.join("sessions");
    let first_session_path = sessions_root.join(format!("{}.json", first.id));
    let resumed_session_path = sessions_root.join(format!("{}.json", resumed.id));
    let current_session_path = relay_root.join("session.json");
    let original_completed_first: Session =
        serde_json::from_slice(&fs::read(&first_session_path).unwrap()).unwrap();
    let original_completed_resumed: Session =
        serde_json::from_slice(&fs::read(&resumed_session_path).unwrap()).unwrap();
    let original_current_session: Session =
        serde_json::from_slice(&fs::read(&current_session_path).unwrap()).unwrap();
    assert_eq!(original_completed_first.state, SessionState::Completed);
    assert_eq!(original_completed_resumed.state, SessionState::Completed);
    assert_eq!(original_completed_resumed.id, resumed.id);
    assert_eq!(original_completed_resumed.resumed_from, Some(first.id));
    assert_eq!(original_current_session, original_completed_resumed);

    let earlier_started_at = "2024-02-01T00:00:00Z".parse().unwrap();
    let later_started_at = "2024-02-02T00:00:00Z".parse().unwrap();
    let mut fixed_first_session = original_completed_first.clone();
    let mut fixed_resumed_session = original_completed_resumed.clone();
    let mut fixed_current_session = original_current_session.clone();
    fixed_first_session.started_at = earlier_started_at;
    fixed_resumed_session.started_at = later_started_at;
    fixed_current_session.started_at = later_started_at;
    for (path, session) in [
        (&first_session_path, &fixed_first_session),
        (&resumed_session_path, &fixed_resumed_session),
        (&current_session_path, &fixed_current_session),
    ] {
        let mut bytes = serde_json::to_vec_pretty(session).unwrap();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
    }

    let completed_first: Session =
        serde_json::from_slice(&fs::read(&first_session_path).unwrap()).unwrap();
    let completed_resumed: Session =
        serde_json::from_slice(&fs::read(&resumed_session_path).unwrap()).unwrap();
    let current_session: Session =
        serde_json::from_slice(&fs::read(&current_session_path).unwrap()).unwrap();
    assert_eq!(completed_first, fixed_first_session);
    assert_eq!(completed_resumed, fixed_resumed_session);
    assert_eq!(current_session, fixed_current_session);
    assert_eq!(
        [completed_first.id, completed_resumed.id],
        [first.id, resumed.id]
    );
    assert!(completed_first.started_at < completed_resumed.started_at);
    assert_eq!(current_session.id, resumed.id);
    assert_eq!(current_session.started_at, completed_resumed.started_at);

    let events_path = project.join(".relay/events.jsonl");
    let original_bytes = fs::read(&events_path).unwrap();
    assert_eq!(original_bytes.last(), Some(&b'\n'));
    let generated_events = read_events(project);
    assert_eq!(
        generated_events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            "session.started",
            "note.recorded",
            "checkpoint.created",
            "handoff.generated",
            "session.finished",
            "session.resumed",
            "note.recorded",
            "session.finished",
        ]
    );

    let original_values = String::from_utf8(original_bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(original_values.len(), generated_events.len());
    let fixed_timestamps = (0..original_values.len())
        .map(|index| format!("2024-02-03T04:05:{index:02}Z"))
        .collect::<Vec<_>>();
    let mut fixed_values = original_values.clone();
    for ((before, after), timestamp) in original_values
        .iter()
        .zip(&mut fixed_values)
        .zip(&fixed_timestamps)
    {
        after["timestamp"] = serde_json::Value::String(timestamp.clone());
        let mut before_without_timestamp = before.clone();
        let mut after_without_timestamp = after.clone();
        before_without_timestamp
            .as_object_mut()
            .unwrap()
            .remove("timestamp");
        after_without_timestamp
            .as_object_mut()
            .unwrap()
            .remove("timestamp");
        assert_eq!(after_without_timestamp, before_without_timestamp);
    }
    let mut fixed_bytes = Vec::new();
    for event in &fixed_values {
        serde_json::to_writer(&mut fixed_bytes, event).unwrap();
        fixed_bytes.push(b'\n');
    }
    fs::write(&events_path, &fixed_bytes).unwrap();
    assert_eq!(fixed_bytes.last(), Some(&b'\n'));

    let fixed_events = read_events(project);
    assert_eq!(fixed_events.len(), generated_events.len());
    for ((before, after), timestamp) in generated_events
        .iter()
        .zip(&fixed_events)
        .zip(&fixed_timestamps)
    {
        assert_eq!(after.schema_version, before.schema_version);
        assert_eq!(after.id, before.id);
        assert_eq!(after.session_id, before.session_id);
        assert_eq!(after.event_type, before.event_type);
        assert_eq!(after.payload, before.payload);
        assert_eq!(
            serde_json::to_value(after.timestamp).unwrap(),
            serde_json::json!(timestamp)
        );
    }

    let lock_before = fs::read(relay_root.join("lock")).unwrap();
    let snapshot_directory = |path: &Path| {
        let mut files = fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_owned(),
                    fs::read(path).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    };
    let directory_names = |path: &Path| {
        let mut names = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let relay_entries_before = directory_names(&relay_root);
    let current_session_before = fs::read(relay_root.join("session.json")).unwrap();
    let sessions_before = snapshot_directory(&relay_root.join("sessions"));
    let checkpoints_before = snapshot_directory(&relay_root.join("checkpoints"));
    let handoffs_before = snapshot_directory(&relay_root.join("handoffs"));
    let operations_before = snapshot_directory(&relay_root.join("operations"));
    let first_summary_path = relay_root.join(format!("summary-{}.md", first.id));
    let resumed_summary_path = relay_root.join(format!("summary-{}.md", resumed.id));
    let first_summary_before = fs::read(&first_summary_path).unwrap();
    let resumed_summary_before = fs::read(&resumed_summary_path).unwrap();
    assert_eq!(sessions_before.len(), 2);
    assert_eq!(checkpoints_before.len(), 1);
    assert_eq!(handoffs_before.len(), 1);

    let default_search = relay(project, &["--json", "search", "needle"]);
    let default_data = json_success_data(&default_search);
    let repeated_default = relay(project, &["--json", "search", "needle"]);
    assert_success(&repeated_default);
    assert!(repeated_default.stderr.is_empty());
    assert_eq!(repeated_default.stdout, default_search.stdout);
    let mut data_keys = default_data
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    data_keys.sort_unstable();
    assert_eq!(
        data_keys,
        [
            "category",
            "limit",
            "matches",
            "order",
            "query",
            "session",
            "since",
            "truncated",
            "type",
            "until",
        ]
    );
    assert_eq!(default_data["query"], "needle");
    assert_eq!(default_data["order"], "oldest_first");
    assert_eq!(default_data["truncated"], false);
    assert!(default_data["session"].is_null());
    assert!(default_data["type"].is_null());
    assert!(default_data["category"].is_null());
    assert!(default_data["since"].is_null());
    assert!(default_data["until"].is_null());
    assert_eq!(default_data["limit"], 100);

    let matches = default_data["matches"].as_array().unwrap();
    let expected_matches = [
        (0, "/payload/goal", goal),
        (1, "/payload/text", progress_text.as_str()),
        (2, "/payload/checkpoint/message", checkpoint_message),
        (3, "/payload/completed_work/1", completed_match),
        (5, "/payload/goal", goal),
        (6, "/payload/text", decision_match),
    ];
    assert_eq!(matches.len(), expected_matches.len());
    for (matched, (event_index, field, text)) in matches.iter().zip(expected_matches) {
        let mut match_keys = matched
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        match_keys.sort_unstable();
        assert_eq!(match_keys, ["event", "field", "text"]);
        assert_eq!(
            matched["event"],
            serde_json::to_value(&fixed_events[event_index]).unwrap()
        );
        assert_eq!(matched["field"], field);
        assert_eq!(matched["text"], text);
    }

    let type_precedence = json_success_data(&relay(project, &["--json", "search", "note"]));
    let type_matches = type_precedence["matches"].as_array().unwrap();
    assert_eq!(type_matches.len(), 2);
    for (matched, event_index) in type_matches.iter().zip([1, 6]) {
        assert_eq!(
            matched["event"],
            serde_json::to_value(&fixed_events[event_index]).unwrap()
        );
        assert_eq!(matched["field"], "type");
        assert_eq!(matched["text"], "note.recorded");
    }

    let exact_type = json_success_data(&relay(
        project,
        &[
            "--json",
            "search",
            "needle",
            "--type",
            "checkpoint.created",
            "--limit",
            "1",
        ],
    ));
    assert_eq!(exact_type["type"], "checkpoint.created");
    assert_eq!(exact_type["matches"].as_array().unwrap().len(), 1);
    assert_eq!(
        exact_type["matches"][0]["event"],
        serde_json::to_value(&fixed_events[2]).unwrap()
    );
    assert_eq!(
        exact_type["matches"][0]["field"],
        "/payload/checkpoint/message"
    );
    assert_eq!(exact_type["truncated"], false);

    let latest = json_success_data(&relay(
        project,
        &["--json", "search", "needle", "--session", "latest"],
    ));
    assert_eq!(latest["session"], resumed.id.to_string());
    assert_eq!(
        latest["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|matched| matched["event"]["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            fixed_events[5].id.to_string(),
            fixed_events[6].id.to_string()
        ]
    );

    let categorized = json_success_data(&relay(
        project,
        &[
            "--json",
            "search",
            "needle",
            "--session",
            "LaTeSt",
            "--category",
            "decision",
            "--limit",
            "1",
        ],
    ));
    assert_eq!(categorized["session"], resumed.id.to_string());
    assert_eq!(categorized["category"], "decision");
    assert_eq!(categorized["limit"], 1);
    assert_eq!(categorized["truncated"], false);
    assert_eq!(categorized["matches"].as_array().unwrap().len(), 1);
    assert_eq!(
        categorized["matches"][0]["event"],
        serde_json::to_value(&fixed_events[6]).unwrap()
    );
    assert_eq!(categorized["matches"][0]["field"], "/payload/text");

    let bounded = json_success_data(&relay(
        project,
        &[
            "--json",
            "search",
            "needle",
            "--since",
            &fixed_timestamps[2],
            "--until",
            &fixed_timestamps[5],
            "--limit",
            "3",
        ],
    ));
    assert_eq!(bounded["since"], fixed_timestamps[2]);
    assert_eq!(bounded["until"], fixed_timestamps[5]);
    assert_eq!(bounded["truncated"], false);
    assert_eq!(
        bounded["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|matched| matched["event"]["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            fixed_events[2].id.to_string(),
            fixed_events[3].id.to_string(),
            fixed_events[5].id.to_string(),
        ]
    );

    let reversed = json_success_data(&relay(
        project,
        &["--json", "search", "needle", "--reverse", "--limit", "2"],
    ));
    assert_eq!(reversed["order"], "newest_first");
    assert_eq!(reversed["limit"], 2);
    assert_eq!(reversed["truncated"], true);
    assert_eq!(
        reversed["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|matched| matched["event"]["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            fixed_events[6].id.to_string(),
            fixed_events[5].id.to_string()
        ]
    );

    let zero_limit = json_success_data(&relay(
        project,
        &["--json", "search", "needle", "--limit", "0"],
    ));
    assert!(zero_limit["matches"].as_array().unwrap().is_empty());
    assert_eq!(zero_limit["limit"], 0);
    assert_eq!(zero_limit["truncated"], true);

    let case_mismatch = json_success_data(&relay(project, &["--json", "search", "NEEDLE"]));
    assert!(case_mismatch["matches"].as_array().unwrap().is_empty());
    assert_eq!(case_mismatch["truncated"], false);

    let empty_human = relay(project, &["search", "needle", "--type", "session"]);
    assert_success(&empty_human);
    assert!(empty_human.stderr.is_empty());
    assert_eq!(empty_human.stdout, b"Truncated: no\n");

    let safe_human = relay(project, &["search", "needle-human"]);
    assert_success(&safe_human);
    assert!(safe_human.stderr.is_empty());
    let safe_lines = stdout(&safe_human)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        safe_lines,
        [
            format!(
                "{}  {}  note.recorded  /payload/text  needle-human progress path [project root] with collapsed whitespace \\u{{1b}}control",
                fixed_events[1].timestamp.to_rfc3339(),
                first.id,
            ),
            "Truncated: no".to_owned(),
        ]
    );
    assert!(!safe_lines[0].contains(project_path.as_str()));
    assert!(!safe_lines[0].contains('\t'));
    assert!(safe_lines[0].contains("\\u{1b}"));
    assert!(!safe_lines[0].contains('\u{1b}'));

    assert_eq!(fs::read(&events_path).unwrap(), fixed_bytes);
    assert_eq!(fs::read(relay_root.join("lock")).unwrap(), lock_before);
    assert_eq!(
        fs::read(relay_root.join("session.json")).unwrap(),
        current_session_before
    );
    assert_eq!(
        snapshot_directory(&relay_root.join("sessions")),
        sessions_before
    );
    assert_eq!(
        snapshot_directory(&relay_root.join("checkpoints")),
        checkpoints_before
    );
    assert_eq!(
        snapshot_directory(&relay_root.join("handoffs")),
        handoffs_before
    );
    assert_eq!(
        snapshot_directory(&relay_root.join("operations")),
        operations_before
    );
    assert_eq!(fs::read(&first_summary_path).unwrap(), first_summary_before);
    assert_eq!(
        fs::read(&resumed_summary_path).unwrap(),
        resumed_summary_before
    );
    assert_eq!(directory_names(&relay_root), relay_entries_before);

    let invalid_range = relay(
        project,
        &[
            "search",
            "needle",
            "--since",
            &fixed_timestamps[5],
            "--until",
            &fixed_timestamps[2],
        ],
    );
    assert!(!invalid_range.status.success());
    assert!(invalid_range.stdout.is_empty());
    assert!(stderr(&invalid_range).contains(
        "`--since` must be earlier than or equal to `--until`; use an earlier `--since` or a later `--until`"
    ));
    assert_eq!(fs::read(&events_path).unwrap(), fixed_bytes);
    assert_eq!(fs::read(relay_root.join("lock")).unwrap(), lock_before);
    assert_eq!(
        fs::read(relay_root.join("session.json")).unwrap(),
        current_session_before
    );
    assert_eq!(
        snapshot_directory(&relay_root.join("sessions")),
        sessions_before
    );
    assert_eq!(
        snapshot_directory(&relay_root.join("checkpoints")),
        checkpoints_before
    );
    assert_eq!(
        snapshot_directory(&relay_root.join("handoffs")),
        handoffs_before
    );
    assert_eq!(
        snapshot_directory(&relay_root.join("operations")),
        operations_before
    );
    assert_eq!(fs::read(&first_summary_path).unwrap(), first_summary_before);
    assert_eq!(
        fs::read(&resumed_summary_path).unwrap(),
        resumed_summary_before
    );
    assert_eq!(directory_names(&relay_root), relay_entries_before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        let outside_session_path = outside.path().join("session.json");
        let mut outside_session = fixed_current_session.clone();
        outside_session.started_at = "2024-02-04T00:00:00Z".parse().unwrap();
        let mut outside_bytes = serde_json::to_vec_pretty(&outside_session).unwrap();
        outside_bytes.push(b'\n');
        fs::write(&outside_session_path, &outside_bytes).unwrap();

        let unsafe_session_path = sessions_root.join("unsafe.json");
        symlink(&outside_session_path, &unsafe_session_path).unwrap();
        let unsafe_search = relay(project, &["search", "needle", "--session", "latest"]);
        assert!(!unsafe_search.status.success());
        assert!(unsafe_search.stdout.is_empty());
        let unsafe_error = stderr(&unsafe_search);
        assert!(unsafe_error.contains("refusing to use symlinked Relay state path"));
        assert!(unsafe_error.contains(".relay/sessions/unsafe.json"));
        assert_eq!(fs::read(&outside_session_path).unwrap(), outside_bytes);

        fs::remove_file(&unsafe_session_path).unwrap();
        assert_eq!(fs::read(&outside_session_path).unwrap(), outside_bytes);
    }
}
#[test]
fn structured_handoff_round_trips_through_context_and_keeps_snapshot() {
    let directory = init_repository();
    let project = directory.path();
    assert_success(&relay(project, &["start", "handoff goal"]));
    assert_success(&relay(project, &["note", "decision", "keep provenance"]));
    assert_success(&relay(project, &["checkpoint", "handoff boundary"]));
    fs::write(project.join("tracked file.txt"), "working change").unwrap();

    let created = relay(
        project,
        &[
            "--json",
            "handoff",
            "create",
            "--to",
            "agent-b",
            "--source-agent",
            "agent-a",
            "--completed-work",
            "implemented handoff",
            "--decision",
            "explicit choice",
            "--failure",
            "none",
            "--next-actions",
            "review output",
            "--next-actions",
            "merge change",
        ],
    );
    let data = json_success_data(&created);
    let record = data["handoff"].clone();
    assert_eq!(record["schema_version"], 2);
    assert!(record["id"].as_str().is_some());
    assert_eq!(record["source_agent"], "agent-a");
    assert_eq!(record["target_agent"], "agent-b");
    assert_eq!(record["goal"], "handoff goal");
    assert_eq!(record["completed"][0]["source"], "explicit");
    assert_eq!(record["decisions"][0]["source"], "recorded_note");
    assert_eq!(record["git"]["branch"], "main");
    assert_eq!(record["git"]["changed_files"], json!(["tracked file.txt"]));
    assert_eq!(
        record["references"]["checkpoints"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(record["references"]["notes"][0]["category"], "decision");

    let artifact_path = data["path"].as_str().unwrap();
    assert_eq!(
        fs::read(project.join(artifact_path)).unwrap(),
        data["markdown"].as_str().unwrap().as_bytes()
    );
    let ledger_before = fs::read(project.join(".relay/handoffs.jsonl")).unwrap();
    let id = record["id"].as_str().unwrap().to_owned();

    let show = relay(project, &["--json", "handoff", "show", id.as_str()]);
    assert_eq!(json_success_data(&show)["handoff"], record);
    let latest = relay(project, &["--json", "handoff", "latest"]);
    assert_eq!(json_success_data(&latest)["handoff"], record);
    let context = relay(project, &["--json", "context", "--handoff", id.as_str()]);
    let context_data = json_success_data(&context);
    assert_eq!(context_data["handoff"], record);
    assert_eq!(
        context_data["git"]["changed_files"],
        json!(["tracked file.txt"])
    );
    assert!(
        context_data["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["text"] == "keep provenance")
    );
    assert!(
        stdout(&relay(project, &["context", "--handoff", id.as_str()])).contains("handoff goal")
    );

    fs::write(project.join("tracked file.txt"), "changed later").unwrap();
    assert_success(&relay(
        project,
        &["note", "blocker", "new note after handoff"],
    ));
    let reread = relay(project, &["--json", "context", "--handoff", id.as_str()]);
    assert_eq!(json_success_data(&reread)["handoff"], record);
    assert_eq!(
        fs::read(project.join(".relay/handoffs.jsonl")).unwrap(),
        ledger_before
    );
    let second = relay(project, &["--json", "handoff", "agent-c"]);
    let second_id = json_success_data(&second)["handoff"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(id, second_id);
    let listed = relay(project, &["--json", "handoff", "list"]);
    assert_eq!(
        json_success_data(&listed)["handoffs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn legacy_handoff_is_read_without_destructive_migration() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    let handoffs = project.join(".relay/handoffs");
    fs::create_dir_all(&handoffs).unwrap();
    let path = handoffs.join("legacy-handoff.md");
    let bytes = b"legacy handoff";
    fs::write(&path, bytes).unwrap();

    let show = relay(project, &["handoff", "show", "legacy-handoff"]);
    assert_success(&show);
    assert!(stdout(&show).contains("legacy handoff"));
    let json_show = relay(project, &["--json", "handoff", "show", "legacy-handoff"]);
    let data = json_success_data(&json_show);
    assert_eq!(data["legacy"], true);
    assert_eq!(data["handoff"], serde_json::Value::Null);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(!project.join(".relay/handoffs.jsonl").exists());
}

#[test]
fn handoff_rejects_invalid_references_and_unsafe_legacy_files() {
    let directory = init_repository();
    let project = directory.path();
    assert_success(&relay(project, &["start", "security"]));

    let invalid = relay(project, &["handoff", "create", "--to", "../outside"]);
    assert!(!invalid.status.success());
    assert!(stderr(&invalid).contains("target agent must match"));
    let invalid_source = relay(
        project,
        &[
            "handoff",
            "create",
            "--to",
            "agent-b",
            "--source-agent",
            "bad/name",
        ],
    );
    assert!(!invalid_source.status.success());
    assert!(stderr(&invalid_source).contains("source agent must match"));

    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.md");
        fs::write(&outside_file, b"outside").unwrap();
        let unsafe_file = project.join(".relay/handoffs/unsafe.md");
        std::os::unix::fs::symlink(&outside_file, &unsafe_file).unwrap();
        let listed = relay(project, &["handoff", "list"]);
        assert!(!listed.status.success());
        assert!(stderr(&listed).contains("handoff file must not be symlinked"));
        assert_eq!(fs::read(&outside_file).unwrap(), b"outside");
        fs::remove_file(&unsafe_file).unwrap();
    }

    let ledger = project.join(".relay/handoffs.jsonl");
    let invalid_json = br#"{"schema_version":99}"#.to_vec();
    let mut invalid_json_line = invalid_json;
    invalid_json_line.push(10);
    fs::write(&ledger, &invalid_json_line).unwrap();
    let unsupported = relay(project, &["handoff", "list"]);
    assert!(!unsupported.status.success());
    assert!(stderr(&unsupported).contains("unsupported handoff schema_version 99"));
    assert_eq!(fs::read(&ledger).unwrap(), invalid_json_line);
}
#[test]
fn handoff_context_respects_character_budget_at_section_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    assert_success(&relay(project, &["start", "bounded handoff"]));
    let long = "x".repeat(4096);
    let created = relay(
        project,
        &["handoff", "agent-b", "--completed-work", long.as_str()],
    );
    assert_success(&created);

    let human = relay(
        project,
        &["context", "--handoff", "latest", "--max-chars", "128"],
    );
    assert_success(&human);
    let text = stdout(&human);
    assert!(text.chars().count() <= 128);
    assert!(text.contains("Truncated: yes"));
    assert!(!text.contains(long.as_str()));

    let json_output = relay(
        project,
        &[
            "--json",
            "context",
            "--handoff",
            "latest",
            "--max-chars",
            "128",
        ],
    );
    let data = json_success_data(&json_output);
    assert_eq!(data["truncated"], true);
    assert_eq!(data["max_chars"], 128);
}

#[test]
fn handoff_count_includes_legacy_markdown_entries() {
    let directory = init_repository();
    let project = directory.path();
    assert_success(&relay(project, &["start", "count goal"]));
    let seed = relay(
        project,
        &[
            "--json",
            "handoff",
            "seed-agent",
            "--completed-work",
            "seed",
        ],
    );
    let seed_data = json_success_data(&seed);
    let seed_record = seed_data["handoff"].clone();
    let seed_sidecar = project.join(seed_record["artifact_path"].as_str().unwrap());
    fs::remove_file(seed_sidecar).unwrap();

    let ledger = project.join(".relay/handoffs.jsonl");
    let mut ledger_bytes = Vec::new();
    for index in 1..=127 {
        let mut record = seed_record.clone();
        record["id"] = format!("00000000-0000-0000-0000-{index:012x}").into();
        record["artifact_path"] = format!(".relay/handoffs/record-{index}.md").into();
        ledger_bytes.extend_from_slice(serde_json::to_string(&record).unwrap().as_bytes());
        ledger_bytes.push(10);
    }
    fs::write(&ledger, &ledger_bytes).unwrap();
    let legacy = project.join(".relay/handoffs/legacy.md");
    fs::write(&legacy, "# legacy\n").unwrap();
    let legacy_bytes = fs::read(&legacy).unwrap();

    let rejected = relay(project, &["handoff", "next-agent"]);
    assert!(!rejected.status.success());
    assert!(stderr(&rejected).contains("handoff store reached maximum of 128 records"));
    assert_eq!(fs::read(&ledger).unwrap(), ledger_bytes);
    assert_eq!(fs::read(&legacy).unwrap(), legacy_bytes);
}

#[test]
fn legacy_display_names_and_oversized_history_remain_compatible() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    assert_success(&relay(project, &["start", "small goal"]));

    let long_goal = "x".repeat(MAX_HANDOFF_GOAL_BYTES + 1024);
    let session_path = project.join(".relay/session.json");
    let mut session = read_session(project);
    session.goal = long_goal.clone();
    session.last_agent = Some("Claude Code".into());
    fs::write(&session_path, serde_json::to_vec(&session).unwrap()).unwrap();
    let durable_session_path = project
        .join(".relay/sessions")
        .join(format!("{}.json", session.id));
    fs::write(durable_session_path, serde_json::to_vec(&session).unwrap()).unwrap();

    let structured = relay(project, &["--json", "handoff", "create", "--to", "agent-b"]);
    let structured_data = json_success_data(&structured);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(project.join(".relay/handoffs.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(project.join(".relay/handoffs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let record = &structured_data["handoff"];
    assert_eq!(record["target_agent"], "agent-b");
    assert_eq!(record["source_session"]["last_agent"], "Claude Code");
    assert_eq!(
        record["goal"].as_str().unwrap().len(),
        MAX_HANDOFF_GOAL_BYTES
    );
    assert_eq!(
        record["source_session"]["goal"].as_str().unwrap().len(),
        MAX_HANDOFF_GOAL_BYTES
    );
    assert_eq!(read_session(project).goal, long_goal);

    let legacy = relay(project, &["--json", "handoff", "Claude Code"]);
    assert_eq!(
        json_success_data(&legacy)["handoff"]["target_agent"],
        "Claude Code"
    );
    assert_eq!(
        read_session(project).last_agent.as_deref(),
        Some("Claude Code")
    );

    let control_agent = "agent\u{1b}[31m";
    let controls = relay(project, &["handoff", control_agent]);
    assert_success(&controls);
    assert!(!stdout(&controls).contains('\u{1b}'));

    let identifier = relay(project, &["--json", "handoff", "codex"]);
    assert_eq!(
        json_success_data(&identifier)["handoff"]["target_agent"],
        "codex"
    );
    assert_eq!(read_session(project).last_agent.as_deref(), Some("codex"));
}

#[cfg(unix)]
#[test]
fn handoff_rejects_symlinked_and_hardlinked_checkpoint_artifacts() {
    for hardlink in [false, true] {
        let directory = init_repository();
        let project = directory.path();
        assert_success(&relay(project, &["start", "checkpoint safety"]));
        assert_success(&relay(project, &["checkpoint", "saved checkpoint"]));
        let checkpoint = only_file(project.join(".relay/checkpoints"));
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("checkpoint.json");
        fs::write(&outside_file, fs::read(&checkpoint).unwrap()).unwrap();
        let outside_bytes = fs::read(&outside_file).unwrap();
        fs::remove_file(&checkpoint).unwrap();
        if hardlink {
            fs::hard_link(&outside_file, &checkpoint).unwrap();
        } else {
            std::os::unix::fs::symlink(&outside_file, &checkpoint).unwrap();
        }

        let handoff = relay(project, &["--json", "handoff", "agent-b"]);
        assert!(!handoff.status.success());
        assert!(handoff.stdout.is_empty());
        assert!(String::from_utf8_lossy(&handoff.stderr).contains("handoff file"));
        assert!(!project.join(".relay/handoffs.jsonl").exists());
        assert_eq!(fs::read(&outside_file).unwrap(), outside_bytes);
    }
}

#[test]
fn reserved_legacy_agent_names_use_explicit_target_escape() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path();
    assert_success(&relay(project, &["start", "reserved names"]));

    for target in ["show", "latest", "list", "create"] {
        let output = relay(project, &["--json", "handoff", "--target", target]);
        assert_eq!(
            json_success_data(&output)["handoff"]["target_agent"],
            target
        );
        assert_eq!(read_session(project).last_agent.as_deref(), Some(target));
    }
}
