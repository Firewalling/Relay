#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const RUN_ID: &str = "supervise-test-1";

fn relay_at(binary: &Path, project: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(binary);
    command.arg("--json").args(args).current_dir(project);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run relay")
}

fn relay(project: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    relay_at(Path::new(env!("CARGO_BIN_EXE_relay")), project, args, env)
}

fn write_supervisor(directory: &TempDir) -> PathBuf {
    let path = directory.path().join("fake-supervisor");
    fs::write(
        &path,
        "#!/bin/sh\ncat > \"$RELAY_TEST_REQUEST\"\nif [ \"${RELAY_TEST_STREAM:-stdout}\" = stderr ]; then\n  printf '%s\\n' \"$RELAY_TEST_RESPONSE\" >&2\nelse\n  printf '%s\\n' \"$RELAY_TEST_RESPONSE\"\nfi\nexit \"${RELAY_TEST_EXIT:-0}\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn execution(run_id: &str, lifecycle: &str, reason: &str, exit_code: Option<i32>) -> Value {
    json!({
        "run_id": run_id,
        "protocol_version": 1,
        "supervisor_version": "test-supervisor",
        "program": "/bin/true",
        "arguments": [],
        "environment": {
            "inherited_allowlist": [], "effective": [], "blocked_inherited_count": 0,
            "duplicate_user_overrides": 0
        },
        "accepted_at": "2026-01-01T00:00:00Z",
        "started_at": "2026-01-01T00:00:00Z",
        "completed_at": "2026-01-01T00:00:00Z",
        "duration_ms": 1,
        "lifecycle": lifecycle,
        "termination_reason": reason,
        "exit_code": exit_code,
        "failure": null,
        "timeout": {"configured_ms": 0, "triggered": lifecycle == "timed_out"},
        "cancellation": {"requested": lifecycle == "cancelled", "source": if lifecycle == "cancelled" { "signal" } else { "" }},
        "termination": {"grace_ms": 2000, "graceful_attempted": false, "escalated": false},
        "stdout": {"text": "kept stdout", "bytes_observed": 11, "bytes_retained": 11, "truncated": false, "first_sequence": null, "last_sequence": null},
        "stderr": {"text": "kept stderr", "bytes_observed": 11, "bytes_retained": 11, "truncated": false, "first_sequence": null, "last_sequence": null},
        "lifecycle_history": [],
        "output_observations": {"limit_bytes": 65536, "bytes_used": 0, "truncated": false, "entries": []}
    })
}

fn success_response(run_id: &str, lifecycle: &str, reason: &str, exit_code: Option<i32>) -> String {
    json!({"output_schema_version": 1, "ok": true, "command": "supervise", "data": execution(run_id, lifecycle, reason, exit_code)}).to_string()
}

fn fake_env<'a>(request: &'a Path, response: &'a str) -> [(&'a str, &'a str); 3] {
    [
        ("RELAY_TEST_REQUEST", request.to_str().unwrap()),
        ("RELAY_TEST_RESPONSE", response),
        ("RELAY_TEST_STREAM", "stdout"),
    ]
}

fn success_data(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "relay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["output_schema_version"], 1);
    assert_eq!(document["ok"], true);
    assert_eq!(document["command"], "supervise");
    document["data"].clone()
}

#[test]
fn fake_supervisor_preserves_request_and_execution_outcomes_without_relay_state() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");
    let state = directory.path().join("go-state.json");
    let literal = "$(touch should-not-run);*; space ' quote \"";
    let response = success_response(RUN_ID, "completed", "exit_nonzero", Some(17));
    let output = relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--state-path",
            state.to_str().unwrap(),
            "--working-directory",
            directory.path().to_str().unwrap(),
            "--timeout-ms",
            "123",
            "--termination-grace-ms",
            "456",
            "--stdout-limit-bytes",
            "789",
            "--stderr-limit-bytes",
            "790",
            "--log-limit-bytes",
            "791",
            "--redact-argument-index",
            "1",
            "--",
            "/bin/echo",
            "ordinary",
            literal,
        ],
        &fake_env(&request, &response),
    );
    let data = success_data(&output);
    assert_eq!(data["execution"]["lifecycle"], "completed");
    assert_eq!(data["execution"]["termination_reason"], "exit_nonzero");
    assert_eq!(data["execution"]["exit_code"], 17);
    assert_eq!(data["state_path"], state.to_str().unwrap());

    let captured: Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
    assert_eq!(captured["protocol_version"], 1);
    assert_eq!(captured["run_id"], RUN_ID);
    assert_eq!(captured["program"], "/bin/echo");
    assert_eq!(captured["arguments"], json!(["ordinary", literal]));
    assert_eq!(captured["redact_argument_indexes"], json!([1]));
    assert_eq!(
        captured["working_directory"],
        directory.path().to_str().unwrap()
    );
    assert_eq!(captured["environment"], json!([]));
    assert_eq!(captured["timeout_ms"], 123);
    assert_eq!(captured["termination_grace_ms"], 456);
    assert_eq!(captured["stdout_limit_bytes"], 789);
    assert_eq!(captured["stderr_limit_bytes"], 790);
    assert_eq!(captured["log_limit_bytes"], 791);
    assert_eq!(captured["state_path"], state.to_str().unwrap());
    assert!(!directory.path().join("should-not-run").exists());
    assert!(!directory.path().join(".relay").exists());

    for (lifecycle, reason) in [
        ("timed_out", "timeout_terminated"),
        ("cancelled", "cancelled_terminated"),
    ] {
        let response = success_response(RUN_ID, lifecycle, reason, None);
        let output = relay(
            directory.path(),
            &[
                "supervise",
                "--supervisor",
                supervisor.to_str().unwrap(),
                "--run-id",
                RUN_ID,
                "--",
                "/bin/true",
            ],
            &fake_env(&request, &response),
        );
        let captured: Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
        assert_eq!(
            captured["state_path"],
            std::env::temp_dir()
                .join("relay-runs")
                .join(format!("{RUN_ID}.json"))
                .to_str()
                .unwrap()
        );
        let data = success_data(&output);
        assert_eq!(data["execution"]["lifecycle"], lifecycle);
        assert_eq!(data["execution"]["termination_reason"], reason);
        assert!(!directory.path().join(".relay").exists());
    }
}

