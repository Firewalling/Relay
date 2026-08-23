#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, process::CommandExt},
    },
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
        "#!/bin/sh\nready_fd=$RELAY_SUPERVISOR_READY_FD\neval \"printf R >&$ready_fd\"\nack=$(eval \"dd bs=1 count=1 <&$ready_fd\" 2>/dev/null)\n[ \"$ack\" = A ] || exit 92\neval \"exec $ready_fd>&-\"\ncat > \"$RELAY_TEST_REQUEST\"\nif [ \"${RELAY_TEST_STREAM:-stdout}\" = stderr ]; then\n  printf '%s\\n' \"$RELAY_TEST_RESPONSE\" >&2\nelse\n  printf '%s\\n' \"$RELAY_TEST_RESPONSE\"\nfi\nexit \"${RELAY_TEST_EXIT:-0}\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn write_legacy_supervisor(directory: &TempDir) -> PathBuf {
    let path = directory.path().join("fake-legacy-supervisor");
    fs::write(
        &path,
        r#"#!/usr/bin/python3
import json
import os
import signal
import sys
import time

signalled = False
def release(_signal, _frame):
    global signalled
    signalled = True

signal.signal(signal.SIGINT, release)
signal.signal(signal.SIGTERM, release)
request = json.load(sys.stdin)
response = json.loads(os.environ["RELAY_TEST_RESPONSE"])
state_path = request["state_path"]
os.makedirs(os.path.dirname(state_path), exist_ok=True)
with open(os.environ["RELAY_TEST_STARTED"], "w", encoding="utf-8") as marker:
    marker.write("request-read\n")
replace_release = os.environ.get("RELAY_TEST_REPLACE_RELEASE")
if replace_release is not None:
    while not os.path.exists(replace_release):
        if signalled:
            with open(os.environ["RELAY_TEST_EARLY_SIGNAL"], "w", encoding="utf-8") as marker:
                marker.write("stale-state-spoofed-readiness\n")
        time.sleep(0.01)
temporary = state_path + ".legacy.tmp"
with open(temporary, "w", encoding="utf-8") as destination:
    json.dump({
        "run_state_version": 1,
        "supervisor_version": response["data"]["supervisor_version"],
        "execution": response["data"],
    }, destination, separators=(",", ":"))
os.replace(temporary, state_path)
if os.environ.get("RELAY_TEST_AUTO_RESPONSE") != "1":
    while not signalled:
        time.sleep(0.01)
    with open(os.environ["RELAY_TEST_SIGNALLED"], "w", encoding="utf-8") as marker:
        marker.write("signal-forwarded\n")
sys.stdout.write(json.dumps(response, separators=(",", ":")) + "\n")
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(target_os = "linux")]
fn write_descriptor_probe_supervisor(directory: &TempDir) -> PathBuf {
    let path = directory.path().join("descriptor-probe-supervisor");
    fs::write(
        &path,
        r#"#!/usr/bin/python3
import errno
import json
import os
import sys

ready_fd = int(os.environ["RELAY_SUPERVISOR_READY_FD"])
if ready_fd < 3:
    sys.stderr.write("readiness descriptor was below 3\n")
    sys.exit(90)
ambient_fd = int(os.environ["RELAY_TEST_AMBIENT_FD"])
try:
    os.fstat(ambient_fd)
except OSError as error:
    if error.errno != errno.EBADF:
        raise
else:
    sys.stderr.write("unrelated inheritable descriptor leaked\n")
    sys.exit(91)
os.write(ready_fd, b"R")
if os.read(ready_fd, 2) != b"A":
    sys.stderr.write("invalid readiness acknowledgement\n")
    sys.exit(92)
os.close(ready_fd)
json.load(sys.stdin)
sys.stdout.write(os.environ["RELAY_TEST_RESPONSE"] + "\n")
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn execution(run_id: &str, lifecycle: &str, reason: &str, exit_code: Option<i32>) -> Value {
    let fixture = match lifecycle {
        "timed_out" => include_str!("../protocol/fixtures/supervisor-v1/response-timeout.json"),
        "cancelled" => {
            include_str!("../protocol/fixtures/supervisor-v1/response-cancellation.json")
        }
        "failed_to_start" => {
            include_str!("../protocol/fixtures/supervisor-v1/response-start-failure.json")
        }
        _ => include_str!("../protocol/fixtures/supervisor-v1/response-success.json"),
    };
    let envelope: Value = serde_json::from_str(fixture).expect("canonical supervisor fixture");
    let mut execution = envelope["data"].clone();
    execution["run_id"] = json!(run_id);
    execution["lifecycle"] = json!(lifecycle);
    execution["termination_reason"] = json!(reason);
    execution["exit_code"] = json!(exit_code);
    let final_event = execution["lifecycle_history"]
        .as_array_mut()
        .expect("canonical lifecycle history")
        .last_mut()
        .expect("canonical final lifecycle event");
    final_event["lifecycle"] = json!(lifecycle);
    execution
}

fn success_response(run_id: &str, lifecycle: &str, reason: &str, exit_code: Option<i32>) -> String {
    let mut execution = execution(run_id, lifecycle, reason, exit_code);
    execution["program"] = json!("/bin/true");
    execution["arguments"] = json!([]);
    execution["timeout"]["configured_ms"] = json!(0);
    execution["termination"]["grace_ms"] = json!(2_000);
    execution["output_observations"]["limit_bytes"] = json!(64 << 10);
    response_with_execution(execution)
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

fn run_fake_response(
    directory: &TempDir,
    supervisor: &Path,
    request: &Path,
    response: &str,
) -> Output {
    relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--timeout-ms",
            "5000",
            "--termination-grace-ms",
            "250",
            "--log-limit-bytes",
            "4096",
            "--",
            "/usr/bin/printf",
            "%s",
            "literal;$(not-a-shell)",
        ],
        &fake_env(request, response),
    )
}

fn assert_invalid_response(output: &Output, case: &str) {
    assert!(
        !output.status.success(),
        "accepted invalid supervisor response: {case}"
    );
    assert!(
        output.stdout.is_empty(),
        "invalid response emitted a success document: {case}"
    );
    let document: Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("invalid error document for {case}: {error}"));
    assert_eq!(document["output_schema_version"], 1, "{case}");
    assert_eq!(document["ok"], false, "{case}");
    assert_eq!(document["command"], "supervise", "{case}");
    assert_eq!(
        document["error"]["code"], "supervisor_invalid_response",
        "{case}"
    );
}

