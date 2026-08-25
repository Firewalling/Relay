#![cfg(target_os = "linux")]

use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const FAKE_SUPERVISOR: &str = r###"#!/usr/bin/python3
import json
import os
import signal
import sys
import time

cancelled = False
def cancel(_signal, _frame):
    global cancelled
    cancelled = True
signal.signal(signal.SIGINT, cancel)
signal.signal(signal.SIGTERM, cancel)
legacy = os.environ.get("RELAY_FAKE_LEGACY") == "1"
if not legacy:
    ready_fd = int(os.environ["RELAY_SUPERVISOR_READY_FD"])
    os.write(ready_fd, b"R")
    if os.read(ready_fd, 2) != b"A":
        sys.stderr.write("invalid readiness acknowledgement\n")
        sys.exit(92)
    os.close(ready_fd)

request = json.load(sys.stdin)
if legacy:
    state_path = request["state_path"]
    os.makedirs(os.path.dirname(state_path), exist_ok=True)
    temporary = state_path + ".legacy.tmp"
    with open(temporary, "w", encoding="utf-8") as destination:
        json.dump({
            "run_state_version": 1,
            "supervisor_version": "legacy-fake-v1",
            "execution": {
                "run_id": request["run_id"],
                "protocol_version": 1,
            },
        }, destination, separators=(",", ":"))
    os.replace(temporary, state_path)
control_path = os.environ["RELAY_FAKE_CONTROL"]
log_path = os.environ["RELAY_FAKE_LOG"]
with open(control_path, "r", encoding="utf-8") as source:
    control = json.load(source)
record = json.dumps(request, separators=(",", ":")) + "\n"
fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
try:
    os.write(fd, record.encode("utf-8"))
finally:
    os.close(fd)

with open(request["state_path"] + ".pid", "w", encoding="utf-8") as pid_file:
    pid_file.write(str(os.getpid()))
name = os.path.basename(request["program"])
mode = control.get(name, "success")
if mode == "malformed":
    sys.stdout.write("{not-json\n")
    sys.exit(0)
if mode == "remote":
    error = {"output_schema_version":1,"ok":False,"command":"supervise",
             "error":{"code":"fake_transport","message":"deliberate fake failure"}}
    sys.stderr.write(json.dumps(error, separators=(",", ":")) + "\n")
    sys.exit(7)

if mode in ("hold", "wait_cancel"):
    deadline = time.monotonic() + 10.0
    while not cancelled and time.monotonic() < deadline:
        time.sleep(0.01)
    if cancelled:
        mode = "cancelled"
    else:
        mode = "success"

lifecycle = "completed"
reason = "exit_zero"
exit_code = 0
failure = None
timeout_triggered = False
cancel_requested = False
cancel_source = "none"
graceful = False
if mode == "nonzero":
    reason = "exit_nonzero"
    exit_code = 17
elif mode == "timeout":
    lifecycle = "timed_out"
    reason = "timeout_terminated"
    exit_code = None
    timeout_triggered = True
    graceful = True
elif mode == "start_failure":
    lifecycle = "failed_to_start"
    reason = "start_error"
    exit_code = None
    failure = {"code":"start_failure","message":"deliberate start failure"}
elif mode == "cancelled":
    lifecycle = "cancelled"
    reason = "cancelled_terminated"
    exit_code = None
    cancel_requested = True
    cancel_source = "signal"
    graceful = True

arguments = list(request["arguments"])
for index in request["redact_argument_indexes"]:
    arguments[index] = "<redacted>"
history = [
    {"sequence":1,"lifecycle":"accepted","timestamp":"2026-01-02T03:04:05Z"},
    {"sequence":2,"lifecycle":"starting","timestamp":"2026-01-02T03:04:05.010Z"},
]
if lifecycle != "failed_to_start":
    history.append({"sequence":3,"lifecycle":"running","timestamp":"2026-01-02T03:04:05.020Z"})
history.append({"sequence":4,"lifecycle":lifecycle,"timestamp":"2026-01-02T03:04:05.100Z"})
execution = {
    "run_id":request["run_id"],
    "protocol_version":1,
    "supervisor_version":"fake-v1",
    "program":request["program"],
    "arguments":arguments,
    "environment":{
        "inherited_allowlist":["LANG","PATH"],
        "effective":[
            {"name":"RELAY_PROTOCOL_VERSION","source":"required"},
            {"name":"RELAY_RUN_ID","source":"required"},
            {"name":"RELAY_SUPERVISOR_VERSION","source":"required"}
        ],
        "blocked_inherited_count":0,
        "duplicate_user_overrides":0
    },
    "accepted_at":"2026-01-02T03:04:05Z",
    "started_at":"2026-01-02T03:04:05.010Z",
    "completed_at":"2026-01-02T03:04:05.100Z",
    "duration_ms":100,
    "lifecycle":lifecycle,
    "termination_reason":reason,
    "exit_code":exit_code,
    "failure":failure,
    "timeout":{"configured_ms":request["timeout_ms"],"triggered":timeout_triggered},
    "cancellation":{"requested":cancel_requested,"source":cancel_source},
    "termination":{"grace_ms":request["termination_grace_ms"],"graceful_attempted":graceful,"escalated":False},
    "stdout":{"text":"","bytes_observed":0,"bytes_retained":0,"truncated":False,"first_sequence":None,"last_sequence":None},
    "stderr":{"text":"","bytes_observed":0,"bytes_retained":0,"truncated":False,"first_sequence":None,"last_sequence":None},
    "lifecycle_history":history,
    "output_observations":{"limit_bytes":request["log_limit_bytes"],"bytes_used":0,"truncated":False,"entries":[]}
}
state_path = request["state_path"]
os.makedirs(os.path.dirname(state_path), exist_ok=True)
temporary = state_path + ".fake.tmp"
with open(temporary, "w", encoding="utf-8") as destination:
    json.dump(execution, destination, separators=(",", ":"))
os.replace(temporary, state_path)
response = {"output_schema_version":1,"ok":True,"command":"supervise","data":execution}
sys.stdout.write(json.dumps(response, separators=(",", ":")) + "\n")
"###;

