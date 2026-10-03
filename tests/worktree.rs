use relay::worktree::{self, ClaimKind};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn relay(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run Relay")
}

fn relay_owned(cwd: &Path, args: Vec<String>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run Relay")
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run Git")
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "Relay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("valid Relay JSON")
}

fn fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let temporary = tempfile::tempdir().expect("create Git fixture");
    let repository = temporary.path().join("repo");
    let sibling_a = temporary.path().join("relay-parser");
    let sibling_b = temporary.path().join("relay-runner");
    fs::create_dir(&repository).unwrap();
    git_ok(&repository, &["init", "--quiet"]);
    git_ok(&repository, &["config", "user.email", "relay@example.com"]);
    git_ok(&repository, &["config", "user.name", "Relay tests"]);
    fs::write(repository.join("seed.txt"), "seed\n").unwrap();
    git_ok(&repository, &["add", "seed.txt"]);
    git_ok(&repository, &["commit", "--quiet", "-m", "initial"]);
    git_ok(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feat/parser",
            sibling_a.to_str().unwrap(),
        ],
    );
    git_ok(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feat/runner",
            sibling_b.to_str().unwrap(),
        ],
    );
    (temporary, repository, sibling_a, sibling_b)
}

fn worktree<'a>(list: &'a Value, path: &Path) -> &'a Value {
    list["data"]["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["worktree_path"] == path.to_string_lossy().as_ref())
        .expect("worktree in list")
}