fn response_with_execution(execution: Value) -> String {
    json!({
        "output_schema_version": 1,
        "ok": true,
        "command": "supervise",
        "data": execution,
    })
    .to_string()
}

#[test]
fn fake_supervisor_preserves_request_and_execution_outcomes_without_relay_state() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");
    let state = directory.path().join("go-state.json");
    let literal = "$(touch should-not-run);*; space ' quote \"";
    let mut custom_execution = execution(RUN_ID, "completed", "exit_nonzero", Some(17));
    custom_execution["program"] = json!("/bin/echo");
    custom_execution["arguments"] = json!(["ordinary", "<redacted>"]);
    custom_execution["timeout"]["configured_ms"] = json!(123);
    custom_execution["termination"]["grace_ms"] = json!(456);
    custom_execution["output_observations"]["limit_bytes"] = json!(791);
    let response = response_with_execution(custom_execution);
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

    for (lifecycle, reason, exit_code) in [
        ("timed_out", "timeout_terminated", None),
        ("cancelled", "cancelled_terminated", Some(0)),
        ("failed_to_start", "start_error", None),
    ] {
        let response = response_with_execution(execution(RUN_ID, lifecycle, reason, exit_code));
        let (program, arguments, timeout_ms) = match lifecycle {
            "timed_out" => ("/usr/bin/sleep", vec!["10"], "100"),
            "cancelled" => ("/usr/bin/worker", vec![], "0"),
            "failed_to_start" => ("/missing/relay-child", vec![], "0"),
            _ => unreachable!(),
        };
        let mut command_args = vec![
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--timeout-ms",
            timeout_ms,
            "--termination-grace-ms",
            "250",
            "--log-limit-bytes",
            "4096",
            "--",
            program,
        ];
        command_args.extend(arguments);
        let output = relay(
            directory.path(),
            &command_args,
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
fn current_handshake_does_not_block_on_a_near_limit_request() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = directory.path().join("large-request-supervisor");
    fs::write(
        &supervisor,
        r#"#!/usr/bin/python3
import json
import os
import sys

fd = int(os.environ["RELAY_SUPERVISOR_READY_FD"])
os.write(fd, b"R")
ack = os.read(fd, 2)
eof = os.read(fd, 1)
os.close(fd)
if ack != b"A" or eof != b"":
    sys.exit(92)
request_bytes = sys.stdin.buffer.read()
json.loads(request_bytes)
if len(request_bytes) < 800_000:
    sys.exit(93)
error = {
    "output_schema_version": 1,
    "ok": False,
    "command": "supervise",
    "error": {
        "code": "fake_large_request",
        "message": "near-limit request crossed the readiness barrier",
    },
}
sys.stderr.write(json.dumps(error, separators=(",", ":")) + "\n")
sys.exit(7)
"#,
    )
    .unwrap();
    fs::set_permissions(&supervisor, fs::Permissions::from_mode(0o755)).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_relay"));
    command
        .current_dir(directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args([
            "--json",
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            "large-request-barrier",
            "--timeout-ms",
            "0",
            "--termination-grace-ms",
            "2000",
            "--stdout-limit-bytes",
            "65536",
            "--stderr-limit-bytes",
            "65536",
            "--log-limit-bytes",
            "65536",
            "--",
            "/bin/true",
        ]);
    let argument = "x".repeat(16 * 1024);
    for index in 0..56 {
        command.arg(format!("{index:02}-{argument}"));
    }
    let mut child = command
        .spawn()
        .expect("spawn Relay with near-limit request");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("poll Relay").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("near-limit request deadlocked before readiness acknowledgement");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("collect Relay output");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fake_large_request"),
        "unexpected Relay error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fake_supervisor_accepts_canonical_null_go_slices_as_empty() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");
    let mut result = execution(RUN_ID, "cancelled", "cancelled_terminated", None);
    result["arguments"] = Value::Null;
    result["started_at"] = result["accepted_at"].clone();
    result["duration_ms"] = json!(210);
    result["termination"]["graceful_attempted"] = json!(false);
    let completed_at = result["completed_at"].clone();
    result["lifecycle_history"] = json!([
        {
            "sequence": 1,
            "lifecycle": "accepted",
            "timestamp": result["accepted_at"].clone(),
        },
        {
            "sequence": 2,
            "lifecycle": "cancelled",
            "timestamp": completed_at,
        },
    ]);
    result["output_observations"]["entries"] = Value::Null;
    let response = response_with_execution(result);
    let output = relay(
        directory.path(),
        &[
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--timeout-ms",
            "0",
            "--termination-grace-ms",
            "250",
            "--log-limit-bytes",
            "4096",
            "--",
            "/usr/bin/worker",
        ],
        &fake_env(&request, &response),
    );
    let data = success_data(&output);
    assert_eq!(data["execution"]["exit_code"], Value::Null);
    assert_eq!(data["execution"]["failure"], Value::Null);
    assert_eq!(data["execution"]["stdout"]["first_sequence"], Value::Null);
    assert_eq!(data["execution"]["stdout"]["last_sequence"], Value::Null);
    assert_eq!(data["execution"]["stderr"]["first_sequence"], Value::Null);
    assert_eq!(data["execution"]["stderr"]["last_sequence"], Value::Null);
    assert_eq!(data["execution"]["arguments"], json!([]));
    assert_eq!(
        data["execution"]["output_observations"]["entries"],
        json!([])
    );
}

#[test]
fn fake_supervisor_requires_a_strict_matching_success_envelope() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");
    let valid = success_response(RUN_ID, "completed", "exit_zero", Some(0));
    let mut wrong_schema: Value = serde_json::from_str(&valid).unwrap();
    wrong_schema["output_schema_version"] = json!(2);
    let mut wrong_command: Value = serde_json::from_str(&valid).unwrap();
    wrong_command["command"] = json!("wrong");
    let mut unknown_field: Value = serde_json::from_str(&valid).unwrap();
    unknown_field
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), json!(true));
    let cases = [
        ("not JSON", "not json".to_owned()),
        ("trailing text", format!("{valid} trailing")),
        ("two documents", format!("{valid}\n{valid}")),
        ("wrong output schema", wrong_schema.to_string()),
        ("wrong command", wrong_command.to_string()),
        ("unknown envelope field", unknown_field.to_string()),
    ];
    for (case, response) in cases {
        let output = run_fake_response(&directory, &supervisor, &request, &response);
        assert_invalid_response(&output, case);
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
fn fake_supervisor_accepts_nullable_nonzero_exit_codes_but_requires_the_field() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");

    for exit_code in [Value::Null, json!(17), json!(-15)] {
        let mut valid = execution(RUN_ID, "completed", "exit_nonzero", None);
        valid["exit_code"] = exit_code.clone();
        let output = run_fake_response(
            &directory,
            &supervisor,
            &request,
            &response_with_execution(valid),
        );
        let data = success_data(&output);
        assert_eq!(data["execution"]["termination_reason"], "exit_nonzero");
        assert_eq!(data["execution"]["exit_code"], exit_code);
    }

    let mut zero = execution(RUN_ID, "completed", "exit_nonzero", None);
    zero["exit_code"] = json!(0);
    let output = run_fake_response(
        &directory,
        &supervisor,
        &request,
        &response_with_execution(zero),
    );
    assert_invalid_response(&output, "exit_nonzero with zero exit code");

    let mut missing = execution(RUN_ID, "completed", "exit_nonzero", None);
    missing
        .as_object_mut()
        .unwrap()
        .remove("exit_code")
        .unwrap();
    let output = run_fake_response(
        &directory,
        &supervisor,
        &request,
        &response_with_execution(missing),
    );
    assert_invalid_response(&output, "exit_nonzero without exit_code");
}