struct Harness {
    directory: TempDir,
    supervisor: PathBuf,
    control: PathBuf,
    log: PathBuf,
    temporary_root: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create isolated run directory");
        let supervisor = directory.path().join("fake-supervisor");
        fs::write(&supervisor, FAKE_SUPERVISOR).expect("write fake supervisor");
        fs::set_permissions(&supervisor, fs::Permissions::from_mode(0o755))
            .expect("make fake supervisor executable");
        let control = directory.path().join("control.json");
        let log = directory.path().join("requests.jsonl");
        let temporary_root = directory.path().join("tmp");
        fs::create_dir(&temporary_root).expect("create isolated temporary directory");
        fs::write(&control, b"{}\n").expect("write initial fake control");
        Self {
            directory,
            supervisor,
            control,
            log,
            temporary_root,
        }
    }

    fn configure(&self, outcomes: &[(&str, &str)]) {
        let value = Value::Object(
            outcomes
                .iter()
                .map(|(task, outcome)| ((*task).to_owned(), json!(*outcome)))
                .collect(),
        );
        fs::write(&self.control, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn plan(&self, name: &str, tasks: Value) -> PathBuf {
        let path = self.directory.path().join(format!("{name}.json"));
        fs::write(
            &path,
            serde_json::to_vec(&json!({"schema_version":1,"tasks":tasks})).unwrap(),
        )
        .unwrap();
        path
    }

    fn state(&self, name: &str) -> PathBuf {
        self.directory.path().join(format!("{name}.state.json"))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_relay"));
        command
            .current_dir(self.directory.path())
            .env("RELAY_FAKE_CONTROL", &self.control)
            .env("RELAY_FAKE_LOG", &self.log)
            .env("TMPDIR", &self.temporary_root);
        command
    }

    fn run_json(&self, plan: &Path, state: &Path) -> Output {
        self.command()
            .args([
                "--json",
                "run",
                "--supervisor",
                self.supervisor.to_str().unwrap(),
                "--state-path",
                state.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .output()
            .expect("run relay coordinator")
    }

    fn run_human(&self, plan: &Path, state: &Path) -> Output {
        self.command()
            .args([
                "run",
                "--supervisor",
                self.supervisor.to_str().unwrap(),
                "--state-path",
                state.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .output()
            .expect("run human relay coordinator")
    }

    fn resume_json(&self, plan: &Path, state: &Path) -> Output {
        self.command()
            .args([
                "--json",
                "run",
                "--supervisor",
                self.supervisor.to_str().unwrap(),
                "--resume",
                state.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .output()
            .expect("resume relay coordinator")
    }

    fn spawn_json(&self, plan: &Path, state: &Path) -> Child {
        self.command()
            .args([
                "--json",
                "run",
                "--supervisor",
                self.supervisor.to_str().unwrap(),
                "--state-path",
                state.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn relay coordinator")
    }

    fn requests(&self) -> Vec<Value> {
        match File::open(&self.log) {
            Ok(file) => BufReader::new(file)
                .lines()
                .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read fake request log: {error}"),
        }
    }

    fn wait_for_requests(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.requests().len() < count {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for fake supervisor"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn task(id: &str, dependencies: &[&str]) -> Value {
    json!({
        "id":id,
        "depends_on":dependencies,
        "program":format!("/fake/{id}"),
        "arguments":[],
        "timeout_ms":5000
    })
}

fn run_document(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "run envelope unexpectedly wrote stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON success envelope");
    assert_eq!(document["output_schema_version"], 1);
    assert_eq!(document["ok"], true);
    assert_eq!(document["command"], "run");
    assert!(document["data"].is_object());
    let run_status = document["data"]["run"]["status"]
        .as_str()
        .expect("run envelope status");
    assert_eq!(
        output.status.code(),
        Some(if run_status == "completed" { 0 } else { 1 }),
        "shell status disagrees with run status {run_status}"
    );
    document
}

fn error_document(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let document: Value =
        serde_json::from_slice(&output.stderr).expect("valid JSON error envelope");
    assert_eq!(document["output_schema_version"], 1);
    assert_eq!(document["ok"], false);
    assert_eq!(document["command"], "run");
    assert!(document["error"]["code"].as_str().is_some());
    document
}

fn read_state(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("aggregate state exists"))
        .expect("aggregate state is valid JSON")
}

fn records(state: &Value) -> &[Value] {
    state["tasks"].as_array().expect("ordered task records")
}

fn record<'a>(state: &'a Value, id: &str) -> &'a Value {
    records(state)
        .iter()
        .find(|task| task["id"] == id)
        .unwrap_or_else(|| panic!("missing aggregate task {id}"))
}

fn request_names(requests: &[Value]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| {
            Path::new(request["program"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect()
}

fn terminate(child: Child) -> Output {
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    child.wait_with_output().expect("wait for cancelled relay")
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

#[test]
fn real_go_supervisor_runs_two_task_dependency_dag_when_configured() {
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

    let directory = tempfile::tempdir().expect("create isolated real-Go run directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).expect("create isolated temporary directory");
    let marker = directory
        .path()
        .join("literal;$(not-a-shell) dependency marker");
    let plan = directory.path().join("real-go-dag.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "tasks": [
                {
                    "id": "produce",
                    "depends_on": [],
                    "program": "/usr/bin/touch",
                    "arguments": [marker.to_str().unwrap()],
                    "timeout_ms": 5_000
                },
                {
                    "id": "consume",
                    "depends_on": ["produce"],
                    "program": "/usr/bin/test",
                    "arguments": ["-f", marker.to_str().unwrap()],
                    "timeout_ms": 5_000
                }
            ]
        }))
        .unwrap(),
    )
    .expect("write real-Go run plan");

    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .env("TMPDIR", &temporary_root)
        .args([
            "--json",
            "run",
            "--supervisor",
            supervisor.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .expect("run Relay with the real Go supervisor");
    let document = run_document(&output);
    let state_path = PathBuf::from(
        document["data"]["state_path"]
            .as_str()
            .expect("default aggregate state path"),
    );
    assert!(state_path.starts_with(directory.path().join(".relay/runs")));
    assert!(state_path.is_file());

    let state = read_state(&state_path);
    assert_eq!(state["status"], "completed");
    assert_eq!(
        records(&state)
            .iter()
            .map(|task| task["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["produce", "consume"]
    );
    assert!(
        records(&state)
            .iter()
            .all(|task| task["status"] == "succeeded")
    );
    assert!(marker.is_file(), "dependent task did not observe producer");

    let supervisor_directory = temporary_root.join("relay/supervisor");
    let expected = [
        (
            "produce",
            "/usr/bin/touch",
            json!([marker.to_str().unwrap()]),
        ),
        (
            "consume",
            "/usr/bin/test",
            json!(["-f", marker.to_str().unwrap()]),
        ),
    ];
    let executions = expected.map(|(id, program, arguments)| {
        let task = record(&state, id);
        let attempt = task["attempt_run_id"]
            .as_str()
            .expect("terminal task attempt identity");
        let supervisor_state = PathBuf::from(
            task["supervisor_state_path"]
                .as_str()
                .expect("external Go state path"),
        );
        assert_eq!(
            supervisor_state,
            supervisor_directory.join(format!("{attempt}.json"))
        );
        assert!(supervisor_state.is_file());

        let go_record: Value =
            serde_json::from_slice(&fs::read(supervisor_state).expect("read Go run record"))
                .expect("valid Go run record");
        assert_eq!(go_record["run_state_version"], 1);
        let execution = go_record["execution"].clone();
        assert_eq!(execution["run_id"], attempt);
        assert_eq!(execution["lifecycle"], "completed");
        assert_eq!(execution["termination_reason"], "exit_zero");
        assert_eq!(execution["exit_code"], 0);
        assert_eq!(execution["program"], program);
        assert_eq!(execution["arguments"], arguments);
        assert_eq!(task["summary"]["lifecycle"], execution["lifecycle"]);
        assert_eq!(
            task["summary"]["termination_reason"],
            execution["termination_reason"]
        );
        assert_eq!(task["summary"]["exit_code"], execution["exit_code"]);
        execution
    });
    assert_ne!(
        record(&state, "produce")["attempt_run_id"],
        record(&state, "consume")["attempt_run_id"]
    );
    assert!(
        chrono::DateTime::parse_from_rfc3339(executions[0]["completed_at"].as_str().unwrap())
            .unwrap()
            <= chrono::DateTime::parse_from_rfc3339(executions[1]["accepted_at"].as_str().unwrap())
                .unwrap(),
        "dependent task started before its prerequisite completed"
    );
}

#[cfg(debug_assertions)]
#[test]
fn real_go_supervisor_cancels_signal_latched_immediately_after_spawn_when_configured() {
    let Some(supervisor) = std::env::var_os("RELAY_SUPERVISOR_E2E_BIN") else {
        return;
    };
    let supervisor = PathBuf::from(supervisor);
    assert!(
        supervisor.is_absolute() && supervisor.is_file(),
        "RELAY_SUPERVISOR_E2E_BIN must name an absolute file"
    );

    let directory = tempfile::tempdir().expect("create isolated real-Go cancellation directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).expect("create isolated temporary directory");
    let stage_marker = directory.path().join("supervisor-spawned");
    let stage_release = directory.path().join("release-supervisor");
    let first_launched = directory.path().join("first-launched");
    let later_launched = directory.path().join("later-launched");
    let state_path = directory.path().join("cancelled.state.json");
    let plan = directory.path().join("cancel-after-spawn.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "tasks": [
                {
                    "id": "first",
                    "depends_on": [],
                    "program": "/usr/bin/touch",
                    "arguments": [first_launched.to_str().unwrap()],
                    "timeout_ms": 5_000
                },
                {
                    "id": "later",
                    "depends_on": ["first"],
                    "program": "/usr/bin/touch",
                    "arguments": [later_launched.to_str().unwrap()],
                    "timeout_ms": 5_000
                }
            ]
        }))
        .unwrap(),
    )
    .expect("write real-Go cancellation plan");

    let child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .env("TMPDIR", &temporary_root)
        .env(
            "RELAY_TEST_SUPERVISOR_PAUSE",
            "after_spawn_before_activation",
        )
        .env("RELAY_TEST_SUPERVISOR_MARKER", &stage_marker)
        .env("RELAY_TEST_SUPERVISOR_RELEASE", &stage_release)
        .args([
            "--json",
            "run",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--state-path",
            state_path.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Relay at the post-supervisor-spawn test stage");
    wait_for_path(&stage_marker);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    fs::write(&stage_release, b"release\n").expect("release supervisor activation");

    let output = child
        .wait_with_output()
        .expect("wait for clean Relay cancellation");
    run_document(&output);
    assert!(
        state_path.is_file(),
        "aggregate cancellation was not persisted"
    );
    let state = read_state(&state_path);
    assert_eq!(state["status"], "cancelled");
    assert_eq!(state["cancellation_requested"], true);
    assert_eq!(record(&state, "first")["status"], "cancelled");
    assert_ne!(record(&state, "first")["status"], "indeterminate");
    assert_eq!(record(&state, "later")["status"], "blocked");
    assert!(
        !first_launched.exists(),
        "cancelled task program was launched"
    );
    assert!(!later_launched.exists(), "later task program was launched");

    let supervisor_state = PathBuf::from(
        record(&state, "first")["supervisor_state_path"]
            .as_str()
            .expect("cancelled task supervisor state path"),
    );
    assert!(
        supervisor_state.is_file(),
        "Go cancellation was not persisted"
    );
    let go_record: Value =
        serde_json::from_slice(&fs::read(supervisor_state).expect("read Go cancellation state"))
            .expect("valid Go cancellation state");
    let execution = &go_record["execution"];
    assert_eq!(execution["lifecycle"], "cancelled");
    assert_eq!(execution["termination_reason"], "cancelled_terminated");
    assert_eq!(execution["cancellation"]["requested"], true);
    assert_eq!(execution["cancellation"]["source"], "supervisor_signal");
    assert!(
        execution["lifecycle_history"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["lifecycle"] != "running"),
        "Go launched the task before observing cancellation"
    );
}

#[test]
fn default_state_path_is_project_scoped_and_survives_temporary_cleanup() {
    let harness = Harness::new();
    let plan = harness.plan("default-state", json!([task("default-state", &[])]));
    let output = harness
        .command()
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let document = run_document(&output);
    let state_path = PathBuf::from(
        document["data"]["state_path"]
            .as_str()
            .expect("default aggregate state path"),
    );
    assert!(state_path.is_absolute());
    assert!(state_path.starts_with(harness.directory.path().join(".relay/runs")));
    assert_eq!(
        state_path.extension().and_then(|value| value.to_str()),
        Some("json")
    );
    assert!(state_path.exists());
    assert_eq!(read_state(&state_path)["status"], "completed");

    fs::remove_dir_all(&harness.temporary_root).unwrap();
    assert!(state_path.exists());
    assert_eq!(read_state(&state_path)["status"], "completed");
}

#[test]
fn default_state_uses_discovered_root_and_honors_scope() {
    let harness = Harness::new();
    let nested = harness.directory.path().join("nested/work");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir(harness.directory.path().join(".relay")).unwrap();

    let auto_plan = harness.plan("auto-scope", json!([task("auto-scope", &[])]));
    let auto = harness
        .command()
        .current_dir(&nested)
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            auto_plan.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let auto_document = run_document(&auto);
    let auto_path = Path::new(auto_document["data"]["state_path"].as_str().unwrap());
    assert!(auto_path.starts_with(harness.directory.path().join(".relay/runs")));

    let current_plan = harness.plan("current-scope", json!([task("current-scope", &[])]));
    let current = harness
        .command()
        .current_dir(&nested)
        .args([
            "--json",
            "--scope",
            "current",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            current_plan.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let current_document = run_document(&current);
    let current_path = Path::new(current_document["data"]["state_path"].as_str().unwrap());
    assert!(current_path.starts_with(nested.join(".relay/runs")));

    let repository = harness
        .command()
        .current_dir(&nested)
        .args([
            "--json",
            "--scope",
            "repository",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            current_plan.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let error = error_document(&repository);
    assert_eq!(error["error"]["code"], "project_discovery");
    assert_eq!(harness.requests().len(), 2);
}

#[test]
fn executes_literal_arguments_in_stable_dependency_order() {
    let harness = Harness::new();
    let literal = "$(touch shell-ran); * ' \" ; newline\nkept";
    let mut independent = task("independent", &[]);
    independent["arguments"] = json!([literal, "two words"]);
    let plan = harness.plan(
        "order",
        json!([
            task("last", &["left", "right"]),
            task("right", &["root"]),
            independent,
            task("left", &["root"]),
            task("root", &[])
        ]),
    );
    let state_path = harness.state("order");

    run_document(&harness.run_json(&plan, &state_path));

    let requests = harness.requests();
    assert_eq!(
        request_names(&requests),
        ["independent", "root", "right", "left", "last"]
    );
    assert_eq!(requests[0]["arguments"], json!([literal, "two words"]));
    assert!(!harness.directory.path().join("shell-ran").exists());
    let state = read_state(&state_path);
    assert_eq!(state["status"], "completed");
    assert!(
        records(&state)
            .iter()
            .all(|task| task["status"] == "succeeded")
    );
}

#[test]
fn rejects_invalid_graphs_before_starting_the_supervisor() {
    let harness = Harness::new();
    let cases = [
        ("duplicate-id", json!([task("a", &[]), task("a", &[])])),
        ("missing", json!([task("a", &["absent"])])),
        ("self", json!([task("a", &["a"])])),
        (
            "duplicate-dependency",
            json!([task("a", &[]), task("b", &["a", "a"])]),
        ),
        ("cycle", json!([task("a", &["b"]), task("b", &["a"])])),
    ];
    for (name, tasks) in cases {
        let plan = harness.plan(name, tasks);
        let output = harness.run_json(&plan, &harness.state(name));
        let error = error_document(&output);
        assert_eq!(error["error"]["code"], "run_plan_invalid", "case {name}");
    }
    assert!(harness.requests().is_empty());
}

#[test]
fn validates_closed_plan_limits_absolute_paths_and_redactions_before_launch() {
    let harness = Harness::new();
    let mut cases = Vec::new();
    let mut zero_timeout = task("zero", &[]);
    zero_timeout["timeout_ms"] = json!(0);
    cases.push(("zero-timeout", zero_timeout));
    let mut negative_timeout = task("negative", &[]);
    negative_timeout["timeout_ms"] = json!(-1);
    cases.push(("negative-timeout", negative_timeout));
    let mut excessive_timeout = task("long", &[]);
    excessive_timeout["timeout_ms"] = json!(86_400_001);
    cases.push(("excessive-timeout", excessive_timeout));
    let mut relative_program = task("program", &[]);
    relative_program["program"] = json!("relative/program");
    cases.push(("relative-program", relative_program));
    let mut relative_working = task("working", &[]);
    relative_working["working_directory"] = json!("relative/directory");
    cases.push(("relative-working-directory", relative_working));
    let mut duplicate_redaction = task("redact-duplicate", &[]);
    duplicate_redaction["arguments"] = json!(["secret"]);
    duplicate_redaction["redact_argument_indexes"] = json!([0, 0]);
    cases.push(("duplicate-redaction", duplicate_redaction));
    let mut out_of_range_redaction = task("redact-range", &[]);
    out_of_range_redaction["arguments"] = json!(["secret"]);
    out_of_range_redaction["redact_argument_indexes"] = json!([1]);
    cases.push(("out-of-range-redaction", out_of_range_redaction));
    let mut unknown = task("unknown", &[]);
    unknown["unexpected"] = json!(true);
    cases.push(("unknown-field", unknown));
    let mut unsafe_id = task("bad/id", &[]);
    unsafe_id["program"] = json!("/fake/bad-id");
    cases.push(("unsafe-id", unsafe_id));

    for (name, invalid_task) in cases {
        let plan = harness.plan(name, json!([invalid_task]));
        let error = error_document(&harness.run_json(&plan, &harness.state(name)));
        assert_eq!(error["error"]["code"], "run_plan_invalid", "case {name}");
    }
    let nonfinite = harness.directory.path().join("nonfinite.json");
    fs::write(
        &nonfinite,
        br#"{"schema_version":1,"tasks":[{"id":"nonfinite","program":"/fake/nonfinite","timeout_ms":1e999}]}"#,
    )
    .unwrap();
    let error = error_document(&harness.run_json(&nonfinite, &harness.state("nonfinite")));
    assert_eq!(error["error"]["code"], "run_plan_invalid");
    let unknown_root = harness.directory.path().join("unknown-root.json");
    fs::write(
        &unknown_root,
        serde_json::to_vec(&json!({
            "schema_version":1,
            "tasks":[task("root-field", &[])],
            "unexpected":true
        }))
        .unwrap(),
    )
    .unwrap();
    let error = error_document(&harness.run_json(&unknown_root, &harness.state("unknown-root")));
    assert_eq!(error["error"]["code"], "run_plan_invalid");

    let valid_plan = harness.plan("relative-supervisor", json!([task("absolute", &[])]));
    let relative_supervisor = harness
        .command()
        .args([
            "--json",
            "run",
            "--supervisor",
            "relative-supervisor",
            "--state-path",
            harness.state("relative-supervisor").to_str().unwrap(),
            valid_plan.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let error = error_document(&relative_supervisor);
    assert_eq!(error["error"]["code"], "run_plan_invalid");
    assert!(harness.requests().is_empty());
}

#[test]
fn oversized_encoded_task_request_is_rejected_before_aggregate_creation() {
    let harness = Harness::new();
    let mut oversized = task("oversized", &[]);
    oversized["arguments"] = json!(["x".repeat(1_048_200)]);
    let plan = harness.plan("oversized-request", json!([oversized]));
    assert!(
        fs::metadata(&plan).unwrap().len() <= 1 << 20,
        "fixture must pass the plan byte limit"
    );
    let state_path = harness.state("oversized-request");

    let error = error_document(&harness.run_json(&plan, &state_path));

    assert_eq!(error["error"]["code"], "run_plan_invalid");
    assert!(!state_path.exists());
    assert!(harness.requests().is_empty());
}

#[test]
fn successful_state_has_unique_attempt_identities_and_sanitizes_secrets() {
    let harness = Harness::new();
    let secret_a = "declared-secret-alpha";
    let secret_b = "declared-secret-beta";
    let mut first = task("first", &[]);
    first["arguments"] = json!(["visible", secret_a]);
    first["redact_argument_indexes"] = json!([1]);
    let mut second = task("second", &["first"]);
    second["arguments"] = json!([secret_b]);
    second["redact_argument_indexes"] = json!([0]);
    let plan = harness.plan("success", json!([first, second]));
    let state_path = harness.state("success");

    let document = run_document(&harness.run_json(&plan, &state_path));
    assert_eq!(document["data"]["state_path"], state_path.to_str().unwrap());
    let bytes = fs::read(&state_path).unwrap();
    let persisted = String::from_utf8(bytes).unwrap();
    assert!(!persisted.contains(secret_a));
    assert!(!persisted.contains(secret_b));
    assert!(persisted.contains("<redacted>"));

    let state: Value = serde_json::from_str(&persisted).unwrap();
    assert_eq!(document["data"]["run"], state);
    assert_eq!(state["schema_version"], 1);
    assert_eq!(state["status"], "completed");
    let first = record(&state, "first");
    let second = record(&state, "second");
    let first_run = first["attempt_run_id"]
        .as_str()
        .expect("first attempt run ID");
    let second_run = second["attempt_run_id"]
        .as_str()
        .expect("second attempt run ID");
    assert_ne!(first_run, second_run);
    let first_path = Path::new(
        first["supervisor_state_path"]
            .as_str()
            .expect("first task state path"),
    );
    let second_path = Path::new(
        second["supervisor_state_path"]
            .as_str()
            .expect("second task state path"),
    );
    let supervisor_directory = harness.temporary_root.join("relay/supervisor");
    assert_eq!(
        first_path,
        supervisor_directory.join(format!("{first_run}.json"))
    );
    assert_eq!(
        second_path,
        supervisor_directory.join(format!("{second_run}.json"))
    );
    assert_ne!(first_path, second_path);
    assert!(first_path.exists() && second_path.exists());
    let requests = harness.requests();
    assert_eq!(requests[0]["state_path"], first_path.to_str().unwrap());
    assert_eq!(requests[1]["state_path"], second_path.to_str().unwrap());
}

#[test]
fn aggregate_state_and_lock_files_are_owner_private() {
    let harness = Harness::new();
    let plan = harness.plan("private-state", json!([task("private-state", &[])]));
    let state_path = harness.state("private-state");

    run_document(&harness.run_json(&plan, &state_path));

    let lock_path = state_path.with_file_name(format!(
        "{}.orchestration.lock",
        state_path.file_name().unwrap().to_str().unwrap()
    ));
    for (description, path) in [
        ("aggregate state", &state_path),
        ("coordinator lock", &lock_path),
    ] {
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{description} mode");
    }
}

#[test]
fn canonical_state_identity_is_persisted_and_hardlink_alias_resume_is_rejected() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "cancelled")]);
    let plan = harness.plan(
        "state-identity",
        json!([task("waiting", &[]), task("later", &[])]),
    );
    let state_path = harness.state("state-identity");
    run_document(&harness.run_json(&plan, &state_path));

    let canonical_state_path = fs::canonicalize(&state_path).unwrap();
    let state = read_state(&state_path);
    assert_eq!(
        state["state_path"],
        canonical_state_path.to_str().expect("UTF-8 canonical path")
    );

    let alias = harness.state("state-identity-alias");
    fs::hard_link(&state_path, &alias).unwrap();
    let before = harness.requests().len();
    let error = error_document(&harness.resume_json(&plan, &alias));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before);
    assert_eq!(read_state(&state_path), state);
}

#[test]
fn failures_block_descendants_but_independent_tasks_continue() {
    let harness = Harness::new();
    harness.configure(&[
        ("nonzero", "nonzero"),
        ("timeout", "timeout"),
        ("start", "start_failure"),
    ]);
    let plan = harness.plan(
        "failures",
        json!([
            task("nonzero", &[]),
            task("nonzero-child", &["nonzero"]),
            task("timeout", &[]),
            task("timeout-child", &["timeout"]),
            task("start", &[]),
            task("start-child", &["start"]),
            task("independent", &[])
        ]),
    );
    let state_path = harness.state("failures");

    let document = run_document(&harness.run_json(&plan, &state_path));

    assert_eq!(
        request_names(&harness.requests()),
        ["nonzero", "timeout", "start", "independent"]
    );
    let state = read_state(&state_path);
    assert_eq!(state["status"], "failed");
    assert_eq!(document["data"]["run"], state);
    for id in ["nonzero", "timeout", "start"] {
        assert_eq!(record(&state, id)["status"], "failed", "task {id}");
    }
    for id in ["nonzero-child", "timeout-child", "start-child"] {
        assert_eq!(record(&state, id)["status"], "blocked", "task {id}");
        assert!(record(&state, id)["reason"].as_str().is_some());
    }
    assert_eq!(record(&state, "independent")["status"], "succeeded");
}

#[test]
fn coordinator_accepts_legacy_v1_state_creation_before_cancellation_response() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "wait_cancel")]);
    let plan = harness.plan("legacy-cancel", json!([task("waiting", &[])]));
    let state_path = harness.state("legacy-cancel");
    let child = harness
        .command()
        .env("RELAY_FAKE_LEGACY", "1")
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--state-path",
            state_path.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coordinator with a legacy v1 supervisor");
    harness.wait_for_requests(1);
    let request = harness.requests().remove(0);
    let attempt_state = PathBuf::from(request["state_path"].as_str().unwrap());
    wait_for_path(&attempt_state);

    let output = terminate(child);
    run_document(&output);
    let aggregate = read_state(&state_path);
    assert_eq!(aggregate["status"], "cancelled");
    assert_eq!(record(&aggregate, "waiting")["status"], "cancelled");
    let execution: Value =
        serde_json::from_slice(&fs::read(attempt_state).expect("legacy attempt state")).unwrap();
    assert_eq!(execution["run_id"], request["run_id"]);
    assert_eq!(execution["lifecycle"], "cancelled");
}

#[test]
fn cancellation_blocks_only_descendants_and_resume_runs_pending_independent_tasks() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "wait_cancel")]);
    let plan = harness.plan(
        "cancel-resume",
        json!([
            task("first", &[]),
            task("waiting", &[]),
            task("later", &[]),
            task("waiting-child", &["waiting"])
        ]),
    );
    let state_path = harness.state("cancel-resume");
    let child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(2);

    let cancelled = terminate(child);
    let document = run_document(&cancelled);
    let interrupted = read_state(&state_path);
    assert_eq!(interrupted["status"], "cancelled");
    assert_eq!(document["data"]["run"], interrupted);
    assert_eq!(record(&interrupted, "first")["status"], "succeeded");
    assert_eq!(record(&interrupted, "waiting")["status"], "cancelled");
    assert_eq!(record(&interrupted, "later")["status"], "pending");
    assert_eq!(record(&interrupted, "waiting-child")["status"], "blocked");
    assert_eq!(interrupted["cancellation_requested"], true);
    assert_eq!(request_names(&harness.requests()), ["first", "waiting"]);

    harness.configure(&[]);
    run_document(&harness.resume_json(&plan, &state_path));
    let resumed = read_state(&state_path);
    assert_eq!(record(&resumed, "first")["status"], "succeeded");
    assert_eq!(record(&resumed, "waiting")["status"], "cancelled");
    assert_eq!(record(&resumed, "later")["status"], "succeeded");
    assert_eq!(record(&resumed, "waiting-child")["status"], "blocked");
    assert_eq!(resumed["cancellation_requested"], false);
    assert_eq!(
        request_names(&harness.requests()),
        ["first", "waiting", "later"]
    );
}

#[test]
fn resume_from_another_directory_keeps_the_original_default_working_directory() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "cancelled")]);
    let plan = harness.plan(
        "cwd-resume",
        json!([task("waiting", &[]), task("later", &[])]),
    );
    let state_path = harness.state("cwd-resume");
    run_document(&harness.run_json(&plan, &state_path));
    let interrupted = read_state(&state_path);
    assert_eq!(record(&interrupted, "later")["status"], "pending");
    assert!(
        interrupted["plan"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["working_directory"] == harness.directory.path().to_str().unwrap())
    );

    let other_directory = harness.directory.path().join("other-invocation");
    fs::create_dir(&other_directory).unwrap();
    harness.configure(&[]);
    let resumed = harness
        .command()
        .current_dir(&other_directory)
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--resume",
            state_path.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .expect("resume relay from another directory");
    run_document(&resumed);

    let requests = harness.requests();
    assert_eq!(request_names(&requests), ["waiting", "later"]);
    assert_eq!(
        requests[1]["working_directory"],
        harness.directory.path().to_str().unwrap()
    );
}