#[test]
fn lists_main_siblings_nested_detached_and_git_file_worktrees() {
    let (_temporary, repository, sibling_a, sibling_b) = fixture();
    let nested = repository.join("nested/deep");
    fs::create_dir_all(&nested).unwrap();

    let list = json(&relay(&repository, &["--json", "worktree", "list"]));
    let worktrees = list["data"]["worktrees"].as_array().unwrap();
    assert_eq!(worktrees.len(), 3);
    assert_eq!(
        list["data"]["repository_root"],
        repository.to_string_lossy().as_ref()
    );
    assert!(
        worktrees
            .iter()
            .all(|item| item["repository_id"] == list["data"]["repository_id"])
    );
    assert_eq!(worktree(&list, &repository)["current"], true);
    assert_eq!(worktree(&list, &sibling_a)["branch"], "feat/parser");
    assert!(
        fs::symlink_metadata(sibling_a.join(".git"))
            .unwrap()
            .is_file()
    );

    let nested_show = relay_owned(
        &nested,
        vec![
            "--json".into(),
            "worktree".into(),
            "show".into(),
            sibling_a.to_string_lossy().into_owned(),
        ],
    );
    let shown = json(&nested_show);
    assert_eq!(
        shown["data"]["worktree"]["worktree_path"],
        sibling_a.to_string_lossy().as_ref()
    );

    git_ok(&sibling_b, &["switch", "--detach", "--quiet"]);
    let detached = json(&relay(&repository, &["--json", "worktree", "list"]));
    let detached = worktree(&detached, &sibling_b);
    assert_eq!(detached["detached"], true);
    assert!(detached["branch"].is_null());
    assert!(detached["ref_name"].is_null());

    let non_git = tempfile::tempdir().unwrap();
    let unavailable = json(&relay(non_git.path(), &["--json", "worktree", "list"]));
    assert_eq!(unavailable["data"]["available"], false);
    assert!(
        unavailable["data"]["worktrees"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn preserves_historical_provenance_after_head_changes_and_prune() {
    let (_temporary, repository, sibling_a, _sibling_b) = fixture();
    assert!(relay(&repository, &["start", "original"]).status.success());
    let session_before: Value =
        serde_json::from_slice(&fs::read(repository.join(".relay/session.json")).unwrap()).unwrap();
    let initial_branch = session_before["initial_git"]["worktree"]["branch"].clone();
    let initial_head = session_before["initial_git"]["worktree"]["head"].clone();

    git_ok(&repository, &["switch", "--quiet", "-c", "advanced"]);
    fs::write(repository.join("seed.txt"), "advanced\n").unwrap();
    git_ok(&repository, &["add", "seed.txt"]);
    git_ok(&repository, &["commit", "--quiet", "-m", "advanced"]);
    assert!(relay(&repository, &["finish"]).status.success());

    let session: Value =
        serde_json::from_slice(&fs::read(repository.join(".relay/session.json")).unwrap()).unwrap();
    assert_eq!(
        session["initial_git"]["worktree"]["worktree_path"],
        repository.to_string_lossy().as_ref()
    );
    assert_eq!(session["initial_git"]["worktree"]["branch"], initial_branch);
    assert_eq!(session["initial_git"]["worktree"]["head"], initial_head);
    assert_eq!(session["final_git"]["worktree"]["branch"], "advanced");
    assert_ne!(session["final_git"]["worktree"]["head"], initial_head);

    let _ = json(&relay(&repository, &["--json", "worktree", "list"]));
    fs::remove_dir_all(&sibling_a).unwrap();
    let stale = json(&relay(&repository, &["--json", "worktree", "list"]));
    assert_eq!(worktree(&stale, &sibling_a)["stale"], true);
    assert_eq!(worktree(&stale, &sibling_a)["deleted"], true);
    git_ok(&repository, &["worktree", "prune"]);
    let pruned = json(&relay(&repository, &["--json", "worktree", "list"]));
    assert_eq!(worktree(&pruned, &sibling_a)["stale"], true);
}

#[test]
fn records_explicit_cross_worktree_handoffs_and_rejects_conflicting_claims() {
    let (_temporary, repository, sibling_a, _sibling_b) = fixture();
    let listing = worktree::list(&repository).unwrap();
    let current = listing
        .worktrees
        .iter()
        .find(|item| item.current)
        .unwrap()
        .provenance
        .clone();
    worktree::claim(&current, ClaimKind::Session, "first-session").unwrap();
    let conflict = worktree::claim(&current, ClaimKind::Session, "second-session").unwrap_err();
    assert!(
        conflict
            .to_string()
            .contains("already has an active session claim")
    );
    worktree::release(&current, ClaimKind::Session, "first-session").unwrap();

    assert!(
        relay(&repository, &["start", "handoff-source"])
            .status
            .success()
    );
    let session: Value =
        serde_json::from_slice(&fs::read(repository.join(".relay/session.json")).unwrap()).unwrap();
    let active_list = json(&relay(&repository, &["--json", "worktree", "list"]));
    let active = worktree(&active_list, &repository);
    assert_eq!(active["active"]["session"], session["id"]);
    assert!(
        active["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &session["id"])
    );
    let output = relay_owned(
        &repository,
        vec![
            "--json".into(),
            "handoff".into(),
            "agent".into(),
            "--target-worktree".into(),
            sibling_a.to_string_lossy().into_owned(),
        ],
    );
    let record = json(&output)["data"]["handoff"].clone();
    assert_eq!(
        record["source_worktree"]["worktree_path"],
        repository.to_string_lossy().as_ref()
    );
    assert_eq!(
        record["target_worktree"]["worktree_path"],
        sibling_a.to_string_lossy().as_ref()
    );
    assert_eq!(
        record["source_worktree"]["repository_id"],
        record["target_worktree"]["repository_id"]
    );
    let after_handoff = json(&relay(&repository, &["--json", "worktree", "list"]));
    assert!(
        !worktree(&after_handoff, &repository)["handoffs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn rejects_malformed_and_symlinked_coordination_metadata() {
    let (_temporary, repository, _sibling_a, _sibling_b) = fixture();
    let first = json(&relay(&repository, &["--json", "worktree", "list"]));
    let history = PathBuf::from(first["data"]["repository_id"].as_str().unwrap())
        .join("relay-worktree-history.json");
    fs::write(&history, b"{not-json\n").unwrap();
    let malformed = relay(&repository, &["--json", "worktree", "list"]);
    assert!(!malformed.status.success());
    assert!(String::from_utf8_lossy(&malformed.stderr).contains("malformed"));
    fs::remove_file(&history).unwrap();

    let symlink_target = tempfile::tempdir().unwrap();
    fs::create_dir_all(symlink_target.path().join("relay-state")).unwrap();
    std::os::unix::fs::symlink(
        symlink_target.path().join("relay-state"),
        repository.join(".relay"),
    )
    .unwrap();
    let symlinked = relay(&repository, &["--json", "worktree", "list"]);
    assert!(!symlinked.status.success());
    assert!(String::from_utf8_lossy(&symlinked.stderr).contains("symlink"));
}

#[test]
fn persists_run_provenance_when_real_supervisor_is_available() {
    let Some(supervisor) = std::env::var_os("RELAY_SUPERVISOR_E2E_BIN").map(PathBuf::from) else {
        return;
    };
    let (_temporary, repository, _sibling_a, _sibling_b) = fixture();
    let plan = repository.join("plan.json");
    fs::write(
        &plan,
        serde_json::json!({
            "schema_version": 1,
            "tasks": [{
                "id": "one",
                "depends_on": [],
                "program": "/bin/true",
                "arguments": [],
                "working_directory": repository,
                "timeout_ms": 5000,
                "termination_grace_ms": 1000,
                "stdout_limit_bytes": 1024,
                "stderr_limit_bytes": 1024,
                "log_limit_bytes": 1024,
                "redact_argument_indexes": []
            }]
        })
        .to_string(),
    )
    .unwrap();
    let state = repository.join(".relay/runs/worktree-run.json");
    let output = relay_owned(
        &repository,
        vec![
            "--json".into(),
            "run".into(),
            "--supervisor".into(),
            supervisor.to_string_lossy().into_owned(),
            "--state-path".into(),
            state.to_string_lossy().into_owned(),
            plan.to_string_lossy().into_owned(),
        ],
    );
    let document = json(&output);
    assert_eq!(document["data"]["run"]["status"], "completed");
    let persisted: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    assert_eq!(
        persisted["worktree"]["worktree_path"],
        repository.to_string_lossy().as_ref()
    );
}