#[test]
fn truncated_observation_log_rejects_lifecycle_stream_endpoint_collisions() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");

    let mut legal_gaps = execution(RUN_ID, "completed", "exit_zero", Some(0));
    legal_gaps["lifecycle_history"][2]["sequence"] = json!(5);
    legal_gaps["lifecycle_history"][3]["sequence"] = json!(7);
    legal_gaps["stdout"]["first_sequence"] = json!(3);
    legal_gaps["stdout"]["last_sequence"] = json!(6);
    legal_gaps["output_observations"]["bytes_used"] = json!(0);
    legal_gaps["output_observations"]["truncated"] = json!(true);
    legal_gaps["output_observations"]["entries"] = json!([]);

    let output = run_fake_response(
        &directory,
        &supervisor,
        &request,
        &response_with_execution(legal_gaps.clone()),
    );
    success_data(&output);

    for (case, first, last) in [
        ("stdout first sequence collision", 5, 6),
        ("stdout last sequence collision", 3, 5),
    ] {
        let mut collision = legal_gaps.clone();
        collision["stdout"]["first_sequence"] = json!(first);
        collision["stdout"]["last_sequence"] = json!(last);
        let output = run_fake_response(
            &directory,
            &supervisor,
            &request,
            &response_with_execution(collision),
        );
        assert_invalid_response(&output, case);
    }
}