#[test]
fn resume_accepts_persisted_supervisor_paths_from_a_different_tmpdir() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "cancelled")]);
    let plan = harness.plan(
        "cross-tmpdir-resume",
        json!([task("waiting", &[]), task("later", &[])]),
    );
    let state_path = harness.state("cross-tmpdir-resume");
    run_document(&harness.run_json(&plan, &state_path));
    let interrupted = read_state(&state_path);
    let prior_supervisor_path = PathBuf::from(
        record(&interrupted, "waiting")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    assert!(prior_supervisor_path.starts_with(&harness.temporary_root));
    assert_eq!(record(&interrupted, "later")["status"], "pending");

    let resume_tmpdir = harness.directory.path().join("resume-tmp");
    fs::create_dir(&resume_tmpdir).unwrap();
    harness.configure(&[]);
    let output = harness
        .command()
        .env("TMPDIR", &resume_tmpdir)
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--resume",
            state_path.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .expect("resume relay with changed TMPDIR");
    run_document(&output);

    let resumed = read_state(&state_path);
    assert_eq!(record(&resumed, "waiting")["status"], "cancelled");
    assert_eq!(record(&resumed, "later")["status"], "succeeded");
    assert_eq!(resumed["cancellation_requested"], false);
    assert_eq!(request_names(&harness.requests()), ["waiting", "later"]);
    assert_eq!(
        record(&resumed, "waiting")["supervisor_state_path"],
        prior_supervisor_path.to_str().unwrap()
    );
    assert!(
        Path::new(
            record(&resumed, "later")["supervisor_state_path"]
                .as_str()
                .unwrap()
        )
        .starts_with(&resume_tmpdir)
    );
}