#[test]
fn fake_supervisor_requires_a_strict_matching_success_envelope() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");
    let valid = success_response(RUN_ID, "completed", "exit_zero", Some(0));
    let cases = [
        "not json".to_owned(),
        format!("{valid} trailing"),
        format!("{valid}\n{valid}"),
        valid.replacen(
            "\"output_schema_version\":1",
            "\"output_schema_version\":2",
            1,
        ),
        valid.replacen("\"command\":\"supervise\"", "\"command\":\"wrong\"", 1),
        valid.replacen("\"protocol_version\":1", "\"protocol_version\":2", 1),
        valid.replacen(RUN_ID, "different-run", 1),
        valid.replacen(
            "\"lifecycle\":\"completed\"",
            "\"lifecycle\":\"unknown\"",
            1,
        ),
        valid.replacen(
            "\"lifecycle\":\"completed\"",
            "\"lifecycle\":\"running\"",
            1,
        ),
        valid.replacen(
            "\"termination_reason\":\"exit_zero\"",
            "\"termination_reason\":\"unknown\"",
            1,
        ),
        valid.replacen(
            "\"termination_reason\":\"exit_zero\"",
            "\"termination_reason\":\"timeout_killed\"",
            1,
        ),
        valid.replacen(
            "\"supervisor_version\":\"test-supervisor\"",
            "\"supervisor_version\":\"\"",
            1,
        ),
        success_response(RUN_ID, "protocol_failure", "protocol_failure", None),
    ];
    for response in cases {
        let output = relay(
            directory.path(),
            &[
                "supervise",
                "--supervisor",
                supervisor.to_str().unwrap(),
                "--run-id",
                RUN_ID,
                "--",
                "/bin/true",
            ],
            &fake_env(&request, &response),
        );
        assert!(
            !output.status.success(),
            "accepted invalid response {response}"
        );
        assert!(!directory.path().join(".relay").exists());
    }

    let error = json!({"output_schema_version": 1, "ok": false, "command": "supervise", "error": {"code": "remote_failure", "message": "deliberate"}}).to_string();
    let output = relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--",
            "/bin/true",
        ],
        &[
            ("RELAY_TEST_REQUEST", request.to_str().unwrap()),
            ("RELAY_TEST_RESPONSE", &error),
            ("RELAY_TEST_STREAM", "stderr"),
            ("RELAY_TEST_EXIT", "1"),
        ],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let document: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(document["command"], "supervise");
    assert_eq!(document["error"]["code"], "remote_failure");
    assert!(
        document["error"]["message"]
            .as_str()
            .unwrap()
            .contains("deliberate")
    );
    assert!(!directory.path().join(".relay").exists());
}