#[test]
fn fake_supervisor_strictly_decodes_and_validates_the_complete_execution_result() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");

    let canonical = execution(RUN_ID, "completed", "exit_zero", Some(0));
    let output = run_fake_response(
        &directory,
        &supervisor,
        &request,
        &response_with_execution(canonical.clone()),
    );
    let data = success_data(&output);
    assert_eq!(data["execution"], canonical);

    for (case, pointer, replacement) in [
        ("mismatched run ID", "/run_id", json!("different-run")),
        (
            "unsupported protocol version",
            "/protocol_version",
            json!(2),
        ),
        ("empty supervisor version", "/supervisor_version", json!("")),
        ("empty program", "/program", json!("")),
        (
            "invalid accepted timestamp",
            "/accepted_at",
            json!("not-a-time"),
        ),
        (
            "started before acceptance",
            "/started_at",
            json!("2026-01-02T03:04:04Z"),
        ),
        (
            "completed before start",
            "/completed_at",
            json!("2026-01-02T03:04:05Z"),
        ),
        ("negative duration", "/duration_ms", json!(-1)),
        ("non-final lifecycle", "/lifecycle", json!("running")),
        (
            "incompatible lifecycle and reason",
            "/termination_reason",
            json!("timeout_terminated"),
        ),
        ("exit-zero with nonzero code", "/exit_code", json!(1)),
        (
            "exit-nonzero with zero code",
            "/termination_reason",
            json!("exit_nonzero"),
        ),
        (
            "completed execution with failure",
            "/failure",
            json!({"code": "unexpected", "message": "unexpected"}),
        ),
        ("negative timeout", "/timeout/configured_ms", json!(-1)),
        (
            "timeout triggered for normal completion",
            "/timeout/triggered",
            json!(true),
        ),
        (
            "cancellation source without request",
            "/cancellation/source",
            json!("signal"),
        ),
        (
            "cancellation request without source",
            "/cancellation/requested",
            json!(true),
        ),
        (
            "negative termination grace",
            "/termination/grace_ms",
            json!(-1),
        ),
        (
            "escalation without graceful attempt",
            "/termination/escalated",
            json!(true),
        ),
        (
            "graceful attempt for normal completion",
            "/termination/graceful_attempted",
            json!(true),
        ),
        (
            "negative stdout observed bytes",
            "/stdout/bytes_observed",
            json!(-1),
        ),
        (
            "negative stdout retained bytes",
            "/stdout/bytes_retained",
            json!(-1),
        ),
        (
            "stdout retains more than observed",
            "/stdout/bytes_retained",
            json!(23),
        ),
        (
            "stdout truncation disagrees with counts",
            "/stdout/truncated",
            json!(true),
        ),
        (
            "stdout has only a last sequence",
            "/stdout/first_sequence",
            Value::Null,
        ),
        (
            "stdout has only a first sequence",
            "/stdout/last_sequence",
            Value::Null,
        ),
        (
            "stdout sequence range is reversed",
            "/stdout/first_sequence",
            json!(5),
        ),
        (
            "empty stderr has a sequence",
            "/stderr/first_sequence",
            json!(4),
        ),
        (
            "history does not begin accepted",
            "/lifecycle_history/0/lifecycle",
            json!("starting"),
        ),
        (
            "history sequence is not increasing",
            "/lifecycle_history/1/sequence",
            json!(1),
        ),
        (
            "history timestamp is invalid",
            "/lifecycle_history/1/timestamp",
            json!("not-a-time"),
        ),
        (
            "history timestamp moves backward",
            "/lifecycle_history/2/timestamp",
            json!("2026-01-02T03:04:04Z"),
        ),
        (
            "history timestamp repeats",
            "/lifecycle_history/2/timestamp",
            json!("2026-01-02T03:04:05.01Z"),
        ),
        (
            "history final lifecycle disagrees",
            "/lifecycle_history/3/lifecycle",
            json!("timed_out"),
        ),
        (
            "negative observation limit",
            "/output_observations/limit_bytes",
            json!(-1),
        ),
        (
            "negative observation accounting",
            "/output_observations/bytes_used",
            json!(-1),
        ),
        (
            "observation accounting exceeds limit",
            "/output_observations/bytes_used",
            json!(4097),
        ),
        (
            "observation accounting disagrees with entries",
            "/output_observations/bytes_used",
            json!(58),
        ),
        (
            "observation log marked truncated despite being complete",
            "/output_observations/truncated",
            json!(true),
        ),
        (
            "observation sequence is zero",
            "/output_observations/entries/0/sequence",
            json!(0),
        ),
        (
            "unknown observation stream",
            "/output_observations/entries/0/stream",
            json!("stdin"),
        ),
        (
            "empty output observation",
            "/output_observations/entries/0/bytes",
            json!(0),
        ),
        (
            "observation bytes disagree with stream",
            "/output_observations/entries/0/bytes",
            json!(23),
        ),
        (
            "observation sequence collides with history",
            "/output_observations/entries/0/sequence",
            json!(3),
        ),
    ] {
        let mut invalid = canonical.clone();
        *invalid
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("canonical execution is missing {pointer}")) = replacement;
        let response = response_with_execution(invalid);
        let output = run_fake_response(&directory, &supervisor, &request, &response);
        assert_invalid_response(&output, case);
    }

    for (case, pointer) in [
        ("missing arguments", "/arguments"),
        ("missing exit code", "/exit_code"),
        ("missing failure", "/failure"),
        ("missing stdout first sequence", "/stdout/first_sequence"),
        ("missing stdout last sequence", "/stdout/last_sequence"),
        ("missing stderr first sequence", "/stderr/first_sequence"),
        ("missing stderr last sequence", "/stderr/last_sequence"),
        ("missing timeout trigger", "/timeout/triggered"),
        ("missing lifecycle history", "/lifecycle_history"),
        (
            "missing observation entries",
            "/output_observations/entries",
        ),
    ] {
        let mut invalid = canonical.clone();
        let (parent, field) = pointer.rsplit_once('/').unwrap();
        invalid
            .pointer_mut(parent)
            .and_then(Value::as_object_mut)
            .unwrap()
            .remove(field)
            .unwrap();
        let response = response_with_execution(invalid);
        let output = run_fake_response(&directory, &supervisor, &request, &response);
        assert_invalid_response(&output, case);
    }

    let mut missing_starting = canonical.clone();
    missing_starting["lifecycle_history"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    let mut repeated_starting = canonical.clone();
    repeated_starting["lifecycle_history"][2]["lifecycle"] = json!("starting");
    let mut out_of_order = canonical.clone();
    out_of_order["lifecycle_history"][1]["lifecycle"] = json!("running");
    out_of_order["lifecycle_history"][2]["lifecycle"] = json!("starting");
    for (case, invalid) in [
        ("history omits starting", missing_starting),
        ("history repeats starting", repeated_starting),
        ("history states are out of order", out_of_order),
    ] {
        let output = run_fake_response(
            &directory,
            &supervisor,
            &request,
            &response_with_execution(invalid),
        );
        assert_invalid_response(&output, case);
    }

    for (case, pointer) in [
        ("unknown execution field", ""),
        ("unknown environment field", "/environment"),
        (
            "unknown environment entry field",
            "/environment/effective/0",
        ),
        ("unknown timeout field", "/timeout"),
        ("unknown cancellation field", "/cancellation"),
        ("unknown termination field", "/termination"),
        ("unknown stdout field", "/stdout"),
        ("unknown stderr field", "/stderr"),
        ("unknown lifecycle event field", "/lifecycle_history/0"),
        ("unknown observation log field", "/output_observations"),
        (
            "unknown output observation field",
            "/output_observations/entries/0",
        ),
    ] {
        let mut invalid = canonical.clone();
        invalid
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("unexpected".into(), json!(true));
        let response = response_with_execution(invalid);
        let output = run_fake_response(&directory, &supervisor, &request, &response);
        assert_invalid_response(&output, case);
    }

    let mut protocol_failure = canonical.clone();
    protocol_failure["lifecycle"] = json!("protocol_failure");
    protocol_failure["termination_reason"] = json!("protocol_failure");
    protocol_failure["lifecycle_history"][3]["lifecycle"] = json!("protocol_failure");
    let output = run_fake_response(
        &directory,
        &supervisor,
        &request,
        &response_with_execution(protocol_failure),
    );
    assert_invalid_response(&output, "protocol failure in success envelope");

    let mut failed_start = execution(RUN_ID, "failed_to_start", "start_error", None);
    failed_start["exit_code"] = json!(0);
    let output = run_fake_response(
        &directory,
        &supervisor,
        &request,
        &response_with_execution(failed_start),
    );
    assert_invalid_response(&output, "failed start with exit code");

    let mut failed_start = execution(RUN_ID, "failed_to_start", "start_error", None);
    failed_start["failure"] = Value::Null;
    let output = run_fake_response(
        &directory,
        &supervisor,
        &request,
        &response_with_execution(failed_start),
    );
    assert_invalid_response(&output, "failed start without failure details");

    let mut failed_start = execution(RUN_ID, "failed_to_start", "start_error", None);
    failed_start["failure"]
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), json!(true));
    let output = run_fake_response(
        &directory,
        &supervisor,
        &request,
        &response_with_execution(failed_start),
    );
    assert_invalid_response(&output, "unknown failure field");

    assert!(!directory.path().join(".relay").exists());
}