#[test]
fn resume_with_redacted_arguments_is_rejected_without_launching() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "cancelled")]);
    let mut later = task("later", &[]);
    later["arguments"] = json!(["sensitive"]);
    later["redact_argument_indexes"] = json!([0]);
    let plan = harness.plan("redacted-resume", json!([task("waiting", &[]), later]));
    let state_path = harness.state("redacted-resume");
    run_document(&harness.run_json(&plan, &state_path));
    let before = harness.requests().len();

    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before);
    assert_eq!(
        record(&read_state(&state_path), "later")["status"],
        "pending"
    );
}

#[test]
fn resume_rejects_inconsistent_task_summaries() {
    let harness = Harness::new();
    harness.configure(&[("cancelled", "cancelled")]);
    let plan = harness.plan(
        "summary-invariants",
        json!([task("cancelled", &[]), task("pending", &[])]),
    );
    let state_path = harness.state("summary-invariants");
    run_document(&harness.run_json(&plan, &state_path));
    let original = read_state(&state_path);
    let before = harness.requests().len();

    let mut missing = original.clone();
    missing["tasks"][0]["summary"] = Value::Null;
    fs::write(&state_path, serde_json::to_vec_pretty(&missing).unwrap()).unwrap();
    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");

    let mut stale = original.clone();
    let cancelled_summary = stale["tasks"][0]["summary"].clone();
    stale["tasks"][1]["summary"] = cancelled_summary;
    fs::write(&state_path, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");

    let mut wrong_status = original.clone();
    wrong_status["tasks"][0]["status"] = json!("succeeded");
    fs::write(
        &state_path,
        serde_json::to_vec_pretty(&wrong_status).unwrap(),
    )
    .unwrap();
    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicts with its execution summary")
    );

    let mut nonterminal = original;
    nonterminal["tasks"][0]["summary"]["lifecycle"] = json!("running");
    fs::write(
        &state_path,
        serde_json::to_vec_pretty(&nonterminal).unwrap(),
    )
    .unwrap();
    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid execution summary")
    );
    assert_eq!(harness.requests().len(), before);
}