#[test]
fn supervisor_discovery_and_local_validation_are_safe() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");
    let response = success_response(RUN_ID, "completed", "exit_zero", Some(0));
    let env = fake_env(&request, &response);

    let explicit = relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--",
            "/bin/true",
        ],
        &[
            ("RELAY_SUPERVISOR", "/definitely/missing"),
            env[0],
            env[1],
            env[2],
        ],
    );
    success_data(&explicit);
    let discovered = relay(
        directory.path(),
        &["supervise", "--run-id", RUN_ID, "--", "/bin/true"],
        &[
            ("RELAY_SUPERVISOR", supervisor.to_str().unwrap()),
            env[0],
            env[1],
            env[2],
        ],
    );
    success_data(&discovered);
    let sibling_relay = directory.path().join("relay");
    let sibling_supervisor = directory.path().join("relay-supervisor");
    fs::copy(env!("CARGO_BIN_EXE_relay"), &sibling_relay).unwrap();
    fs::set_permissions(&sibling_relay, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&supervisor, &sibling_supervisor).unwrap();
    let sibling = relay_at(
        &sibling_relay,
        directory.path(),
        &["supervise", "--run-id", RUN_ID, "--", "/bin/true"],
        &env,
    );
    success_data(&sibling);
    let relative = relay(
        directory.path(),
        &["supervise", "--run-id", RUN_ID, "--", "/bin/true"],
        &[("RELAY_SUPERVISOR", "relay-supervisor"), env[0], env[1]],
    );
    assert!(!relative.status.success());
    let missing = relay(
        directory.path(),
        &["supervise", "--run-id", RUN_ID, "--", "/bin/true"],
        &[(
            "RELAY_SUPERVISOR",
            directory
                .path()
                .join("missing-supervisor")
                .to_str()
                .unwrap(),
        )],
    );
    assert!(!missing.status.success());
    for args in [
        vec![
            "supervise",
            "--supervisor",
            "relay-supervisor",
            "--",
            "/bin/true",
        ],
        vec![
            "supervise",
            "--supervisor",
            sibling_supervisor.to_str().unwrap(),
            "--run-id",
            "bad id",
            "--",
            "/bin/true",
        ],
        vec![
            "supervise",
            "--supervisor",
            sibling_supervisor.to_str().unwrap(),
            "--",
            "relative-program",
        ],
        vec![
            "supervise",
            "--supervisor",
            sibling_supervisor.to_str().unwrap(),
            "--redact-argument-index",
            "1",
            "--",
            "/bin/true",
        ],
        vec![
            "supervise",
            "--supervisor",
            sibling_supervisor.to_str().unwrap(),
            "--state-path",
            "/tmp/.relay/state.json",
            "--",
            "/bin/true",
        ],
    ] {
        let output = relay(directory.path(), &args, &env);
        assert!(
            !output.status.success(),
            "accepted invalid invocation: {args:?}"
        );
    }
    assert!(!directory.path().join(".relay").exists());
    let rust_state = directory.path().join(".relay");
    fs::create_dir(&rust_state).unwrap();
    let sentinel = rust_state.join("sentinel");
    fs::write(&sentinel, "unchanged").unwrap();
    let output = relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            sibling_supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--",
            "/bin/true",
        ],
        &env,
    );
    success_data(&output);
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
}

#[test]
fn real_go_supervisor_e2e_when_configured() {
    let Some(supervisor) = std::env::var_os("RELAY_SUPERVISOR_E2E_BIN") else {
        return;
    };
    let supervisor = PathBuf::from(supervisor);
    assert!(
        supervisor.is_absolute(),
        "RELAY_SUPERVISOR_E2E_BIN must be absolute"
    );
    assert!(
        supervisor.is_file(),
        "RELAY_SUPERVISOR_E2E_BIN must name a file"
    );

    let directory = tempfile::tempdir().unwrap();
    let literal = "$(not-a-shell); * ; 'quoted'";
    let output = relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            "go-success",
            "--",
            "/usr/bin/printf",
            "%s",
            literal,
        ],
        &[],
    );
    let data = success_data(&output);
    assert_eq!(data["execution"]["lifecycle"], "completed");
    assert_eq!(data["execution"]["exit_code"], 0);
    assert_eq!(data["execution"]["stdout"]["text"], literal);

    let output = relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            "go-nonzero",
            "--",
            "/bin/false",
        ],
        &[],
    );
    let data = success_data(&output);
    assert_eq!(data["execution"]["lifecycle"], "completed");
    assert_eq!(data["execution"]["termination_reason"], "exit_nonzero");
    assert_eq!(data["execution"]["exit_code"], 1);

    let output = relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            "go-timeout",
            "--timeout-ms",
            "20",
            "--",
            "/bin/sleep",
            "10",
        ],
        &[],
    );
    let data = success_data(&output);
    assert_eq!(data["execution"]["lifecycle"], "timed_out");

    let state = directory.path().join("go-cancel-state.json");
    let child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args([
            "--json",
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            "go-cancel",
            "--state-path",
            state.to_str().unwrap(),
            "--",
            "/bin/sleep",
            "10",
        ])
        .current_dir(directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(bytes) = fs::read(&state)
            && let Ok(record) = serde_json::from_slice::<Value>(&bytes)
            && record["execution"]["lifecycle"] == "running"
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Go supervisor never persisted running state"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let output = child.wait_with_output().unwrap();
    let data = success_data(&output);
    assert_eq!(data["execution"]["lifecycle"], "cancelled");
    let record: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    assert_eq!(record["execution"]["lifecycle"], "cancelled");
    assert!(!directory.path().join(".relay").exists());
}