#[test]
fn fake_supervisor_binds_execution_result_to_the_complete_request() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_supervisor(&directory);
    let request = directory.path().join("request.json");
    let mut canonical = execution(RUN_ID, "completed", "exit_zero", Some(0));
    canonical["program"] = json!("/bin/echo");
    canonical["arguments"] = json!(["<redacted>", "visible", "<redacted>"]);
    canonical["timeout"]["configured_ms"] = json!(123);
    canonical["termination"]["grace_ms"] = json!(456);
    canonical["output_observations"]["limit_bytes"] = json!(791);
    canonical["stdout"]["text"] = json!("literal;$(not-a-shell");
    canonical["stdout"]["bytes_retained"] = json!(21);
    canonical["stdout"]["truncated"] = json!(true);
    let command_args = [
        "supervise",
        "--supervisor",
        supervisor.to_str().unwrap(),
        "--run-id",
        RUN_ID,
        "--timeout-ms",
        "123",
        "--termination-grace-ms",
        "456",
        "--stdout-limit-bytes",
        "21",
        "--stderr-limit-bytes",
        "1",
        "--log-limit-bytes",
        "791",
        "--redact-argument-index",
        "2",
        "--redact-argument-index",
        "0",
        "--",
        "/bin/echo",
        "secret-one",
        "visible",
        "secret-two",
    ];

    let response = response_with_execution(canonical.clone());
    let output = relay(
        directory.path(),
        &command_args,
        &fake_env(&request, &response),
    );
    let data = success_data(&output);
    assert_eq!(data["execution"], canonical);

    for (case, pointer, replacement) in [
        ("wrong program", "/program", json!("/bin/false")),
        (
            "unredacted first secret argument",
            "/arguments",
            json!(["secret-one", "visible", "<redacted>"]),
        ),
        (
            "unredacted second secret argument",
            "/arguments",
            json!(["<redacted>", "visible", "secret-two"]),
        ),
        (
            "wrong visible argument",
            "/arguments",
            json!(["<redacted>", "different", "<redacted>"]),
        ),
        (
            "wrong configured timeout",
            "/timeout/configured_ms",
            json!(124),
        ),
        (
            "wrong termination grace",
            "/termination/grace_ms",
            json!(457),
        ),
        (
            "wrong observation limit",
            "/output_observations/limit_bytes",
            json!(792),
        ),
    ] {
        let mut invalid = canonical.clone();
        *invalid
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("canonical execution is missing {pointer}")) = replacement;
        let response = response_with_execution(invalid);
        let output = relay(
            directory.path(),
            &command_args,
            &fake_env(&request, &response),
        );
        assert_invalid_response(&output, case);
    }
    let mut over_limit = canonical.clone();
    over_limit["stdout"]["text"] = json!("literal;$(not-a-shell)");
    over_limit["stdout"]["bytes_retained"] = json!(22);
    over_limit["stdout"]["truncated"] = json!(false);
    let output = relay(
        directory.path(),
        &command_args,
        &fake_env(&request, &response_with_execution(over_limit)),
    );
    assert_invalid_response(&output, "stdout retention exceeds request limit");
    let mut stderr_over_limit = canonical.clone();
    stderr_over_limit["stderr"]["text"] = json!("xx");
    stderr_over_limit["stderr"]["bytes_observed"] = json!(2);
    stderr_over_limit["stderr"]["bytes_retained"] = json!(2);
    stderr_over_limit["stderr"]["first_sequence"] = json!(5);
    stderr_over_limit["stderr"]["last_sequence"] = json!(5);
    stderr_over_limit["lifecycle_history"][3]["sequence"] = json!(6);
    stderr_over_limit["output_observations"]["bytes_used"] = json!(113);
    stderr_over_limit["output_observations"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(json!({"sequence": 5, "stream": "stderr", "bytes": 2}));
    let output = relay(
        directory.path(),
        &command_args,
        &fake_env(&request, &response_with_execution(stderr_over_limit)),
    );
    assert_invalid_response(&output, "stderr retention exceeds request limit");
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
fn legacy_v1_supervisor_activates_after_creating_its_requested_state_record() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_legacy_supervisor(&directory);
    let state = directory.path().join("legacy-state.json");
    let started = directory.path().join("legacy-request-read");
    let signalled = directory.path().join("legacy-signal-forwarded");
    let response = success_response(RUN_ID, "completed", "exit_zero", Some(0));
    let child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .env("RELAY_TEST_RESPONSE", &response)
        .env("RELAY_TEST_STARTED", &started)
        .env("RELAY_TEST_SIGNALLED", &signalled)
        .args([
            "--json",
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--state-path",
            state.to_str().unwrap(),
            "--",
            "/bin/true",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&state);
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let output = child.wait_with_output().unwrap();
    success_data(&output);
    assert!(
        signalled.is_file(),
        "legacy supervisor did not receive the signal forwarded after state proof"
    );
    let record: Value = serde_json::from_slice(&fs::read(state).unwrap()).unwrap();
    assert_eq!(record["run_state_version"], 1);
    assert_eq!(record["execution"]["run_id"], RUN_ID);
}

#[test]
fn legacy_v1_supervisor_can_prove_readiness_and_exit_with_its_response_immediately() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_legacy_supervisor(&directory);
    let state = directory.path().join("fast-legacy-state.json");
    let started = directory.path().join("fast-legacy-request-read");
    let response = success_response(RUN_ID, "completed", "exit_zero", Some(0));
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .env("RELAY_TEST_RESPONSE", &response)
        .env("RELAY_TEST_STARTED", &started)
        .env("RELAY_TEST_AUTO_RESPONSE", "1")
        .args([
            "--json",
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--state-path",
            state.to_str().unwrap(),
            "--",
            "/bin/true",
        ])
        .output()
        .unwrap();

    success_data(&output);
    assert!(started.is_file());
    let record: Value = serde_json::from_slice(&fs::read(state).unwrap()).unwrap();
    assert_eq!(record["execution"]["run_id"], RUN_ID);
}