#[test]
fn resume_rejects_inconsistent_dependency_transitions() {
    let harness = Harness::new();
    harness.configure(&[("failed", "nonzero")]);
    let plan = harness.plan(
        "pending-after-failure",
        json!([task("failed", &[]), task("child", &["failed"])]),
    );
    let state_path = harness.state("pending-after-failure");
    run_document(&harness.run_json(&plan, &state_path));
    let before = harness.requests().len();
    let mut pending = read_state(&state_path);
    pending["status"] = json!("running");
    pending["tasks"][1]["status"] = json!("pending");
    pending["tasks"][1]["reason"] = Value::Null;
    fs::write(&state_path, serde_json::to_vec_pretty(&pending).unwrap()).unwrap();
    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("pending task")
    );
    assert_eq!(harness.requests().len(), before);

    let harness = Harness::new();
    let plan = harness.plan(
        "blocked-after-success",
        json!([task("first", &[]), task("child", &["first"])]),
    );
    let state_path = harness.state("blocked-after-success");
    run_document(&harness.run_json(&plan, &state_path));
    let before = harness.requests().len();
    let mut blocked = read_state(&state_path);
    blocked["status"] = json!("failed");
    blocked["tasks"][1]["status"] = json!("blocked");
    blocked["tasks"][1]["reason"] = json!("dependency first did not succeed");
    blocked["tasks"][1]["attempt_run_id"] = Value::Null;
    blocked["tasks"][1]["supervisor_state_path"] = Value::Null;
    blocked["tasks"][1]["summary"] = Value::Null;
    fs::write(&state_path, serde_json::to_vec_pretty(&blocked).unwrap()).unwrap();
    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("blocked task")
    );
    assert_eq!(harness.requests().len(), before);
}