#[test]
fn stale_state_does_not_spoof_legacy_readiness_before_atomic_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_legacy_supervisor(&directory);
    let state = directory.path().join("reused-state.json");
    let started = directory.path().join("legacy-request-read");
    let replace_release = directory.path().join("allow-state-replacement");
    let early_signal = directory.path().join("signal-before-replacement");
    let signalled = directory.path().join("signal-after-replacement");
    fs::write(
        &state,
        serde_json::to_vec(&json!({
            "run_state_version": 1,
            "supervisor_version": "stale-v1",
            "execution": {"run_id": "stale-run"}
        }))
        .unwrap(),
    )
    .unwrap();
    let response = success_response(RUN_ID, "completed", "exit_zero", Some(0));
    let child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .env("RELAY_TEST_RESPONSE", &response)
        .env("RELAY_TEST_STARTED", &started)
        .env("RELAY_TEST_SIGNALLED", &signalled)
        .env("RELAY_TEST_REPLACE_RELEASE", &replace_release)
        .env("RELAY_TEST_EARLY_SIGNAL", &early_signal)
        .args([
            "--json",
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--state-path",
            state.to_str().unwrap(),
            "--",
            "/bin/true",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&started);
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    // The compatibility poll is capped at 50 ms; this margin tests that the
    // unchanged record is not a proof rather than measuring successful startup.
    thread::sleep(Duration::from_secs(1));
    assert!(
        !early_signal.exists(),
        "the stale state record incorrectly activated signal forwarding"
    );
    fs::write(&replace_release, b"replace\n").unwrap();
    let output = child.wait_with_output().unwrap();
    success_data(&output);
    assert!(signalled.is_file());
    let record: Value = serde_json::from_slice(&fs::read(state).unwrap()).unwrap();
    assert_eq!(record["execution"]["run_id"], RUN_ID);
}