#[test]
fn resume_accepts_a_recorded_unsuccessful_dependency_reason_after_transitive_blocking() {
    let harness = Harness::new();
    harness.configure(&[("failed", "nonzero")]);
    let plan = harness.plan(
        "stable-block-reason",
        json!([
            task("failed", &[]),
            task("target", &["earlier", "failed"]),
            task("earlier", &["failed"])
        ]),
    );
    let state_path = harness.state("stable-block-reason");
    run_document(&harness.run_json(&plan, &state_path));
    let state = read_state(&state_path);
    assert_eq!(
        record(&state, "target")["reason"],
        "dependency failed did not succeed"
    );
    assert_eq!(record(&state, "earlier")["status"], "blocked");
    let before = harness.requests().len();

    let document = run_document(&harness.resume_json(&plan, &state_path));

    let resumed = read_state(&state_path);
    assert_eq!(document["data"]["run"], resumed);
    assert_eq!(
        record(&resumed, "target")["reason"],
        "dependency failed did not succeed"
    );
    assert_eq!(record(&resumed, "earlier")["status"], "blocked");
    assert_eq!(harness.requests().len(), before);
}

#[test]
fn resume_rejects_changed_plans_and_indeterminate_or_running_state() {
    let harness = Harness::new();
    harness.configure(&[("broken", "malformed")]);
    let indeterminate_plan = harness.plan(
        "indeterminate",
        json!([task("broken", &[]), task("pending", &[])]),
    );
    let indeterminate_state = harness.state("indeterminate");
    let initial_error =
        error_document(&harness.run_json(&indeterminate_plan, &indeterminate_state));
    assert_eq!(initial_error["error"]["code"], "run_supervisor");
    let state = read_state(&indeterminate_state);
    assert_eq!(state["status"], "indeterminate");
    assert_eq!(record(&state, "broken")["status"], "indeterminate");
    assert_eq!(record(&state, "pending")["status"], "pending");
    let before = harness.requests().len();
    let unsafe_resume =
        error_document(&harness.resume_json(&indeterminate_plan, &indeterminate_state));
    assert_eq!(unsafe_resume["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before);

    harness.configure(&[("holding", "hold")]);
    let running_plan = harness.plan("running", json!([task("holding", &[])]));
    let running_state = harness.state("running");
    let mut running = harness.spawn_json(&running_plan, &running_state);
    harness.wait_for_requests(before + 1);
    assert_eq!(unsafe { libc::kill(running.id() as i32, libc::SIGKILL) }, 0);
    running.wait().unwrap();
    let running_snapshot = read_state(&running_state);
    assert_eq!(record(&running_snapshot, "holding")["status"], "running");
    let abandoned_supervisor_state = PathBuf::from(
        record(&running_snapshot, "holding")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    let running_resume = error_document(&harness.resume_json(&running_plan, &running_state));
    assert_eq!(running_resume["error"]["code"], "run_state_invalid");
    let fake_pid_path = PathBuf::from(format!("{}.pid", abandoned_supervisor_state.display()));
    let pid_deadline = Instant::now() + Duration::from_secs(1);
    while !fake_pid_path.exists() {
        assert!(
            Instant::now() < pid_deadline,
            "fake supervisor PID was not recorded"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let fake_pid: i32 = fs::read_to_string(&fake_pid_path).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(fake_pid, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !abandoned_supervisor_state.exists() {
        assert!(
            Instant::now() < deadline,
            "orphaned fake supervisor did not finish"
        );
        thread::sleep(Duration::from_millis(10));
    }

    harness.configure(&[]);
    let completed_plan = harness.plan("original", json!([task("original", &[])]));
    let completed_state = harness.state("original");
    run_document(&harness.run_json(&completed_plan, &completed_state));
    let changed_plan = harness.plan("changed", json!([task("changed", &[])]));
    let mismatch = error_document(&harness.resume_json(&changed_plan, &completed_state));
    assert_eq!(mismatch["error"]["code"], "run_state_invalid");
}

#[test]
fn one_aggregate_state_path_allows_only_one_coordinator() {
    let harness = Harness::new();
    harness.configure(&[("holding", "hold")]);
    let plan = harness.plan("busy", json!([task("holding", &[])]));
    let state_path = harness.state("busy");
    let first = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(1);

    let second = harness.run_json(&plan, &state_path);
    let error = error_document(&second);
    assert_eq!(error["error"]["code"], "run_busy");
    assert_eq!(harness.requests().len(), 1);

    let first_output = terminate(first);
    run_document(&first_output);
}

#[cfg(debug_assertions)]
#[test]
fn race_created_initial_state_is_not_overwritten() {
    let harness = Harness::new();
    let plan = harness.plan("initial-collision", json!([task("never", &[])]));
    let state_path = harness.state("initial-collision");
    let marker = harness.directory.path().join("initial-collision.marker");
    let release = harness.directory.path().join("initial-collision.release");
    let child = harness
        .command()
        .env("RELAY_TEST_RUN_PAUSE", "before_initial_install")
        .env("RELAY_TEST_RUN_MARKER", &marker)
        .env("RELAY_TEST_RUN_RELEASE", &release)
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--state-path",
            state_path.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn paused coordinator");
    wait_for_path(&marker);

    let collision = b"created by another process\n";
    fs::write(&state_path, collision).unwrap();
    fs::write(&release, b"release\n").unwrap();
    let output = child.wait_with_output().unwrap();

    let error = error_document(&output);
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert_eq!(fs::read(&state_path).unwrap(), collision);
    assert!(harness.requests().is_empty());
}

#[cfg(not(debug_assertions))]
#[test]
fn release_build_ignores_orchestration_pause_environment() {
    for pause in [
        "before_initial_install",
        "before_supervisor_launch",
        "after_supervisor_result",
    ] {
        let harness = Harness::new();
        let plan = harness.plan(pause, json!([task("only", &[])]));
        let state_path = harness.state(pause);
        let marker = harness.directory.path().join(format!("{pause}.marker"));
        let release = harness.directory.path().join(format!("{pause}.release"));
        fs::write(&release, b"release\n").unwrap();

        let output = harness
            .command()
            .env("RELAY_TEST_RUN_PAUSE", pause)
            .env("RELAY_TEST_RUN_MARKER", &marker)
            .env("RELAY_TEST_RUN_RELEASE", &release)
            .args([
                "--json",
                "run",
                "--supervisor",
                harness.supervisor.to_str().unwrap(),
                "--state-path",
                state_path.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .output()
            .expect("run release coordinator with inherited pause variables");

        run_document(&output);
        assert!(!marker.exists(), "release build reacted at {pause}");
        assert_eq!(read_state(&state_path)["status"], "completed");
    }
}

#[cfg(debug_assertions)]
#[test]
fn cancellation_before_launch_and_after_result_preserves_resumable_work() {
    for pause in ["before_supervisor_launch", "after_supervisor_result"] {
        let harness = Harness::new();
        let plan = harness.plan(pause, json!([task("first", &[]), task("later", &[])]));
        let state_path = harness.state(pause);
        let marker = harness.directory.path().join(format!("{pause}.marker"));
        let release = harness.directory.path().join(format!("{pause}.release"));
        let child = harness
            .command()
            .env("RELAY_TEST_RUN_PAUSE", pause)
            .env("RELAY_TEST_RUN_MARKER", &marker)
            .env("RELAY_TEST_RUN_RELEASE", &release)
            .args([
                "--json",
                "run",
                "--supervisor",
                harness.supervisor.to_str().unwrap(),
                "--state-path",
                state_path.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn paused coordinator");
        wait_for_path(&marker);
        let signal = if pause == "before_supervisor_launch" {
            libc::SIGINT
        } else {
            libc::SIGTERM
        };
        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        fs::write(&release, b"release\n").unwrap();
        run_document(&child.wait_with_output().unwrap());

        let expected_launches = usize::from(pause == "after_supervisor_result");
        assert_eq!(harness.requests().len(), expected_launches, "pause {pause}");
        let interrupted = read_state(&state_path);
        assert_eq!(interrupted["status"], "cancelled", "pause {pause}");
        assert_eq!(interrupted["cancellation_requested"], true, "pause {pause}");
        assert_eq!(
            record(&interrupted, "first")["status"],
            if pause == "after_supervisor_result" {
                "succeeded"
            } else {
                "pending"
            },
            "pause {pause}"
        );
        assert_eq!(record(&interrupted, "later")["status"], "pending");
        assert!(
            records(&interrupted)
                .iter()
                .all(|task| task["status"] != "running"),
            "running task remained at {pause}"
        );

        run_document(&harness.resume_json(&plan, &state_path));
        let resumed = read_state(&state_path);
        assert_eq!(resumed["status"], "completed", "pause {pause}");
        assert_eq!(resumed["cancellation_requested"], false, "pause {pause}");
        assert_eq!(record(&resumed, "first")["status"], "succeeded");
        assert_eq!(record(&resumed, "later")["status"], "succeeded");
        assert_eq!(harness.requests().len(), 2, "pause {pause}");
    }
}

#[test]
fn malformed_and_remote_supervisor_responses_persist_indeterminate_state() {
    for (mode, name) in [("malformed", "malformed"), ("remote", "transport")] {
        let harness = Harness::new();
        harness.configure(&[(name, mode)]);
        let plan = harness.plan(name, json!([task(name, &[]), task("later", &[])]));
        let state_path = harness.state(name);

        let error = error_document(&harness.run_json(&plan, &state_path));
        assert_eq!(error["error"]["code"], "run_supervisor");

        assert_eq!(request_names(&harness.requests()), [name]);
        let state = read_state(&state_path);
        let run_id = state["run_id"].as_str().expect("aggregate run ID");
        let canonical_state_path = state["state_path"]
            .as_str()
            .expect("canonical aggregate state path");
        assert_eq!(error["data"]["state_path"], canonical_state_path);
        assert_eq!(error["data"]["run"], state);
        let message = error["error"]["message"]
            .as_str()
            .expect("indeterminate error message");
        assert!(message.contains(run_id));
        assert!(message.contains(canonical_state_path));
        assert_eq!(state["status"], "indeterminate");
        assert_eq!(record(&state, name)["status"], "indeterminate");
        assert_eq!(record(&state, "later")["status"], "pending");
        assert!(record(&state, name)["reason"].as_str().is_some());
    }
}

#[test]
fn rejects_unsafe_aggregate_paths_before_launch() {
    let harness = Harness::new();
    let plan = harness.plan("paths", json!([task("safe", &[])]));

    let relative = harness
        .command()
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--state-path",
            "relative.json",
            plan.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    error_document(&relative);

    let relay_directory = harness.directory.path().join(".relay");
    let runs_directory = relay_directory.join("runs");
    fs::create_dir_all(&runs_directory).unwrap();
    let lexical = harness.run_json(&plan, &relay_directory.join("aggregate.json"));
    error_document(&lexical);
    let traversal = harness.run_json(&plan, &runs_directory.join("../escaped.json"));
    error_document(&traversal);

    let other_relay = harness.directory.path().join("other/.relay/runs");
    fs::create_dir_all(&other_relay).unwrap();
    let other_tree = harness.run_json(&plan, &other_relay.join("aggregate.json"));
    error_document(&other_tree);

    let linked_target = harness.directory.path().join("linked-target");
    fs::create_dir(&linked_target).unwrap();
    let linked_runs_child = runs_directory.join("linked");
    symlink(&linked_target, &linked_runs_child).unwrap();
    let confined = harness.run_json(&plan, &linked_runs_child.join("aggregate.json"));
    error_document(&confined);

    let linked_directory = harness.directory.path().join("linked-state-parent");
    symlink(&relay_directory, &linked_directory).unwrap();
    let resolved = harness.run_json(&plan, &linked_directory.join("aggregate.json"));
    error_document(&resolved);

    let target = harness.directory.path().join("existing-target.json");
    fs::write(&target, b"unchanged").unwrap();
    let linked_file = harness.directory.path().join("linked-state.json");
    symlink(&target, &linked_file).unwrap();
    let symlink_output = harness.run_json(&plan, &linked_file);
    error_document(&symlink_output);
    assert_eq!(fs::read(&target).unwrap(), b"unchanged");
    assert!(harness.requests().is_empty());
}

#[test]
fn renamed_parent_symlink_swap_cannot_redirect_aggregate_updates() {
    let harness = Harness::new();
    harness.configure(&[("holding", "hold")]);
    let plan = harness.plan(
        "parent-swap",
        json!([task("holding", &[]), task("later", &[])]),
    );
    let relay_directory = harness.directory.path().join(".relay");
    let runs_directory = relay_directory.join("runs");
    fs::create_dir_all(&runs_directory).unwrap();
    let state_path = runs_directory.join("aggregate.json");
    let child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(1);

    let moved_runs = relay_directory.join("runs-before-swap");
    let redirected = harness.directory.path().join("redirected-writes");
    fs::create_dir(&redirected).unwrap();
    fs::rename(&runs_directory, &moved_runs).unwrap();
    symlink(&redirected, &runs_directory).unwrap();

    let error = error_document(&terminate(child));
    assert_eq!(error["error"]["code"], "run_state_invalid");

    assert!(!redirected.join("aggregate.json").exists());
    assert_eq!(request_names(&harness.requests()), ["holding"]);
}

#[test]
fn accepts_explicit_aggregate_path_in_project_runs_directory() {
    let harness = Harness::new();
    let plan = harness.plan("project-runs", json!([task("safe", &[])]));
    let state_path = harness.directory.path().join(".relay/runs/explicit.json");

    let document = run_document(&harness.run_json(&plan, &state_path));

    assert_eq!(document["data"]["state_path"], state_path.to_str().unwrap());
    assert_eq!(read_state(&state_path)["status"], "completed");
}

#[test]
fn human_and_json_outputs_are_framed_and_run_ignores_the_project_lock() {
    let harness = Harness::new();
    let plan = harness.plan("framing", json!([task("framing", &[])]));
    let json_state = harness.state("json-framing");
    let json_output = harness.run_json(&plan, &json_state);
    let document = run_document(&json_output);
    assert_eq!(document["data"]["state_path"], json_state.to_str().unwrap());

    let relay_directory = harness.directory.path().join(".relay");
    fs::create_dir_all(&relay_directory).unwrap();
    let project_lock_path = relay_directory.join("lock");
    let project_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&project_lock_path)
        .unwrap();
    project_lock.lock_exclusive().unwrap();

    let human_state = harness.state("human-framing");
    let mut human_child = harness
        .command()
        .args([
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--state-path",
            human_state.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if human_child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            FileExt::unlock(&project_lock).unwrap();
            human_child.kill().unwrap();
            human_child.wait().unwrap();
            panic!("run waited for the unrelated project lock");
        }
        thread::sleep(Duration::from_millis(10));
    }
    FileExt::unlock(&project_lock).unwrap();
    let human = human_child.wait_with_output().unwrap();
    assert!(
        human.status.success(),
        "human run failed: {}",
        String::from_utf8_lossy(&human.stderr)
    );
    assert!(human.stderr.is_empty());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("Run"));
    assert!(text.contains("completed"));
    assert!(text.contains("framing"));
    assert!(text.contains(human_state.to_str().unwrap()));
    assert!(!text.trim_start().starts_with('{'));
}

#[test]
fn human_output_exit_status_tracks_completed_failed_cancelled_and_indeterminate_runs() {
    for (name, mode, tasks, expected_status, succeeds) in [
        (
            "human-success",
            "success",
            json!([task("human-success", &[])]),
            "completed",
            true,
        ),
        (
            "human-failure",
            "nonzero",
            json!([
                task("human-failure", &[]),
                task("blocked-child", &["human-failure"])
            ]),
            "failed",
            false,
        ),
        (
            "human-cancelled",
            "cancelled",
            json!([task("human-cancelled", &[])]),
            "cancelled",
            false,
        ),
    ] {
        let harness = Harness::new();
        harness.configure(&[(name, mode)]);
        let plan = harness.plan(name, tasks);
        let state_path = harness.state(name);

        let output = harness.run_human(&plan, &state_path);

        assert_eq!(
            output.status.code(),
            Some(if succeeds { 0 } else { 1 }),
            "case {name}"
        );
        assert!(output.stderr.is_empty(), "case {name}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains(&format!(": {expected_status}")),
            "case {name}"
        );
        assert!(text.contains(state_path.to_str().unwrap()), "case {name}");
        let state = read_state(&state_path);
        assert_eq!(state["status"], expected_status, "case {name}");
        if name == "human-failure" {
            assert_eq!(record(&state, "human-failure")["status"], "failed");
            assert_eq!(record(&state, "blocked-child")["status"], "blocked");
        }
    }

    let harness = Harness::new();
    harness.configure(&[("human-indeterminate", "malformed")]);
    let plan = harness.plan(
        "human-indeterminate",
        json!([task("human-indeterminate", &[])]),
    );
    let state_path = harness.state("human-indeterminate");

    let output = harness.run_human(&plan, &state_path);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.starts_with("error: supervisor failed for run "));
    assert!(error.contains(state_path.to_str().unwrap()));
    let state = read_state(&state_path);
    assert_eq!(state["status"], "indeterminate");
    assert_eq!(
        record(&state, "human-indeterminate")["status"],
        "indeterminate"
    );
}