#[cfg(target_os = "linux")]
#[test]
fn readiness_fd_stays_above_stdio_and_unrelated_inheritable_fds_are_confined() {
    const AMBIENT_FD: libc::c_int = 100;

    let directory = tempfile::tempdir().unwrap();
    let supervisor = write_descriptor_probe_supervisor(&directory);
    let state = directory.path().join("descriptor-state.json");
    let response = success_response(RUN_ID, "completed", "exit_zero", Some(0));
    let ambient = OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .expect("open ambient descriptor source");
    let ambient_source = ambient.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_relay"));
    command
        .current_dir(directory.path())
        .env("RELAY_TEST_RESPONSE", &response)
        .env("RELAY_TEST_AMBIENT_FD", AMBIENT_FD.to_string())
        .args([
            "--json",
            "supervise",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--run-id",
            RUN_ID,
            "--state-path",
            state.to_str().unwrap(),
            "--",
            "/bin/true",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(ambient_source, AMBIENT_FD) == -1
                || libc::fcntl(AMBIENT_FD, libc::F_SETFD, 0) == -1
                || libc::close(libc::STDIN_FILENO) == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().expect("run Relay with stdin closed");
    success_data(&output);
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
            "go-signal-exit",
            "--",
            "/bin/sh",
            "-c",
            "kill -TERM $$",
        ],
        &[],
    );
    let data = success_data(&output);
    assert_eq!(data["execution"]["lifecycle"], "completed");
    assert_eq!(data["execution"]["termination_reason"], "exit_nonzero");
    assert_eq!(data["execution"]["exit_code"], Value::Null);

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
