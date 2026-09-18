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
import fcntl

cancelled = False
def cancel(_signal, _frame):
    global cancelled
    cancelled = True
signal.signal(signal.SIGINT, cancel)
signal.signal(signal.SIGTERM, cancel)
legacy = os.environ.get("RELAY_FAKE_LEGACY") == "1"
if os.environ.get("RELAY_FAKE_TRANSPORT") == "1":
    time.sleep(0.05)
    sys.exit(0)
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
name = os.path.basename(request["program"])
with open(control_path, "r+", encoding="utf-8") as control_file:
    fcntl.flock(control_file, fcntl.LOCK_EX)
    control = json.load(control_file)
    configured = control.get(name, "success")
    if isinstance(configured, list):
        mode = configured.pop(0) if configured else "success"
        control[name] = configured
        control_file.seek(0)
        control_file.truncate()
        json.dump(control, control_file, separators=(",", ":"))
        control_file.flush()
        os.fsync(control_file.fileno())
    else:
        mode = configured
    fcntl.flock(control_file, fcntl.LOCK_UN)
record = json.dumps(request, separators=(",", ":")) + "\n"
fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
try:
    os.write(fd, record.encode("utf-8"))
finally:
    os.close(fd)

with open(request["state_path"] + ".pid", "w", encoding="utf-8") as pid_file:
    pid_file.write(str(os.getpid()))
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

if mode == "barrier":
    barrier_directory = os.environ.get("RELAY_FAKE_BARRIER_DIR")
    if not barrier_directory:
        sys.stderr.write("RELAY_FAKE_BARRIER_DIR is required\n")
        sys.exit(93)
    os.makedirs(barrier_directory, exist_ok=True)
    with open(os.path.join(barrier_directory, "started-" + name), "w", encoding="utf-8") as marker:
        marker.write(str(os.getpid()))
    release = os.path.join(barrier_directory, "release")
    while not cancelled and not os.path.exists(release):
        time.sleep(0.01)
    mode = "cancelled" if cancelled else "success"

for variable in request.get("environment", []):
    if variable.get("name") != "RELAY_OUTPUT_MANIFEST":
        continue
    for output_path in json.loads(variable["value"]).values():
        os.makedirs(os.path.dirname(output_path), exist_ok=True)
        payload = b"null\n" if mode == "null_output" else (b"x" * 4097 if mode == "oversized_output" else (b"{not-json\n" if mode == "malformed_output" else (b"{\"source\":\"fake\"}\n" if output_path.endswith(".json") else b"fake report\n")))
        if mode == "symlink_output":
            target = output_path + ".target"
            with open(target, "wb") as output_file:
                output_file.write(payload)
            os.symlink(target, output_path)
        elif mode == "hardlink_output":
            target = output_path + ".target"
            with open(target, "wb") as output_file:
                output_file.write(payload)
            os.link(target, output_path)
        else:
            with open(output_path, "wb") as output_file:
                output_file.write(payload)
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
    json.dump(execution if legacy else {"run_state_version":2,"supervisor_version":"fake-v1","state_path":state_path,"execution":execution}, destination, separators=(",", ":"))
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

    fn configure_sequences(&self, outcomes: &[(&str, &[&str])]) {
        let value = Value::Object(
            outcomes
                .iter()
                .map(|(task, sequence)| ((*task).to_owned(), json!(sequence)))
                .collect(),
        );
        fs::write(&self.control, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn plan(&self, name: &str, tasks: Value) -> PathBuf {
        self.plan_with_schema(name, 1, tasks)
    }

    fn plan_v2(&self, name: &str, tasks: Value) -> PathBuf {
        self.plan_with_schema(name, 2, tasks)
    }

    fn plan_v3(&self, name: &str, tasks: Value) -> PathBuf {
        self.plan_with_schema(name, 3, tasks)
    }

    fn plan_with_schema(&self, name: &str, schema_version: u32, tasks: Value) -> PathBuf {
        let path = self.directory.path().join(format!("{name}.json"));
        fs::write(
            &path,
            serde_json::to_vec(&json!({"schema_version":schema_version,"tasks":tasks})).unwrap(),
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
        self.run_json_jobs(plan, state, 1)
    }

    fn run_json_jobs(&self, plan: &Path, state: &Path, jobs: usize) -> Output {
        let jobs = jobs.to_string();
        self.command()
            .args([
                "--json",
                "run",
                "--jobs",
                jobs.as_str(),
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
        self.resume_json_jobs(plan, state, 1)
    }

    fn resume_json_jobs(&self, plan: &Path, state: &Path, jobs: usize) -> Output {
        let jobs = jobs.to_string();
        self.command()
            .args([
                "--json",
                "run",
                "--jobs",
                jobs.as_str(),
                "--supervisor",
                self.supervisor.to_str().unwrap(),
                "--resume",
                state.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .output()
            .expect("resume relay coordinator")
    }
    fn workflow_file(&self, name: &str, definition: &Value) -> PathBuf {
        let directory = self.directory.path().join("relay-workflows");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{name}.json"));
        fs::write(&path, serde_json::to_vec_pretty(definition).unwrap()).unwrap();
        path
    }

    fn workflow_run_json(&self, name: &str, state: &Path, inputs: &[&str]) -> Output {
        let mut command = self.command();
        command.args([
            "--json",
            "--scope",
            "current",
            "workflow",
            "run",
            name,
            "--supervisor",
            self.supervisor.to_str().unwrap(),
            "--state-path",
            state.to_str().unwrap(),
        ]);
        for input in inputs {
            command.args(["--input", input]);
        }
        command.output().expect("run reusable workflow")
    }
    fn workflow_run_json_jobs(
        &self,
        name: &str,
        state: &Path,
        jobs: usize,
        inputs: &[&str],
    ) -> Output {
        let jobs = jobs.to_string();
        let mut command = self.command();
        command.args([
            "--json",
            "--scope",
            "current",
            "workflow",
            "run",
            name,
            "--jobs",
            jobs.as_str(),
            "--supervisor",
            self.supervisor.to_str().unwrap(),
            "--state-path",
            state.to_str().unwrap(),
        ]);
        for input in inputs {
            command.args(["--input", input]);
        }
        command.output().expect("run reusable workflow with jobs")
    }

    fn workflow_validate_json(&self, name: &str, inputs: &[&str]) -> Output {
        let mut command = self.command();
        command.args(["--json", "--scope", "current", "workflow", "validate", name]);
        for input in inputs {
            command.args(["--input", input]);
        }
        command.output().expect("validate reusable workflow")
    }

    fn workflow_show_json(&self, name: &str) -> Output {
        self.command()
            .args(["--json", "--scope", "current", "workflow", "show", name])
            .output()
            .expect("show reusable workflow")
    }

    fn workflow_list_json(&self) -> Output {
        self.command()
            .args(["--json", "--scope", "current", "workflow", "list"])
            .output()
            .expect("list reusable workflows")
    }

    fn spawn_workflow_json(&self, name: &str, state: &Path, marker: &Path) -> Child {
        let jobs = "1";
        let release = marker.with_extension("release");
        self.command()
            .env("RELAY_TEST_RUN_PAUSE", "after_supervisor_result")
            .env("RELAY_TEST_RUN_MARKER", marker)
            .env("RELAY_TEST_RUN_RELEASE", release)
            .args([
                "--json",
                "--scope",
                "current",
                "workflow",
                "run",
                name,
                "--jobs",
                jobs,
                "--supervisor",
                self.supervisor.to_str().unwrap(),
                "--state-path",
                state.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn reusable workflow")
    }
    fn spawn_workflow_without_pause(&self, name: &str, state: &Path) -> Child {
        self.command()
            .args([
                "--json",
                "--scope",
                "current",
                "workflow",
                "run",
                name,
                "--jobs",
                "1",
                "--supervisor",
                self.supervisor.to_str().unwrap(),
                "--state-path",
                state.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn reusable workflow without pause")
    }

    fn resume_workflow_json(&self, name: &str, state: &Path) -> Output {
        self.command()
            .args([
                "--json",
                "--scope",
                "current",
                "workflow",
                "run",
                name,
                "--jobs",
                "1",
                "--supervisor",
                self.supervisor.to_str().unwrap(),
                "--resume",
                state.to_str().unwrap(),
            ])
            .output()
            .expect("resume reusable workflow")
    }

    fn spawn_json(&self, plan: &Path, state: &Path) -> Child {
        self.spawn_json_jobs(plan, state, 1)
    }

    fn spawn_json_jobs(&self, plan: &Path, state: &Path, jobs: usize) -> Child {
        let jobs = jobs.to_string();
        self.command()
            .args([
                "--json",
                "run",
                "--jobs",
                jobs.as_str(),
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

    fn spawn_json_jobs_with_barrier(
        &self,
        plan: &Path,
        state: &Path,
        jobs: usize,
        barrier: &Path,
    ) -> Child {
        let jobs = jobs.to_string();
        self.command()
            .env("RELAY_FAKE_BARRIER_DIR", barrier)
            .args([
                "--json",
                "run",
                "--jobs",
                jobs.as_str(),
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
            .expect("spawn barrier relay coordinator")
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
fn retry_task(id: &str, dependencies: &[&str], max_attempts: u32, delay_ms: u64) -> Value {
    let mut task = task(id, dependencies);
    task["retry"] = json!({"max_attempts":max_attempts,"delay_ms":delay_ms});
    task
}
fn request_output_path(request: &Value, name: &str) -> PathBuf {
    let manifest = request["environment"]
        .as_array()
        .expect("request environment")
        .iter()
        .find(|entry| entry["name"] == "RELAY_OUTPUT_MANIFEST")
        .and_then(|entry| entry["value"].as_str())
        .expect("output manifest environment");
    let manifest: Value = serde_json::from_str(manifest).expect("valid output manifest");
    manifest[name]
        .as_str()
        .map(PathBuf::from)
        .expect("declared output environment")
}

#[test]
fn output_declarations_are_strict_and_bounded() {
    let harness = Harness::new();
    let mut cases = Vec::new();

    let mut duplicate = task("duplicate", &[]);
    duplicate["outputs"] = json!([
        {"name":"report","type":"file","path":"report.txt"},
        {"name":"report","type":"file","path":"other.txt"}
    ]);
    cases.push(("duplicate", duplicate));

    let mut unknown = task("unknown", &[]);
    unknown["outputs"] = json!([{"name":"report","type":"file","path":"report.txt","extra":true}]);
    cases.push(("unknown", unknown));

    let mut bad_name = task("bad-name", &[]);
    bad_name["outputs"] = json!([{"name":"Report","type":"file","path":"report.txt"}]);
    cases.push(("bad-name", bad_name));

    let mut traversal = task("traversal", &[]);
    traversal["outputs"] = json!([{"name":"report","type":"file","path":"../report.txt"}]);
    cases.push(("traversal", traversal));

    let mut absolute = task("absolute", &[]);
    absolute["outputs"] = json!([{"name":"report","type":"file","path":"/tmp/report.txt"}]);
    cases.push(("absolute", absolute));

    let mut wrong_type = task("wrong-type", &[]);
    wrong_type["outputs"] = json!([{"name":"report","type":"text","path":"report.txt"}]);
    cases.push(("wrong-type", wrong_type));

    let mut null_outputs = task("null-outputs", &[]);
    null_outputs["outputs"] = Value::Null;
    cases.push(("null-outputs", null_outputs));

    let mut too_many = task("too-many", &[]);
    too_many["outputs"] = json!((0..5).map(|index| json!({"name":format!("out{index}"),"type":"file","path":format!("out{index}.txt")})).collect::<Vec<_>>());
    cases.push(("too-many", too_many));

    let mut unknown_input = task("unknown-input", &[]);
    unknown_input["inputs"] = json!([{"from_task":"missing","output":"report","as":"--report"}]);
    cases.push(("unknown-input", unknown_input));

    let mut input_unknown_field = task("input-unknown-field", &[]);
    input_unknown_field["inputs"] =
        json!([{"from_task":"missing","output":"report","as":"--report","extra":true}]);
    cases.push(("input-unknown-field", input_unknown_field));

    let mut empty_argument = task("empty-argument", &[]);
    empty_argument["inputs"] = json!([{"from_task":"missing","output":"report","as":""}]);
    cases.push(("empty-argument", empty_argument));

    let mut long_path = task("long-path", &[]);
    long_path["outputs"] = json!([{"name":"report","type":"file","path":"x".repeat(513)}]);
    cases.push(("long-path", long_path));

    for (name, invalid_task) in cases {
        let plan = harness.plan_v3(name, json!([invalid_task]));
        let error = error_document(&harness.run_json(&plan, &harness.state(name)));
        assert_eq!(error["error"]["code"], "run_plan_invalid", "case {name}");
    }

    let mut producer = task("missing-output-producer", &[]);
    producer["outputs"] = json!([{"name":"report","type":"file","path":"report.txt"}]);
    let mut missing_output = task("missing-output-consumer", &["missing-output-producer"]);
    missing_output["inputs"] =
        json!([{"from_task":"missing-output-producer","output":"missing","as":"--missing"}]);
    let plan = harness.plan_v3("missing-output-ref", json!([producer, missing_output]));
    let error = error_document(&harness.run_json(&plan, &harness.state("missing-output-ref")));
    assert_eq!(error["error"]["code"], "run_plan_invalid");

    let mut producer = task("nondirect-producer", &[]);
    producer["outputs"] = json!([{"name":"report","type":"file","path":"report.txt"}]);
    let mut nondirect_consumer = task("nondirect-consumer", &[]);
    nondirect_consumer["inputs"] =
        json!([{"from_task":"nondirect-producer","output":"report","as":"--report"}]);
    let plan = harness.plan_v3("nondirect-ref", json!([producer, nondirect_consumer]));
    let error = error_document(&harness.run_json(&plan, &harness.state("nondirect-ref")));
    assert_eq!(error["error"]["code"], "run_plan_invalid");

    assert!(harness.requests().is_empty());
}

#[test]
fn schema_v2_rejects_even_empty_output_and_input_fields() {
    let harness = Harness::new();
    let mut output_task = task("v2-output-field", &[]);
    output_task["outputs"] = json!([]);
    let output_plan = harness.plan_v2("v2-output-field", json!([output_task]));
    let output_error =
        error_document(&harness.run_json(&output_plan, &harness.state("v2-output-field")));
    assert_eq!(output_error["error"]["code"], "run_plan_invalid");

    let mut input_task = task("v2-input-field", &[]);
    input_task["inputs"] = json!([]);
    let input_plan = harness.plan_v2("v2-input-field", json!([input_task]));
    let input_error =
        error_document(&harness.run_json(&input_plan, &harness.state("v2-input-field")));
    assert_eq!(input_error["error"]["code"], "run_plan_invalid");
    assert!(harness.requests().is_empty());
}
#[test]
fn successful_outputs_are_provenanced_consumed_and_resume_safe() {
    let harness = Harness::new();
    let mut producer = task("produce", &[]);
    producer["outputs"] = json!([
        {"name":"summary","type":"json","path":"summary.json"},
        {"name":"report","type":"file","path":"report.txt"}
    ]);
    let mut consumer = task("consume", &["produce"]);
    consumer["inputs"] = json!([
        {"from_task":"produce","output":"summary","as":"--summary"},
        {"from_task":"produce","output":"report","as":"--report"}
    ]);
    let plan = harness.plan_v3("outputs-success", json!([producer, consumer]));
    let state_path = harness.state("outputs-success");
    let document = run_document(&harness.run_json(&plan, &state_path));
    let state = read_state(&state_path);
    let producer_state = record(&state, "produce");
    let outputs = producer_state["outputs"]
        .as_array()
        .expect("task outputs")
        .to_owned();
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0]["type"], "json");
    assert_eq!(outputs[0]["value"]["source"], "fake");
    assert_eq!(outputs[0]["producer_task"], "produce");
    assert_eq!(outputs[0]["run_id"], state["run_id"]);
    assert_eq!(outputs[0]["attempt"], 1);
    assert_eq!(
        outputs[0]["attempt_run_id"],
        attempts(&state, "produce")[0]["run_id"]
    );
    assert!(outputs[0]["sha256"].as_str().unwrap().len() == 64);
    assert!(Path::new(outputs[1]["path"].as_str().unwrap()).is_file());
    assert_eq!(document["data"]["run"], state);

    let requests = harness.requests();
    assert_eq!(requests.len(), 2);
    let consumer_arguments = requests[1]["arguments"].as_array().expect("consumer argv");
    assert!(consumer_arguments.iter().any(|value| value == "--summary"));
    assert!(
        consumer_arguments
            .iter()
            .any(|value| value == "{\"source\":\"fake\"}")
    );
    assert!(consumer_arguments.iter().any(|value| value == "--report"));
    assert!(
        consumer_arguments
            .iter()
            .any(|value| value == outputs[1]["path"].as_str().unwrap())
    );

    let before_resume = requests.len();
    let resumed = run_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(harness.requests().len(), before_resume);
    assert_eq!(
        resumed["data"]["run"]["tasks"][0]["outputs"],
        json!(outputs)
    );
}

#[test]
fn failed_attempt_outputs_never_cross_retry_boundary() {
    let harness = Harness::new();
    harness.configure_sequences(&[("produce", &["nonzero", "success"])]);
    let mut producer = task("produce", &[]);
    producer["outputs"] = json!([{"name":"summary","type":"json","path":"summary.json"}]);
    producer["retry"] = json!({"max_attempts":2,"delay_ms":0});
    let plan = harness.plan_v3("outputs-retry", json!([producer]));
    let state_path = harness.state("outputs-retry");
    run_document(&harness.run_json(&plan, &state_path));
    let state = read_state(&state_path);
    let history = attempts(&state, "produce");
    assert_eq!(history.len(), 2);
    assert!(history[0]["outputs"].is_null());
    assert_eq!(history[1]["outputs"].as_array().unwrap().len(), 1);
    assert_eq!(record(&state, "produce")["outputs"][0]["attempt"], 2);
    let requests = harness.requests();
    let first_path = request_output_path(&requests[0], "summary");
    let final_path = request_output_path(&requests[1], "summary");
    assert_ne!(first_path, final_path);
    assert!(first_path.is_file());
    assert_eq!(
        record(&state, "produce")["outputs"][0]["path"],
        final_path.to_str().unwrap()
    );
}

#[test]
fn malformed_success_output_becomes_indeterminate_and_unconsumable() {
    let harness = Harness::new();
    harness.configure(&[("produce", "malformed_output")]);
    let mut producer = task("produce", &[]);
    producer["outputs"] = json!([{"name":"summary","type":"json","path":"summary.json"}]);
    let mut consumer = task("consume", &["produce"]);
    consumer["inputs"] = json!([{"from_task":"produce","output":"summary","as":"--summary"}]);
    let plan = harness.plan_v3("outputs-malformed", json!([producer, consumer]));
    let state_path = harness.state("outputs-malformed");
    let document = run_document(&harness.run_json(&plan, &state_path));
    let state = read_state(&state_path);
    assert_eq!(document["data"]["run"]["status"], "indeterminate");
    assert_eq!(record(&state, "produce")["status"], "indeterminate");
    assert!(record(&state, "produce")["outputs"].is_null());
    let before = harness.requests().len();
    assert_eq!(before, 1);
    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before);
}
#[test]
fn symlink_and_hardlink_outputs_are_not_trusted() {
    let harness = Harness::new();
    for (name, mode) in [
        ("symlink", "symlink_output"),
        ("hardlink", "hardlink_output"),
    ] {
        harness.configure(&[(name, mode)]);
        let mut producer = task(name, &[]);
        producer["outputs"] = json!([{"name":"report","type":"file","path":"report.txt"}]);
        let plan = harness.plan_v3(name, json!([producer]));
        let state_path = harness.state(name);
        let document = run_document(&harness.run_json(&plan, &state_path));
        let state = read_state(&state_path);
        assert_eq!(document["data"]["run"]["status"], "indeterminate");
        assert_eq!(record(&state, name)["status"], "indeterminate");
        assert!(record(&state, name)["outputs"].is_null());
    }
}
#[test]
fn real_go_supervisor_proves_outputs_and_resume_without_rerun() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let harness = Harness::new();
    let counter = harness.directory.path().join("output-counter.log");
    let producer_script = "import json,os,sys; paths=json.loads(os.environ['RELAY_OUTPUT_MANIFEST']); open(paths['api_key'],'w').write('{\"real\":true}'); open(paths['report'],'w').write('real report\\n'); open(sys.argv[1],'a').write('producer\\n')";
    let consumer_script = "import os,sys; assert sys.argv[2] == '--summary'; assert sys.argv[3] == '{\"real\":true}'; assert sys.argv[4] == '--report'; assert os.path.isfile(sys.argv[5]); open(sys.argv[1],'a').write('consumer\\n')";
    let mut producer = task("produce", &[]);
    producer["program"] = json!("/usr/bin/python3");
    producer["arguments"] = json!(["-c", producer_script, counter.to_str().unwrap()]);
    producer["outputs"] = json!([
        {"name":"api_key","type":"json","path":"summary.json"},
        {"name":"report","type":"file","path":"report.txt"}
    ]);
    let mut consumer = task("consume", &["produce"]);
    consumer["program"] = json!("/usr/bin/python3");
    consumer["arguments"] = json!(["-c", consumer_script, counter.to_str().unwrap()]);
    consumer["inputs"] = json!([
        {"from_task":"produce","output":"api_key","as":"--summary"},
        {"from_task":"produce","output":"report","as":"--report"}
    ]);
    let plan = harness.plan_v3("real-outputs", json!([producer, consumer]));
    let state = harness.state("real-outputs");
    run_document(
        &real_relay_command(
            &supervisor,
            harness.directory.path(),
            &harness.temporary_root,
        )
        .args([
            "--jobs",
            "1",
            "--state-path",
            state.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .expect("run real Go supervisor"),
    );
    let first_state = read_state(&state);
    assert_eq!(first_state["status"], "completed");
    assert_eq!(first_state["tasks"][0]["outputs"][0]["value"]["real"], true);
    let before_resume = fs::read_to_string(&counter).unwrap();
    assert_eq!(before_resume.lines().count(), 2);
    run_document(
        &real_relay_command(
            &supervisor,
            harness.directory.path(),
            &harness.temporary_root,
        )
        .args([
            "--jobs",
            "1",
            "--resume",
            state.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .expect("resume real Go supervisor run"),
    );
    assert_eq!(fs::read_to_string(counter).unwrap().lines().count(), 2);
}
#[test]
fn oversized_structured_output_is_refused_without_trusting_bytes() {
    let harness = Harness::new();
    harness.configure(&[("oversized", "oversized_output")]);
    let mut producer = task("oversized", &[]);
    producer["outputs"] = json!([{"name":"summary","type":"json","path":"summary.json"}]);
    let plan = harness.plan_v3("oversized-output", json!([producer]));
    let state_path = harness.state("oversized-output");
    let document = run_document(&harness.run_json(&plan, &state_path));
    let state = read_state(&state_path);
    assert_eq!(document["data"]["run"]["status"], "indeterminate");
    assert!(record(&state, "oversized")["outputs"].is_null());
    let before = harness.requests().len();
    let error = error_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before);
}

#[test]
fn tampered_output_metadata_is_rejected_on_resume() {
    for (name, cross_project) in [("tampered-hash", false), ("tampered-path", true)] {
        let harness = Harness::new();
        let mut producer = task("produce", &[]);
        producer["outputs"] = json!([{"name":"summary","type":"json","path":"summary.json"}]);
        let plan = harness.plan_v3(name, json!([producer]));
        let state_path = harness.state(name);
        run_document(&harness.run_json(&plan, &state_path));
        let mut state = read_state(&state_path);
        if cross_project {
            state["tasks"][0]["outputs"][0]["path"] = json!("/tmp/relay-outside.json");
        } else {
            state["tasks"][0]["outputs"][0]["sha256"] = json!("0".repeat(64));
        }
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let before = harness.requests().len();
        let error = error_document(&harness.resume_json(&plan, &state_path));
        assert_eq!(error["error"]["code"], "run_state_invalid");
        assert_eq!(harness.requests().len(), before);
    }
}
#[test]
fn real_go_crash_reconciliation_promotes_outputs_once() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create crash output directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let marker = directory.path().join("output-pause.marker");
    let counter = directory.path().join("output-counter.log");
    let plan = directory.path().join("output-crash-plan.json");
    let state = directory.path().join("output-crash-state.json");
    let producer_script = "import json,os,sys; paths=json.loads(os.environ['RELAY_OUTPUT_MANIFEST']); open(paths['summary'],'w').write('{\"real\":true}'); open(sys.argv[1],'a').write('producer\\n')";
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version":3,
            "tasks":[{
                "id":"produce",
                "program":"/usr/bin/python3",
                "arguments":["-c",producer_script,counter.to_str().unwrap()],
                "timeout_ms":5000,
                "outputs":[{"name":"summary","type":"json","path":"summary.json"}]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut child = spawn_real_crash_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        &marker,
        1,
    );
    wait_for_path(&marker);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();
    let interrupted = read_state(&state);
    assert_eq!(interrupted["tasks"][0]["status"], "running");
    assert!(interrupted["tasks"][0]["outputs"].is_null());
    run_document(
        &real_relay_command(&supervisor, directory.path(), &temporary_root)
            .args([
                "--jobs",
                "1",
                "--resume",
                state.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .output()
            .expect("resume crashed output run"),
    );
    let resumed = read_state(&state);
    assert_eq!(resumed["status"], "completed");
    assert_eq!(resumed["tasks"][0]["outputs"][0]["value"]["real"], true);
    assert_eq!(fs::read_to_string(counter).unwrap().lines().count(), 1);
}
#[test]
fn human_output_lists_declared_outputs_without_hash_noise() {
    let harness = Harness::new();
    let mut producer = task("produce", &[]);
    producer["outputs"] = json!([{"name":"summary","type":"json","path":"summary.json"}]);
    let plan = harness.plan_v3("human-outputs", json!([producer]));
    let output = harness.run_human(&plan, &harness.state("human-outputs"));
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("outputs:"));
    assert!(text.contains("summary -> json"));
    assert!(!text.contains("sha256"));
}

#[test]
fn failed_cancelled_and_timeout_attempts_have_no_trusted_outputs() {
    let harness = Harness::new();
    for (name, mode, status) in [
        ("failed-output", "nonzero", "failed"),
        ("cancelled-output", "cancelled", "cancelled"),
        ("timeout-output", "timeout", "failed"),
    ] {
        harness.configure(&[(name, mode)]);
        let mut producer = task(name, &[]);
        producer["outputs"] = json!([{"name":"report","type":"file","path":"report.txt"}]);
        let plan = harness.plan_v3(name, json!([producer]));
        let state_path = harness.state(name);
        let document = run_document(&harness.run_json(&plan, &state_path));
        let state = read_state(&state_path);
        assert_eq!(document["data"]["run"]["status"], status);
        assert!(record(&state, name)["outputs"].is_null());
    }
}
#[test]
fn independent_tasks_publish_outputs_in_parallel() {
    let harness = Harness::new();
    let mut left = task("left", &[]);
    left["outputs"] = json!([{"name":"summary","type":"json","path":"left.json"}]);
    let mut right = task("right", &[]);
    right["outputs"] = json!([{"name":"summary","type":"json","path":"right.json"}]);
    let plan = harness.plan_v3("parallel-outputs", json!([left, right]));
    let state_path = harness.state("parallel-outputs");
    run_document(&harness.run_json_jobs(&plan, &state_path, 2));
    let state = read_state(&state_path);
    assert_eq!(record(&state, "left")["status"], "succeeded");
    assert_eq!(record(&state, "right")["status"], "succeeded");
    assert_eq!(
        record(&state, "left")["outputs"][0]["value"]["source"],
        "fake"
    );
    assert_eq!(
        record(&state, "right")["outputs"][0]["value"]["source"],
        "fake"
    );
    assert_eq!(harness.requests().len(), 2);
}
#[test]
fn json_null_output_survives_persistence_and_resume() {
    let harness = Harness::new();
    harness.configure(&[("null-output", "null_output")]);
    let mut producer = task("null-output", &[]);
    producer["outputs"] = json!([{"name":"summary","type":"json","path":"summary.json"}]);
    let plan = harness.plan_v3("null-output", json!([producer]));
    let state_path = harness.state("null-output");
    run_document(&harness.run_json(&plan, &state_path));
    let state = read_state(&state_path);
    assert!(record(&state, "null-output")["outputs"][0]["value"].is_null());
    let before = harness.requests().len();
    let resumed = run_document(&harness.resume_json(&plan, &state_path));
    assert!(resumed["data"]["run"]["tasks"][0]["outputs"][0]["value"].is_null());
    assert_eq!(harness.requests().len(), before);
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
fn wait_for_file_size(path: &Path, minimum: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::metadata(path).map_or(0, |metadata| metadata.len()) < minimum {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {} bytes in {}",
            minimum,
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_pid(pid_path: &Path) -> i32 {
    fs::read_to_string(pid_path)
        .expect("read supervisor PID")
        .trim()
        .parse()
        .expect("valid supervisor PID")
}

fn wait_for_process_exit(pid_path: &Path) {
    wait_for_path(pid_path);
    let pid = read_pid(pid_path);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if unsafe { libc::kill(pid, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            panic!("could not inspect supervisor PID {pid}: {error}");
        }
        assert!(
            Instant::now() < deadline,
            "supervisor PID {pid} remained alive"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn supervisor_pid_path(state: &Value, id: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}.pid",
        record(state, id)["supervisor_state_path"]
            .as_str()
            .expect("supervisor state path")
    ))
}

fn downgrade_to_version(path: &Path, schema_version: u32) {
    let mut state = read_state(path);
    state["schema_version"] = json!(schema_version);
    for task in state["tasks"].as_array_mut().unwrap() {
        task.as_object_mut().unwrap().remove("attempts");
        task.as_object_mut().unwrap().remove("retry_ready_at");
    }
    if schema_version == 1 {
        state
            .as_object_mut()
            .expect("aggregate state object")
            .remove("jobs");
    }
    fs::write(path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
}

fn downgrade_to_v1(path: &Path) {
    downgrade_to_version(path, 1);
}

fn downgrade_to_v2(path: &Path) {
    downgrade_to_version(path, 2);
}
fn downgrade_to_v3(path: &Path) {
    let mut state = read_state(path);
    state["schema_version"] = json!(3);
    fs::write(path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
}

fn wait_for_retry_pending(path: &Path, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if path.is_file() {
            let state = read_state(path);
            let task = record(&state, id);
            if task["status"] == "pending" && task["retry_ready_at"].is_string() {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for retry scheduling"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn attempts<'a>(state: &'a Value, id: &str) -> &'a [Value] {
    record(state, id)["attempts"]
        .as_array()
        .expect("retained attempt history")
}
fn wait_for_running_task(path: &Path, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if path.is_file() {
            let state = read_state(path);
            let task = record(&state, id);
            if task["status"] == "running" && task["supervisor_state_path"].is_string() {
                return state;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for running task"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn no_retry_is_default_and_first_success_has_one_attempt() {
    let harness = Harness::new();
    harness.configure(&[("failed", "nonzero")]);
    let failed_plan = harness.plan("default-failure", json!([task("failed", &[])]));
    let failed_state = harness.state("default-failure");
    run_document(&harness.run_json(&failed_plan, &failed_state));
    let failed = read_state(&failed_state);
    assert_eq!(failed["plan"]["schema_version"], 1);
    assert_eq!(failed["status"], "failed");
    assert_eq!(harness.requests().len(), 1);
    assert_eq!(attempts(&failed, "failed").len(), 1);
    assert_eq!(attempts(&failed, "failed")[0]["status"], "failed");

    harness.configure(&[]);
    let success_plan = harness.plan("default-success", json!([task("success", &[])]));
    let success_state = harness.state("default-success");
    run_document(&harness.run_json(&success_plan, &success_state));
    let success = read_state(&success_state);
    assert_eq!(record(&success, "success")["status"], "succeeded");
    assert_eq!(attempts(&success, "success").len(), 1);
}

#[test]
fn retry_fail_once_then_succeeds_with_fresh_attempt_records() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"])]);
    let plan = harness.plan_v2("retry-success", json!([retry_task("flaky", &[], 2, 0)]));
    let state_path = harness.state("retry-success");

    run_document(&harness.run_json(&plan, &state_path));

    let state = read_state(&state_path);
    let history = attempts(&state, "flaky");
    assert_eq!(state["plan"]["schema_version"], 2);
    assert_eq!(state["status"], "completed");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["status"], "failed");
    assert_eq!(history[1]["status"], "succeeded");
    assert_ne!(history[0]["run_id"], history[1]["run_id"]);
    assert_ne!(
        history[0]["supervisor_state_path"],
        history[1]["supervisor_state_path"]
    );
    assert_eq!(
        record(&state, "flaky")["attempt_run_id"],
        history[1]["run_id"]
    );
    assert_eq!(harness.requests().len(), 2);
}

#[test]
fn exhausted_retries_block_descendants_and_continue_unrelated_work() {
    let harness = Harness::new();
    harness.configure_sequences(&[("failed", &["nonzero", "nonzero"])]);
    let plan = harness.plan_v2(
        "retry-exhausted",
        json!([
            task("child", &["failed"]),
            retry_task("failed", &[], 2, 0),
            task("other", &[])
        ]),
    );
    let state_path = harness.state("retry-exhausted");

    run_document(&harness.run_json(&plan, &state_path));

    let state = read_state(&state_path);
    assert_eq!(state["status"], "failed");
    assert_eq!(record(&state, "failed")["status"], "failed");
    assert_eq!(record(&state, "child")["status"], "blocked");
    assert_eq!(record(&state, "other")["status"], "succeeded");
    assert_eq!(attempts(&state, "failed").len(), 2);
    assert_eq!(
        request_names(&harness.requests()),
        ["failed", "failed", "other"]
    );
}

#[test]
fn retry_delay_leaves_slot_free_for_unrelated_ready_work() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"]), ("other", &["barrier"])]);
    let barrier = harness.directory.path().join("retry-delay-barrier");
    let plan = harness.plan_v2(
        "retry-delay",
        json!([retry_task("flaky", &[], 2, 200), task("other", &[])]),
    );
    let state_path = harness.state("retry-delay");
    let child = harness.spawn_json_jobs_with_barrier(&plan, &state_path, 1, &barrier);

    harness.wait_for_requests(2);
    wait_for_retry_pending(&state_path, "flaky");
    assert_eq!(request_names(&harness.requests()), ["flaky", "other"]);
    let running = read_state(&state_path);
    assert_eq!(record(&running, "flaky")["status"], "pending");
    assert_eq!(record(&running, "other")["status"], "running");
    assert_eq!(attempts(&running, "flaky").len(), 1);
    wait_for_path(&barrier.join("started-other"));

    wait_for_path(&barrier);
    fs::write(barrier.join("release"), b"release").unwrap();
    harness.wait_for_requests(3);
    run_document(&child.wait_with_output().unwrap());
    assert_eq!(read_state(&state_path)["status"], "completed");
}

#[test]
fn descendants_wait_for_eventual_retry_success() {
    let harness = Harness::new();
    harness.configure_sequences(&[("root", &["nonzero", "success"])]);
    let plan = harness.plan_v2(
        "retry-descendant",
        json!([task("child", &["root"]), retry_task("root", &[], 2, 0)]),
    );
    let state_path = harness.state("retry-descendant");

    run_document(&harness.run_json(&plan, &state_path));

    let state = read_state(&state_path);
    assert_eq!(state["status"], "completed");
    assert_eq!(
        request_names(&harness.requests()),
        ["root", "root", "child"]
    );
    assert_eq!(record(&state, "child")["status"], "succeeded");
}

#[test]
fn resume_after_scheduled_retry_keeps_failed_attempt_history() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"])]);
    let plan = harness.plan_v2("retry-resume", json!([retry_task("flaky", &[], 2, 200)]));
    let state_path = harness.state("retry-resume");
    let mut child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(1);
    wait_for_retry_pending(&state_path, "flaky");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();

    let waiting = read_state(&state_path);
    assert_eq!(waiting["status"], "running");
    assert_eq!(attempts(&waiting, "flaky").len(), 1);
    assert_eq!(record(&waiting, "flaky")["status"], "pending");

    run_document(&harness.resume_json(&plan, &state_path));
    let resumed = read_state(&state_path);
    assert_eq!(resumed["status"], "completed");
    assert_eq!(attempts(&resumed, "flaky").len(), 2);
    assert_eq!(harness.requests().len(), 2);
}

#[test]
fn cancellation_during_retry_delay_prevents_next_launch() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"])]);
    let plan = harness.plan_v2(
        "retry-delay-cancel",
        json!([retry_task("flaky", &[], 2, 1000)]),
    );
    let state_path = harness.state("retry-delay-cancel");
    let child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(1);
    wait_for_retry_pending(&state_path, "flaky");

    run_document(&terminate(child));
    let cancelled = read_state(&state_path);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(record(&cancelled, "flaky")["status"], "pending");
    assert_eq!(attempts(&cancelled, "flaky").len(), 1);
    assert_eq!(harness.requests().len(), 1);
}

#[test]
fn cancellation_during_retry_attempt_is_not_retried() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "hold"])]);
    let plan = harness.plan_v2(
        "retry-active-cancel",
        json!([retry_task("flaky", &[], 3, 0)]),
    );
    let state_path = harness.state("retry-active-cancel");
    let child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(2);
    let running = wait_for_running_task(&state_path, "flaky");
    wait_for_path(&supervisor_pid_path(&running, "flaky"));

    run_document(&terminate(child));
    let cancelled = read_state(&state_path);
    let history = attempts(&cancelled, "flaky");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["status"], "failed");
    assert_eq!(history[1]["status"], "cancelled");
    assert_eq!(harness.requests().len(), 2);
}

#[test]
fn timeout_is_retryable_only_when_plan_opts_in() {
    let harness = Harness::new();
    harness.configure_sequences(&[("timed", &["timeout", "success"])]);
    let plan = harness.plan_v2("retry-timeout", json!([retry_task("timed", &[], 2, 0)]));
    let state_path = harness.state("retry-timeout");

    run_document(&harness.run_json(&plan, &state_path));

    let state = read_state(&state_path);
    let history = attempts(&state, "timed");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["status"], "failed");
    assert_eq!(
        history[0]["summary"]["termination_reason"],
        "timeout_terminated"
    );
    assert_eq!(history[1]["status"], "succeeded");
}

#[test]
fn protocol_and_remote_uncertainty_never_retries() {
    let harness = Harness::new();
    harness.configure(&[("malformed", "malformed"), ("remote", "remote")]);
    let plan = harness.plan_v2(
        "retry-uncertain",
        json!([
            retry_task("malformed", &[], 3, 0),
            retry_task("remote", &[], 3, 0)
        ]),
    );
    let state_path = harness.state("retry-uncertain");

    let error = error_document(&harness.run_json(&plan, &state_path));
    assert_eq!(error["error"]["code"], "run_supervisor");
    let state = read_state(&state_path);
    assert_eq!(state["status"], "indeterminate");
    for id in ["malformed", "remote"] {
        assert_eq!(record(&state, id)["status"], "indeterminate");
        assert_eq!(attempts(&state, id).len(), 1);
        assert_eq!(attempts(&state, id)[0]["status"], "indeterminate");
    }
    let before_resume = harness.requests().len();
    assert_eq!(
        error_document(&harness.resume_json(&plan, &state_path))["error"]["code"],
        "run_state_invalid"
    );
    assert_eq!(harness.requests().len(), before_resume);
}

#[test]
fn supervisor_transport_uncertainty_never_retries() {
    let harness = Harness::new();
    let plan = harness.plan_v2(
        "retry-transport",
        json!([retry_task("transport", &[], 3, 0)]),
    );
    let state_path = harness.state("retry-transport");
    let output = harness
        .command()
        .env("RELAY_FAKE_TRANSPORT", "1")
        .args([
            "--json",
            "run",
            "--jobs",
            "1",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--state-path",
            state_path.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .expect("run transport uncertainty plan");
    assert_eq!(error_document(&output)["error"]["code"], "run_supervisor");
    let state = read_state(&state_path);
    assert_eq!(record(&state, "transport")["status"], "indeterminate");
    assert_eq!(attempts(&state, "transport").len(), 1);
    assert_eq!(attempts(&state, "transport")[0]["status"], "indeterminate");
    assert!(harness.requests().is_empty());
}

#[test]
fn running_retry_attempt_refuses_resume() {
    let harness = Harness::new();
    harness.configure(&[("running", "hold")]);
    let plan = harness.plan_v2("retry-running", json!([retry_task("running", &[], 2, 0)]));
    let state_path = harness.state("retry-running");
    let mut child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(1);
    let state = read_state(&state_path);
    assert_eq!(attempts(&state, "running")[0]["status"], "running");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();

    let before_resume = harness.requests().len();
    assert_eq!(
        error_document(&harness.resume_json(&plan, &state_path))["error"]["code"],
        "run_state_invalid"
    );
    assert_eq!(harness.requests().len(), before_resume);
    let pid_path = supervisor_pid_path(&state, "running");
    wait_for_path(&pid_path);
    assert_eq!(unsafe { libc::kill(read_pid(&pid_path), libc::SIGTERM) }, 0);
    wait_for_process_exit(&pid_path);
}

#[test]
fn parallel_retries_respect_jobs_cap_and_keep_history_bounded() {
    let harness = Harness::new();
    harness.configure_sequences(&[
        ("left", &["nonzero", "barrier"]),
        ("right", &["nonzero", "barrier"]),
    ]);
    let barrier = harness.directory.path().join("parallel-retry-barrier");
    let plan = harness.plan_v2(
        "parallel-retries",
        json!([
            retry_task("right", &[], 2, 100),
            retry_task("left", &[], 2, 100)
        ]),
    );
    let state_path = harness.state("parallel-retries");
    let child = harness.spawn_json_jobs_with_barrier(&plan, &state_path, 2, &barrier);

    harness.wait_for_requests(4);
    let running = read_state(&state_path);
    assert_eq!(
        records(&running)
            .iter()
            .filter(|task| task["status"] == "running")
            .count(),
        2
    );
    assert!(
        records(&running)
            .iter()
            .all(|task| attempts(&running, task["id"].as_str().unwrap()).len() <= 2)
    );
    wait_for_path(&barrier);
    fs::write(barrier.join("release"), b"release").unwrap();
    run_document(&child.wait_with_output().unwrap());
    let completed = read_state(&state_path);
    assert_eq!(completed["status"], "completed");
    assert_eq!(attempts(&completed, "left").len(), 2);
    assert_eq!(attempts(&completed, "right").len(), 2);
}

#[test]
fn retry_history_is_tightly_bounded_by_maximum_attempts() {
    let harness = Harness::new();
    let outcomes = vec!["nonzero"; 10];
    harness.configure_sequences(&[("bounded", outcomes.as_slice())]);
    let plan = harness.plan_v2("retry-bound", json!([retry_task("bounded", &[], 10, 0)]));
    let state_path = harness.state("retry-bound");

    run_document(&harness.run_json(&plan, &state_path));

    let state = read_state(&state_path);
    assert_eq!(state["status"], "failed");
    assert_eq!(attempts(&state, "bounded").len(), 10);
    assert_eq!(harness.requests().len(), 10);
}

#[test]
fn jobs_one_retry_order_remains_deterministic() {
    let harness = Harness::new();
    harness.configure_sequences(&[("first", &["nonzero", "success"]), ("second", &["success"])]);
    let plan = harness.plan_v2(
        "retry-jobs-one",
        json!([retry_task("first", &[], 2, 0), task("second", &[])]),
    );
    let state_path = harness.state("retry-jobs-one");

    run_document(&harness.run_json_jobs(&plan, &state_path, 1));

    assert_eq!(
        request_names(&harness.requests()),
        ["first", "first", "second"]
    );
    assert_eq!(read_state(&state_path)["status"], "completed");
}

#[test]
fn retry_schema_is_explicit_and_bounds_invalid_policies() {
    let harness = Harness::new();
    let v1_retry = harness.plan_with_schema("retry-v1", 1, json!([retry_task("v1", &[], 2, 0)]));
    let v1_state = harness.state("retry-v1");
    let error = error_document(&harness.run_json(&v1_retry, &v1_state));
    assert_eq!(error["error"]["code"], "run_plan_invalid");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("retry field is not allowed")
    );
    assert!(!v1_state.exists());

    let mut v1_null_task = task("v1-null", &[]);
    v1_null_task["retry"] = Value::Null;
    let v1_null_retry = harness.plan_with_schema("retry-v1-null", 1, json!([v1_null_task]));
    let v1_null_state = harness.state("retry-v1-null");
    let error = error_document(&harness.run_json(&v1_null_retry, &v1_null_state));
    assert_eq!(error["error"]["code"], "run_plan_invalid");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("retry field is not allowed")
    );
    assert!(!v1_null_state.exists());

    let invalid = harness.plan_v2("retry-invalid", json!([retry_task("invalid", &[], 11, 0)]));
    let invalid_state = harness.state("retry-invalid");
    let error = error_document(&harness.run_json(&invalid, &invalid_state));
    assert_eq!(error["error"]["code"], "run_plan_invalid");
    assert!(!invalid_state.exists());

    let mut v2_null_task = task("v2-null", &[]);
    v2_null_task["retry"] = Value::Null;
    let v2_null = harness.plan_v2("retry-v2-null", json!([v2_null_task]));
    let v2_null_state = harness.state("retry-v2-null");
    let error = error_document(&harness.run_json(&v2_null, &v2_null_state));
    assert_eq!(error["error"]["code"], "run_plan_invalid");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("retry must be an object")
    );
    assert!(!v2_null_state.exists());

    let mut v2_wrong_task = task("v2-wrong", &[]);
    v2_wrong_task["retry"] = json!("wrong-type");
    let v2_wrong = harness.plan_v2("retry-v2-wrong", json!([v2_wrong_task]));
    let v2_wrong_state = harness.state("retry-v2-wrong");
    let error = error_document(&harness.run_json(&v2_wrong, &v2_wrong_state));
    assert_eq!(error["error"]["code"], "run_plan_invalid");
    assert!(!v2_wrong_state.exists());
    assert!(harness.requests().is_empty());
}

#[test]
fn real_go_supervisor_retries_known_failure_then_succeeds() {
    let Some(supervisor) = std::env::var_os("RELAY_SUPERVISOR_E2E_BIN") else {
        return;
    };
    let supervisor = PathBuf::from(supervisor);
    assert!(supervisor.is_absolute() && supervisor.is_file());

    let directory = tempfile::tempdir().expect("create real-Go retry directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).expect("create real-Go retry temporary directory");
    let script = directory.path().join("fail-once.py");
    let marker = directory.path().join("failed-once");
    fs::write(
        &script,
        "import pathlib,sys
marker=pathlib.Path(sys.argv[1])
if marker.exists():
    sys.exit(0)
marker.touch()
sys.exit(17)
",
    )
    .expect("write real-Go retry script");
    let plan = directory.path().join("real-go-retry.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "tasks": [{
                "id": "flaky",
                "program": "/usr/bin/python3",
                "arguments": [script.to_str().unwrap(), marker.to_str().unwrap()],
                "timeout_ms": 5_000,
                "retry": {"max_attempts": 2, "delay_ms": 0}
            }]
        }))
        .unwrap(),
    )
    .expect("write real-Go retry plan");
    let state_path = directory.path().join("real-go-retry.state.json");
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .env("TMPDIR", &temporary_root)
        .args([
            "--json",
            "run",
            "--jobs",
            "1",
            "--supervisor",
            supervisor.to_str().unwrap(),
            "--state-path",
            state_path.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .expect("run real-Go retry plan");
    run_document(&output);
    let state = read_state(&state_path);
    let history = attempts(&state, "flaky");
    assert_eq!(state["status"], "completed");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["status"], "failed");
    assert_eq!(history[0]["summary"]["termination_reason"], "exit_nonzero");
    assert_eq!(history[1]["status"], "succeeded");
    assert!(Path::new(history[0]["supervisor_state_path"].as_str().unwrap()).is_file());
    assert!(Path::new(history[1]["supervisor_state_path"].as_str().unwrap()).is_file());
}

#[test]
fn real_go_supervisor_cancellation_during_retry_attempt_is_not_retried() {
    let Some(supervisor) = std::env::var_os("RELAY_SUPERVISOR_E2E_BIN") else {
        return;
    };
    let supervisor = PathBuf::from(supervisor);
    assert!(supervisor.is_absolute() && supervisor.is_file());

    let directory = tempfile::tempdir().expect("create real-Go cancellation directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).expect("create real-Go cancellation temporary directory");
    let plan = directory.path().join("real-go-cancel-retry.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "tasks": [{
                "id": "sleep",
                "program": "/usr/bin/sleep",
                "arguments": ["10"],
                "timeout_ms": 60_000,
                "retry": {"max_attempts": 3, "delay_ms": 0}
            }]
        }))
        .unwrap(),
    )
    .expect("write real-Go cancellation plan");
    let state_path = directory.path().join("real-go-cancel-retry.state.json");
    let child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .env("TMPDIR", &temporary_root)
        .args([
            "--json",
            "run",
            "--jobs",
            "1",
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
        .expect("spawn real-Go cancellation run");
    wait_for_path(&state_path);
    let running = wait_for_running_task(&state_path, "sleep");
    let go_state = PathBuf::from(
        record(&running, "sleep")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    wait_for_path(&go_state);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    run_document(
        &child
            .wait_with_output()
            .expect("wait for real-Go cancellation"),
    );

    let cancelled = read_state(&state_path);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(record(&cancelled, "sleep")["status"], "cancelled");
    assert_eq!(attempts(&cancelled, "sleep").len(), 1);
    assert_eq!(attempts(&cancelled, "sleep")[0]["status"], "cancelled");
    let go_record = read_state(&go_state);
    assert_eq!(go_record["execution"]["lifecycle"], "cancelled");
    assert_eq!(
        go_record["execution"]["termination_reason"],
        "cancelled_terminated"
    );
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
            serde_json::from_slice(&fs::read(&supervisor_state).expect("read Go run record"))
                .expect("valid Go run record");
        assert_eq!(go_record["run_state_version"], 2);
        assert_eq!(go_record["state_path"], supervisor_state.to_str().unwrap());
        assert!(
            go_record["process_identity"]["supervisor"]["pid"]
                .as_i64()
                .unwrap()
                > 0
        );
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

#[test]
fn real_go_supervisor_runs_independent_tasks_in_parallel() {
    let Some(supervisor) = std::env::var_os("RELAY_SUPERVISOR_E2E_BIN") else {
        return;
    };
    let supervisor = PathBuf::from(supervisor);
    assert!(supervisor.is_absolute() && supervisor.is_file());

    let directory = tempfile::tempdir().expect("create real-Go parallel directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).expect("create real-Go temporary directory");
    let first_started = directory.path().join("first-started");
    let second_started = directory.path().join("second-started");
    let release = directory.path().join("release");
    let state_path = directory.path().join("parallel.state.json");
    let plan = directory.path().join("parallel.json");
    let program = "/usr/bin/python3";
    let script = r#"import pathlib,sys,time
pathlib.Path(sys.argv[1]).touch()
while not pathlib.Path(sys.argv[2]).exists():
    time.sleep(0.01)
"#;
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "tasks": [
                {
                    "id": "first",
                    "depends_on": [],
                    "program": program,
                    "arguments": ["-c", script, first_started.to_str().unwrap(), release.to_str().unwrap()],
                    "timeout_ms": 5_000
                },
                {
                    "id": "second",
                    "depends_on": [],
                    "program": program,
                    "arguments": ["-c", script, second_started.to_str().unwrap(), release.to_str().unwrap()],
                    "timeout_ms": 5_000
                }
            ]
        }))
        .unwrap(),
    )
    .expect("write real-Go parallel plan");

    let child = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .env("TMPDIR", &temporary_root)
        .args([
            "--json",
            "run",
            "--jobs",
            "2",
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
        .expect("spawn real-Go parallel Relay run");
    wait_for_path(&first_started);
    wait_for_path(&second_started);
    fs::write(&release, b"release").expect("release real-Go tasks");
    let output = child
        .wait_with_output()
        .expect("wait for real-Go parallel run");
    run_document(&output);
    let state = read_state(&state_path);
    assert_eq!(state["jobs"], 2);
    assert!(
        records(&state)
            .iter()
            .all(|record| record["status"] == "succeeded")
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
    let explanation = Command::new(env!("CARGO_BIN_EXE_relay"))
        .current_dir(directory.path())
        .args(["--json", "run", "explain", state_path.to_str().unwrap()])
        .output()
        .expect("explain cancelled persisted run");
    let explanation = run_explanation_document(&explanation);
    assert_eq!(explanation["data"]["run"]["status"], "cancelled");
    assert_eq!(explanation["data"]["run"]["cancellation_requested"], true);
    assert_eq!(explanation["data"]["tasks"][0]["status"], "cancelled");
    assert_eq!(explanation["data"]["tasks"][1]["status"], "blocked");
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
fn jobs_one_keeps_sequential_order_and_state() {
    let harness = Harness::new();
    let plan = harness.plan("jobs-one", json!([task("second", &[]), task("first", &[])]));
    let state_path = harness.state("jobs-one");

    run_document(&harness.run_json_jobs(&plan, &state_path, 1));

    assert_eq!(request_names(&harness.requests()), ["second", "first"]);
    let state = read_state(&state_path);
    assert_eq!(state["jobs"], 1);
    assert!(
        records(&state)
            .iter()
            .all(|record| record["status"] == "succeeded")
    );
}

#[test]
fn jobs_two_overlaps_independent_tasks_and_persists_running_attempts() {
    let harness = Harness::new();
    harness.configure(&[("first", "barrier"), ("second", "barrier")]);
    let barrier = harness.directory.path().join("overlap-barrier");
    let plan = harness.plan("overlap", json!([task("first", &[]), task("second", &[])]));
    let state_path = harness.state("overlap");
    let child = harness.spawn_json_jobs_with_barrier(&plan, &state_path, 2, &barrier);

    harness.wait_for_requests(2);
    wait_for_path(&barrier.join("started-first"));
    wait_for_path(&barrier.join("started-second"));
    let running = read_state(&state_path);
    assert_eq!(running["jobs"], 2);
    assert_eq!(
        records(&running)
            .iter()
            .filter(|record| record["status"] == "running")
            .count(),
        2
    );
    let first_attempt = record(&running, "first")["attempt_run_id"]
        .as_str()
        .expect("first running attempt");
    let second_attempt = record(&running, "second")["attempt_run_id"]
        .as_str()
        .expect("second running attempt");
    assert_ne!(first_attempt, second_attempt);
    assert!(record(&running, "first")["supervisor_state_path"].is_string());
    assert!(record(&running, "second")["supervisor_state_path"].is_string());

    wait_for_path(&barrier);
    fs::write(barrier.join("release"), b"release").unwrap();
    run_document(&child.wait_with_output().unwrap());
    let completed = read_state(&state_path);
    assert_eq!(completed["status"], "completed");
}

#[test]
fn jobs_two_never_exceeds_limit_and_launches_ready_tasks_in_input_order() {
    let harness = Harness::new();
    harness.configure(&[
        ("d", "barrier"),
        ("b", "barrier"),
        ("c", "barrier"),
        ("a", "barrier"),
    ]);
    let barrier = harness.directory.path().join("limit-barrier");
    let plan = harness.plan(
        "limit-order",
        json!([
            task("d", &[]),
            task("b", &[]),
            task("c", &[]),
            task("a", &[])
        ]),
    );
    let state_path = harness.state("limit-order");
    let child = harness.spawn_json_jobs_with_barrier(&plan, &state_path, 2, &barrier);

    harness.wait_for_requests(2);
    let initial_log = harness.requests();
    let mut initial_names = request_names(&initial_log);
    initial_names.sort_unstable();
    assert_eq!(initial_names, ["b", "d"]);
    assert!(!barrier.join("started-c").exists());
    assert!(!barrier.join("started-a").exists());

    wait_for_path(&barrier);
    fs::write(barrier.join("release"), b"release").unwrap();
    harness.wait_for_requests(4);
    run_document(&child.wait_with_output().unwrap());
    let all_log = harness.requests();
    let mut all_names = request_names(&all_log);
    let mut initial_names = all_names[..2].to_vec();
    initial_names.sort_unstable();
    let mut later_names = all_names.split_off(2);
    later_names.sort_unstable();
    assert_eq!(initial_names, ["b", "d"]);
    assert_eq!(later_names, ["a", "c"]);
}

#[test]
fn parallel_fan_out_and_fan_in_wait_for_all_dependencies() {
    let harness = Harness::new();
    harness.configure(&[("right", "barrier"), ("left", "barrier")]);
    let barrier = harness.directory.path().join("fan-barrier");
    let plan = harness.plan(
        "fan",
        json!([
            task("join", &["right", "left"]),
            task("right", &["root"]),
            task("left", &["root"]),
            task("root", &[])
        ]),
    );
    let state_path = harness.state("fan");
    let child = harness.spawn_json_jobs_with_barrier(&plan, &state_path, 2, &barrier);

    harness.wait_for_requests(3);
    let initial_log = harness.requests();
    let initial_names = request_names(&initial_log);
    assert_eq!(initial_names[0], "root");
    let mut branch_names = request_names(&initial_log[1..]);
    branch_names.sort_unstable();
    assert_eq!(branch_names, ["left", "right"]);
    let running = read_state(&state_path);
    assert_eq!(record(&running, "right")["status"], "running");
    assert_eq!(record(&running, "left")["status"], "running");
    assert_eq!(record(&running, "join")["status"], "pending");

    wait_for_path(&barrier);
    fs::write(barrier.join("release"), b"release").unwrap();
    harness.wait_for_requests(4);
    let final_log = harness.requests();
    let final_names = request_names(&final_log);
    assert_eq!(final_names[0], "root");
    assert_eq!(final_names[3], "join");
    run_document(&child.wait_with_output().unwrap());
    assert_eq!(read_state(&state_path)["status"], "completed");
}

#[test]
fn parallel_failure_and_timeout_block_descendants_but_preserve_unrelated_work() {
    let harness = Harness::new();
    harness.configure(&[
        ("failed", "nonzero"),
        ("timeout", "timeout"),
        ("other", "barrier"),
    ]);
    let barrier = harness.directory.path().join("failure-barrier");
    let plan = harness.plan(
        "parallel-failures",
        json!([
            task("failed-child", &["failed"]),
            task("failed", &[]),
            task("timeout-child", &["timeout"]),
            task("timeout", &[]),
            task("other", &[])
        ]),
    );
    let state_path = harness.state("parallel-failures");
    let child = harness.spawn_json_jobs_with_barrier(&plan, &state_path, 2, &barrier);

    harness.wait_for_requests(3);
    let request_log = harness.requests();
    let mut names = request_names(&request_log);
    names.sort_unstable();
    assert_eq!(names, ["failed", "other", "timeout"]);
    wait_for_path(&barrier);
    wait_for_path(&barrier);
    fs::write(barrier.join("release"), b"release").unwrap();
    run_document(&child.wait_with_output().unwrap());

    let state = read_state(&state_path);
    assert_eq!(state["status"], "failed");
    assert_eq!(record(&state, "failed")["status"], "failed");
    assert_eq!(record(&state, "timeout")["status"], "failed");
    assert_eq!(record(&state, "failed-child")["status"], "blocked");
    assert_eq!(record(&state, "timeout-child")["status"], "blocked");
    assert_eq!(record(&state, "other")["status"], "succeeded");
}

#[test]
fn parallel_cancellation_reaches_all_active_tasks_and_resume_runs_pending_work() {
    let harness = Harness::new();
    harness.configure(&[("first", "barrier"), ("second", "barrier")]);
    let barrier = harness.directory.path().join("cancel-barrier");
    let plan = harness.plan(
        "parallel-cancel",
        json!([task("first", &[]), task("second", &[]), task("later", &[])]),
    );
    let state_path = harness.state("parallel-cancel");
    let child = harness.spawn_json_jobs_with_barrier(&plan, &state_path, 2, &barrier);
    harness.wait_for_requests(2);
    wait_for_path(&barrier.join("started-first"));
    wait_for_path(&barrier.join("started-second"));

    let output = terminate(child);
    run_document(&output);
    let cancelled = read_state(&state_path);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(record(&cancelled, "first")["status"], "cancelled");
    assert_eq!(record(&cancelled, "second")["status"], "cancelled");
    assert_eq!(record(&cancelled, "later")["status"], "pending");
    assert_eq!(harness.requests().len(), 2);

    harness.configure(&[]);
    run_document(&harness.resume_json_jobs(&plan, &state_path, 2));
    let resumed = read_state(&state_path);
    assert_eq!(resumed["status"], "cancelled");
    assert_eq!(record(&resumed, "later")["status"], "succeeded");
    assert_eq!(harness.requests().len(), 3);
}

#[cfg(debug_assertions)]
#[test]
fn signal_during_parallel_startup_cancels_all_without_new_launches() {
    let harness = Harness::new();
    harness.configure(&[("first", "hold"), ("second", "hold")]);
    let marker = harness.directory.path().join("parallel-start.marker");
    let release = harness.directory.path().join("parallel-start.release");
    let plan = harness.plan(
        "parallel-startup",
        json!([task("first", &[]), task("second", &[]), task("later", &[])]),
    );
    let state_path = harness.state("parallel-startup");
    let jobs = "2";
    let child = harness
        .command()
        .env(
            "RELAY_TEST_SUPERVISOR_PAUSE",
            "after_spawn_before_activation",
        )
        .env("RELAY_TEST_SUPERVISOR_MARKER", &marker)
        .env("RELAY_TEST_SUPERVISOR_RELEASE", &release)
        .args([
            "--json",
            "run",
            "--jobs",
            jobs,
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
        .unwrap();
    wait_for_file_size(&marker, 2);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    fs::write(&release, b"release").unwrap();
    run_document(&child.wait_with_output().unwrap());

    let state = read_state(&state_path);
    assert_eq!(state["status"], "cancelled");
    assert_eq!(record(&state, "first")["status"], "cancelled");
    assert_eq!(record(&state, "second")["status"], "cancelled");
    assert_eq!(record(&state, "later")["status"], "pending");
    assert_eq!(harness.requests().len(), 2);
}

#[cfg(debug_assertions)]
#[test]
fn cancellation_before_supervisor_spawn_does_not_launch_new_work() {
    const ITERATIONS: usize = 100;
    let harness = Harness::new();
    harness.configure(&[("only", "hold")]);

    for iteration in 0..ITERATIONS {
        let marker = harness
            .directory
            .path()
            .join(format!("before-spawn-{iteration}.marker"));
        let release = harness
            .directory
            .path()
            .join(format!("before-spawn-{iteration}.release"));
        let name = format!("before-spawn-{iteration}");
        let plan = harness.plan(&name, json!([task("only", &[])]));
        let state_path = harness.state(&name);
        let child = harness
            .command()
            .env(
                "RELAY_TEST_SUPERVISOR_PAUSE",
                "after_spawn_cancellation_check",
            )
            .env("RELAY_TEST_SUPERVISOR_MARKER", &marker)
            .env("RELAY_TEST_SUPERVISOR_RELEASE", &release)
            .args([
                "--json",
                "run",
                "--jobs",
                "1",
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
            .unwrap();
        wait_for_path(&marker);
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        fs::write(&release, b"release").unwrap();
        run_document(&child.wait_with_output().unwrap());

        let state = read_state(&state_path);
        assert_eq!(state["status"], "cancelled");
        assert_eq!(record(&state, "only")["status"], "pending");
        assert!(record(&state, "only")["attempt_run_id"].is_null());
        assert!(record(&state, "only")["supervisor_state_path"].is_null());
    }
    assert!(harness.requests().is_empty());
}

#[cfg(debug_assertions)]
#[test]
fn persistence_failure_during_cancellation_drains_two_active_workers() {
    let harness = Harness::new();
    harness.configure(&[("first", "barrier"), ("second", "barrier")]);
    let barrier = harness.directory.path().join("persist-cancel-barrier");
    let plan = harness.plan(
        "persist-cancel",
        json!([task("first", &[]), task("second", &[]), task("later", &[]),]),
    );
    let state_path = harness.state("persist-cancel");
    let child = harness
        .command()
        .env("RELAY_FAKE_BARRIER_DIR", &barrier)
        .env("RELAY_TEST_RUN_FAIL_PERSIST_AT", "cancellation")
        .args([
            "--json",
            "run",
            "--jobs",
            "2",
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
        .unwrap();
    harness.wait_for_requests(2);
    wait_for_path(&barrier.join("started-first"));
    wait_for_path(&barrier.join("started-second"));
    let running = read_state(&state_path);
    let first_pid = supervisor_pid_path(&running, "first");
    let second_pid = supervisor_pid_path(&running, "second");

    let error = error_document(&terminate(child));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    let state = read_state(&state_path);
    assert_eq!(state["schema_version"], 5);
    assert_eq!(state["jobs"], 2);
    assert_eq!(state["status"], "cancelled");
    assert_eq!(state["cancellation_requested"], true);
    assert_eq!(record(&state, "first")["status"], "cancelled");
    assert_eq!(record(&state, "second")["status"], "cancelled");
    assert_eq!(record(&state, "later")["status"], "pending");
    assert_eq!(harness.requests().len(), 2);
    wait_for_process_exit(&first_pid);
    wait_for_process_exit(&second_pid);
}

#[test]
fn persistence_failure_after_one_completion_drains_remaining_worker() {
    let harness = Harness::new();
    harness.configure(&[("first", "success"), ("second", "hold")]);
    let marker = harness.directory.path().join("persist-completion.marker");
    let release = harness.directory.path().join("persist-completion.release");
    let plan = harness.plan(
        "persist-completion",
        json!([task("first", &[]), task("second", &[]), task("later", &[]),]),
    );
    let state_path = harness.state("persist-completion");
    let child = harness
        .command()
        .env("RELAY_TEST_RUN_FAIL_PERSIST_AT", "completion")
        .env("RELAY_TEST_RUN_PAUSE", "after_supervisor_result")
        .env("RELAY_TEST_RUN_MARKER", &marker)
        .env("RELAY_TEST_RUN_RELEASE", &release)
        .args([
            "--json",
            "run",
            "--jobs",
            "2",
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
        .unwrap();
    harness.wait_for_requests(2);
    wait_for_path(&marker);
    let running = read_state(&state_path);
    let second_pid = supervisor_pid_path(&running, "second");
    fs::write(&release, b"release").unwrap();

    let error = error_document(&child.wait_with_output().unwrap());
    assert_eq!(error["error"]["code"], "run_state_invalid");
    let state = read_state(&state_path);
    assert_eq!(state["schema_version"], 5);
    assert_eq!(state["jobs"], 2);
    assert_eq!(record(&state, "first")["status"], "succeeded");
    assert_eq!(record(&state, "second")["status"], "cancelled");
    assert_eq!(record(&state, "later")["status"], "pending");
    assert_eq!(harness.requests().len(), 2);
    wait_for_process_exit(&second_pid);
}

#[test]
fn parallel_protocol_failure_is_indeterminate_without_stopping_unrelated_tasks() {
    let harness = Harness::new();
    harness.configure(&[("broken", "malformed"), ("other", "barrier")]);
    let barrier = harness.directory.path().join("protocol-barrier");
    let plan = harness.plan(
        "parallel-protocol",
        json!([task("broken", &[]), task("other", &[]), task("later", &[])]),
    );
    let state_path = harness.state("parallel-protocol");
    let child = harness.spawn_json_jobs_with_barrier(&plan, &state_path, 2, &barrier);
    harness.wait_for_requests(3);
    let request_log = harness.requests();
    let mut names = request_names(&request_log);
    names.sort_unstable();
    assert_eq!(names, ["broken", "later", "other"]);
    wait_for_path(&barrier);
    fs::write(barrier.join("release"), b"release").unwrap();

    let error = error_document(&child.wait_with_output().unwrap());
    assert_eq!(error["error"]["code"], "run_supervisor");
    let state = read_state(&state_path);
    assert_eq!(state["status"], "indeterminate");
    assert_eq!(record(&state, "broken")["status"], "indeterminate");
    assert_eq!(record(&state, "other")["status"], "succeeded");
    assert_eq!(record(&state, "later")["status"], "succeeded");
}

#[test]
fn resume_refuses_multiple_uncertain_active_attempts() {
    let harness = Harness::new();
    harness.configure(&[("first", "hold"), ("second", "hold")]);
    let plan = harness.plan(
        "uncertain-parallel",
        json!([task("first", &[]), task("second", &[]), task("later", &[])]),
    );
    let state_path = harness.state("uncertain-parallel");
    let mut child = harness.spawn_json_jobs(&plan, &state_path, 2);
    harness.wait_for_requests(2);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();

    let state = read_state(&state_path);
    assert_eq!(record(&state, "first")["status"], "running");
    assert_eq!(record(&state, "second")["status"], "running");
    let before = harness.requests().len();
    let error = error_document(&harness.resume_json_jobs(&plan, &state_path, 2));
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before);

    for id in ["first", "second"] {
        let attempt_state = PathBuf::from(
            record(&state, id)["supervisor_state_path"]
                .as_str()
                .unwrap(),
        );
        let pid_path = PathBuf::from(format!("{}.pid", attempt_state.display()));
        wait_for_path(&pid_path);
        let pid: i32 = fs::read_to_string(pid_path).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    }
}

#[test]
fn migrates_clean_v1_state_and_resumes_pending_work() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "cancelled")]);
    let plan = harness.plan(
        "legacy-pending",
        json!([task("waiting", &[]), task("later", &[])]),
    );
    let state_path = harness.state("legacy-pending");
    run_document(&harness.run_json(&plan, &state_path));
    downgrade_to_v1(&state_path);
    let legacy = read_state(&state_path);
    assert_eq!(legacy["schema_version"], 1);
    assert!(legacy["jobs"].is_null());

    harness.configure(&[]);
    run_document(&harness.resume_json(&plan, &state_path));

    let migrated = read_state(&state_path);
    assert_eq!(migrated["schema_version"], 5);
    assert_eq!(migrated["jobs"], 1);
    assert_eq!(record(&migrated, "waiting")["status"], "cancelled");
    assert_eq!(record(&migrated, "later")["status"], "succeeded");
    assert_eq!(request_names(&harness.requests()), ["waiting", "later"]);
}

#[test]
fn migrates_v3_retry_history_without_dropping_attempts() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky-v3", &["nonzero", "success"])]);
    let plan = harness.plan_v2(
        "v3-retry-history",
        json!([retry_task("flaky-v3", &[], 2, 0)]),
    );
    let state_path = harness.state("v3-retry-history");
    run_document(&harness.run_json(&plan, &state_path));
    assert_eq!(attempts(&read_state(&state_path), "flaky-v3").len(), 2);
    downgrade_to_v3(&state_path);
    assert_eq!(read_state(&state_path)["schema_version"], 3);
    let before_resume = harness.requests().len();
    harness.configure(&[]);
    run_document(&harness.resume_json(&plan, &state_path));
    let migrated = read_state(&state_path);
    assert_eq!(migrated["schema_version"], 5);
    assert_eq!(attempts(&migrated, "flaky-v3").len(), 2);
    assert_eq!(harness.requests().len(), before_resume);
}
#[test]
fn migrates_clean_v2_state_and_resumes_pending_work() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "cancelled")]);
    let plan = harness.plan(
        "legacy-v2-pending",
        json!([task("waiting", &[]), task("later", &[])]),
    );
    let state_path = harness.state("legacy-v2-pending");
    run_document(&harness.run_json(&plan, &state_path));
    downgrade_to_v2(&state_path);
    let legacy = read_state(&state_path);
    assert_eq!(legacy["schema_version"], 2);
    assert!(legacy["tasks"][0]["attempts"].is_null());

    harness.configure(&[]);
    run_document(&harness.resume_json(&plan, &state_path));
    let migrated = read_state(&state_path);
    assert_eq!(migrated["schema_version"], 5);
    assert_eq!(attempts(&migrated, "waiting").len(), 1);
    assert_eq!(record(&migrated, "later")["status"], "succeeded");
}

#[test]
fn migrates_v4_direct_state_without_workflow_metadata() {
    let harness = Harness::new();
    let plan = harness.plan("direct-v4", json!([task("only", &[])]));
    let state_path = harness.state("direct-v4");
    run_document(&harness.run_json(&plan, &state_path));
    let mut state = read_state(&state_path);
    assert_eq!(state["schema_version"], 5);
    assert!(state["workflow"].is_null());
    state["schema_version"] = json!(4);
    fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    let before_resume = harness.requests().len();

    run_document(&harness.resume_json(&plan, &state_path));

    let migrated = read_state(&state_path);
    assert_eq!(migrated["schema_version"], 5);
    assert!(migrated["workflow"].is_null());
    assert_eq!(harness.requests().len(), before_resume);
}

#[test]
fn unsafe_v1_running_or_indeterminate_state_stays_non_resumable() {
    let harness = Harness::new();
    harness.configure(&[("running", "hold")]);
    let running_plan = harness.plan("legacy-running", json!([task("running", &[])]));
    let running_path = harness.state("legacy-running");
    let mut child = harness.spawn_json(&running_plan, &running_path);
    harness.wait_for_requests(1);
    let running = read_state(&running_path);
    let running_pid = supervisor_pid_path(&running, "running");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();
    downgrade_to_v1(&running_path);
    let before_running_resume = harness.requests().len();
    let running_error = error_document(&harness.resume_json(&running_plan, &running_path));
    assert_eq!(running_error["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before_running_resume);
    assert_eq!(
        unsafe { libc::kill(read_pid(&running_pid), libc::SIGTERM) },
        0
    );
    wait_for_process_exit(&running_pid);

    harness.configure(&[("uncertain", "malformed")]);
    let uncertain_plan = harness.plan("legacy-indeterminate", json!([task("uncertain", &[])]));
    let uncertain_path = harness.state("legacy-indeterminate");
    error_document(&harness.run_json(&uncertain_plan, &uncertain_path));
    assert_eq!(read_state(&uncertain_path)["status"], "indeterminate");
    downgrade_to_v1(&uncertain_path);
    let before_uncertain_resume = harness.requests().len();
    let uncertain_error = error_document(&harness.resume_json(&uncertain_plan, &uncertain_path));
    assert_eq!(uncertain_error["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before_uncertain_resume);
}

#[test]
fn rejects_unknown_and_malformed_aggregate_state_versions() {
    let harness = Harness::new();
    let plan = harness.plan("state-schema-errors", json!([task("only", &[])]));
    let state_path = harness.state("state-schema-errors");
    run_document(&harness.run_json(&plan, &state_path));
    let original = read_state(&state_path);
    let before = harness.requests().len();

    let mut unknown_version = original.clone();
    unknown_version["schema_version"] = json!(99);
    let mut missing_jobs = original.clone();
    missing_jobs
        .as_object_mut()
        .expect("aggregate state object")
        .remove("jobs");
    let mut unknown_field = original.clone();
    unknown_field["unexpected"] = json!(true);
    let mut legacy_null = original.clone();
    legacy_null["schema_version"] = json!(1);
    legacy_null["jobs"] = Value::Null;
    let mut legacy_zero = legacy_null.clone();
    legacy_zero["jobs"] = json!(0);
    let mut legacy_jobs = original.clone();
    legacy_jobs["schema_version"] = json!(1);
    legacy_jobs["jobs"] = json!(1);
    let mut v2_zero = original.clone();
    v2_zero["jobs"] = json!(0);
    for invalid in [
        unknown_version,
        missing_jobs,
        unknown_field,
        legacy_null,
        legacy_zero,
        legacy_jobs,
        v2_zero,
    ] {
        fs::write(&state_path, serde_json::to_vec_pretty(&invalid).unwrap()).unwrap();
        let error = error_document(&harness.resume_json(&plan, &state_path));
        assert_eq!(error["error"]["code"], "run_state_invalid");
        assert_eq!(harness.requests().len(), before);
    }
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
    assert_eq!(state["schema_version"], 5);
    assert_eq!(state["jobs"], 1);
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
    assert_eq!(record(&state, "pending")["status"], "succeeded");
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

        assert_eq!(request_names(&harness.requests()), [name, "later"]);
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
        assert_eq!(record(&state, "later")["status"], "succeeded");
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
#[test]
fn corrupted_retry_history_refuses_resume_without_launching() {
    for (label, status) in [
        ("succeeded", "succeeded"),
        ("cancelled", "cancelled"),
        ("indeterminate", "indeterminate"),
        ("running", "running"),
    ] {
        let harness = Harness::new();
        harness.configure_sequences(&[("flaky", &["nonzero", "nonzero", "success"])]);
        let plan = harness.plan_v2(
            &format!("corrupt-{label}"),
            json!([retry_task("flaky", &[], 3, 0)]),
        );
        let state_path = harness.state(&format!("corrupt-{label}"));
        run_document(&harness.run_json(&plan, &state_path));

        let mut state = read_state(&state_path);
        state["tasks"][0]["attempts"][0]["status"] = json!(status);
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let before_resume = harness.requests().len();

        assert_eq!(
            error_document(&harness.resume_json(&plan, &state_path))["error"]["code"],
            "run_state_invalid"
        );
        assert_eq!(harness.requests().len(), before_resume);
    }
}

#[test]
fn non_retryable_failed_history_refuses_resume_without_launching() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"])]);
    let plan = harness.plan_v2(
        "corrupt-non-retryable-history",
        json!([retry_task("flaky", &[], 2, 0)]),
    );
    let state_path = harness.state("corrupt-non-retryable-history");
    run_document(&harness.run_json(&plan, &state_path));

    let mut state = read_state(&state_path);
    state["tasks"][0]["attempts"][0]["summary"]["cancellation_requested"] = json!(true);
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let before_resume = harness.requests().len();

    assert_eq!(
        error_document(&harness.resume_json(&plan, &state_path))["error"]["code"],
        "run_state_invalid"
    );
    assert_eq!(harness.requests().len(), before_resume);

    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"])]);
    let plan = harness.plan_v2(
        "corrupt-pending-non-retryable",
        json!([retry_task("flaky", &[], 2, 60_000)]),
    );
    let state_path = harness.state("corrupt-pending-non-retryable");
    let mut child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(1);
    wait_for_retry_pending(&state_path, "flaky");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();

    let mut state = read_state(&state_path);
    state["tasks"][0]["attempts"][0]["summary"]["cancellation_requested"] = json!(true);
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let before_resume = harness.requests().len();

    assert_eq!(
        error_document(&harness.resume_json(&plan, &state_path))["error"]["code"],
        "run_state_invalid"
    );
    assert_eq!(harness.requests().len(), before_resume);
}

#[test]
fn retryable_failure_without_deadline_refuses_resume() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"])]);
    let plan = harness.plan_v2(
        "corrupt-missing-retry-deadline",
        json!([retry_task("flaky", &[], 2, 60_000)]),
    );
    let state_path = harness.state("corrupt-missing-retry-deadline");
    let mut child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(1);
    wait_for_retry_pending(&state_path, "flaky");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();

    let mut state = read_state(&state_path);
    let latest = state["tasks"][0]["attempts"][0].clone();
    let task = &mut state["tasks"][0];
    task["status"] = json!("failed");
    task["attempt_run_id"] = latest["run_id"].clone();
    task["supervisor_state_path"] = latest["supervisor_state_path"].clone();
    task["reason"] = latest["reason"].clone();
    task["summary"] = latest["summary"].clone();
    task["retry_ready_at"] = Value::Null;
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let before_resume = harness.requests().len();

    assert_eq!(
        error_document(&harness.resume_json(&plan, &state_path))["error"]["code"],
        "run_state_invalid"
    );
    assert_eq!(harness.requests().len(), before_resume);
}

#[test]
fn valid_retry_history_transitions_resume_without_new_launches() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "nonzero", "success"])]);
    let plan = harness.plan_v2(
        "valid-retry-history",
        json!([retry_task("flaky", &[], 3, 0)]),
    );
    let state_path = harness.state("valid-retry-history");
    run_document(&harness.run_json(&plan, &state_path));
    let state = read_state(&state_path);
    assert_eq!(
        attempts(&state, "flaky")
            .iter()
            .map(|attempt| attempt["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["failed", "failed", "succeeded"]
    );
    let before_resume = harness.requests().len();
    run_document(&harness.resume_json(&plan, &state_path));
    assert_eq!(harness.requests().len(), before_resume);

    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"])]);
    let plan = harness.plan_v2(
        "valid-pending-retry",
        json!([retry_task("flaky", &[], 2, 60_000)]),
    );
    let state_path = harness.state("valid-pending-retry");
    let mut child = harness.spawn_json(&plan, &state_path);
    harness.wait_for_requests(1);
    wait_for_retry_pending(&state_path, "flaky");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();
    let state = read_state(&state_path);
    assert_eq!(record(&state, "flaky")["status"], "pending");
    assert_eq!(attempts(&state, "flaky")[0]["status"], "failed");

    run_document(&harness.resume_json(&plan, &state_path));
    let resumed = read_state(&state_path);
    assert_eq!(resumed["status"], "completed");
    assert_eq!(attempts(&resumed, "flaky").len(), 2);
}

fn real_supervisor_path() -> Option<PathBuf> {
    let supervisor = std::env::var_os("RELAY_SUPERVISOR_E2E_BIN")?;
    let supervisor = PathBuf::from(supervisor);
    assert!(supervisor.is_absolute() && supervisor.is_file());
    Some(supervisor)
}

fn real_relay_command(supervisor: &Path, directory: &Path, temporary_root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_relay"));
    command
        .current_dir(directory)
        .env("TMPDIR", temporary_root)
        .args(["--json", "run", "--supervisor"])
        .arg(supervisor);
    command
}

fn spawn_real_crash_run(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    plan: &Path,
    state: &Path,
    marker: &Path,
    jobs: usize,
) -> Child {
    let jobs = jobs.to_string();
    let release = marker.with_extension("release");
    real_relay_command(supervisor, directory, temporary_root)
        .env("RELAY_TEST_RUN_PAUSE", "after_supervisor_result")
        .env("RELAY_TEST_RUN_MARKER", marker)
        .env("RELAY_TEST_RUN_RELEASE", release)
        .args([
            "--jobs",
            jobs.as_str(),
            "--state-path",
            state.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real-Go crash run")
}

fn resume_real_run(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    plan: &Path,
    state: &Path,
    jobs: usize,
) -> Output {
    let jobs = jobs.to_string();
    real_relay_command(supervisor, directory, temporary_root)
        .args([
            "--jobs",
            jobs.as_str(),
            "--resume",
            state.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .output()
        .expect("resume real-Go run")
}

fn wait_for_go_running_state(path: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if path.is_file() {
            let record = read_state(path);
            if record["execution"]["lifecycle"] == "running"
                && record["process_identity"]["child"]["pid"].is_number()
            {
                return record;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Go running state"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn kill_real_attempt(state: &Value, id: &str) {
    let path = PathBuf::from(
        record(state, id)["supervisor_state_path"]
            .as_str()
            .expect("real supervisor state path"),
    );
    let Ok(payload) = fs::read(&path) else {
        return;
    };
    let Ok(go_record) = serde_json::from_slice::<Value>(&payload) else {
        return;
    };
    if let Some(pid) = go_record["process_identity"]["child"]["pid"].as_i64()
        && pid > 0
    {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    if let Some(pid) = go_record["process_identity"]["supervisor"]["pid"].as_i64()
        && pid > 0
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

fn kill_coordinator(mut child: Child) {
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().expect("wait for killed coordinator");
}

#[test]
fn real_go_reconciles_completed_attempt_without_rerun() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create completed recovery directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let script = directory.path().join("count.py");
    let counter = directory.path().join("count");
    let descendant = directory.path().join("descendant-ran");
    fs::write(
        &script,
        "import pathlib,sys\np=pathlib.Path(sys.argv[1])\nn=int(p.read_text()) if p.exists() else 0\np.write_text(str(n+1))\n",
    )
    .unwrap();
    let plan = directory.path().join("plan.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "tasks": [
                {
                    "id": "only",
                    "program": "/usr/bin/python3",
                    "arguments": [script.to_str().unwrap(), counter.to_str().unwrap()],
                    "timeout_ms": 5_000
                },
                {
                    "id": "later",
                    "depends_on": ["only"],
                    "program": "/usr/bin/touch",
                    "arguments": [descendant.to_str().unwrap()],
                    "timeout_ms": 5_000
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let state = directory.path().join("aggregate.json");
    let marker = directory.path().join("pause.marker");
    let child = spawn_real_crash_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        &marker,
        1,
    );
    wait_for_path(&marker);
    let interrupted = read_state(&state);
    assert_eq!(record(&interrupted, "only")["status"], "running");
    let go_state = PathBuf::from(
        record(&interrupted, "only")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    let mut go_record = read_state(&go_state);
    assert_eq!(go_record["run_state_version"], 2);
    assert_eq!(go_record["execution"]["lifecycle"], "completed");
    go_record["run_state_version"] = json!(1);
    go_record.as_object_mut().unwrap().remove("state_path");
    go_record
        .as_object_mut()
        .unwrap()
        .remove("process_identity");
    fs::write(&go_state, serde_json::to_vec(&go_record).unwrap()).unwrap();
    kill_coordinator(child);

    let resumed = resume_real_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        1,
    );
    let document = run_document(&resumed);
    let final_state = read_state(&state);
    assert_eq!(final_state["status"], "completed");
    assert_eq!(record(&final_state, "only")["status"], "succeeded");
    assert_eq!(attempts(&final_state, "only").len(), 1);
    assert_eq!(record(&final_state, "later")["status"], "succeeded");
    assert_eq!(attempts(&final_state, "later").len(), 1);
    assert!(descendant.is_file());
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    assert_eq!(
        document["data"]["reconciliation"][0]["classification"],
        "completed"
    );
}

#[test]
fn real_go_reconciles_known_failure_and_retries_once() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create failure recovery directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let script = directory.path().join("fail-once.py");
    let counter = directory.path().join("count");
    fs::write(
        &script,
        "import pathlib,sys\np=pathlib.Path(sys.argv[1])\nn=int(p.read_text()) if p.exists() else 0\np.write_text(str(n+1))\nsys.exit(17 if n == 0 else 0)\n",
    )
    .unwrap();
    let plan = directory.path().join("plan.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "tasks": [{
                "id": "flaky",
                "program": "/usr/bin/python3",
                "arguments": [script.to_str().unwrap(), counter.to_str().unwrap()],
                "timeout_ms": 5_000,
                "retry": {"max_attempts": 2, "delay_ms": 0}
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let state = directory.path().join("aggregate.json");
    let marker = directory.path().join("pause.marker");
    let child = spawn_real_crash_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        &marker,
        1,
    );
    wait_for_path(&marker);
    let interrupted = read_state(&state);
    let first_id = attempts(&interrupted, "flaky")[0]["run_id"].clone();
    let go_state = PathBuf::from(
        record(&interrupted, "flaky")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(
        read_state(&go_state)["execution"]["termination_reason"],
        "exit_nonzero"
    );
    kill_coordinator(child);

    let resumed = resume_real_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        1,
    );
    let document = run_document(&resumed);
    let final_state = read_state(&state);
    let history = attempts(&final_state, "flaky");
    assert_eq!(final_state["status"], "completed");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["run_id"], first_id);
    assert_eq!(history[0]["status"], "failed");
    assert_eq!(history[1]["status"], "succeeded");
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
    assert_eq!(
        document["data"]["reconciliation"][0]["classification"],
        "failed"
    );

    let repeated = resume_real_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        1,
    );
    let repeated_document = run_document(&repeated);
    assert!(
        repeated_document["data"]["reconciliation"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(attempts(&read_state(&state), "flaky").len(), 2);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
}

#[test]
fn real_go_verified_active_attempt_blocks_duplicate_resume() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create active recovery directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let plan = directory.path().join("plan.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "tasks": [{
                "id": "sleep",
                "program": "/usr/bin/sleep",
                "arguments": ["30"],
                "timeout_ms": 60_000
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let state = directory.path().join("aggregate.json");
    let marker = directory.path().join("unused.marker");
    let release = directory.path().join("unused.release");
    let child = real_relay_command(&supervisor, directory.path(), &temporary_root)
        .args([
            "--jobs",
            "1",
            "--state-path",
            state.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real active run");
    let running = wait_for_running_task(&state, "sleep");
    let go_state = PathBuf::from(
        record(&running, "sleep")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    let go_record = wait_for_go_running_state(&go_state);
    assert!(
        go_record["process_identity"]["supervisor"]["pid"]
            .as_i64()
            .unwrap()
            > 0
    );
    assert!(
        go_record["process_identity"]["child"]["pid"]
            .as_i64()
            .unwrap()
            > 0
    );
    assert!(!marker.exists() && !release.exists());
    kill_coordinator(child);

    let resumed = resume_real_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        1,
    );
    let error = error_document(&resumed);
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert_eq!(
        error["data"]["reconciliation"][0]["classification"],
        "still_running"
    );
    assert_eq!(read_state(&state)["tasks"][0]["status"], "running");
    assert_eq!(attempts(&read_state(&state), "sleep").len(), 1);
    kill_real_attempt(&read_state(&state), "sleep");
}

#[test]
fn real_go_rejects_mismatching_process_identity_with_same_pid() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create identity mismatch directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let plan = directory.path().join("plan.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "tasks": [{
                "id": "sleep",
                "program": "/usr/bin/sleep",
                "arguments": ["30"],
                "timeout_ms": 60_000
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let state = directory.path().join("aggregate.json");
    let child = real_relay_command(&supervisor, directory.path(), &temporary_root)
        .args([
            "--jobs",
            "1",
            "--state-path",
            state.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real PID mismatch run");
    let interrupted = wait_for_running_task(&state, "sleep");
    let go_state = PathBuf::from(
        record(&interrupted, "sleep")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    let mut go_record = wait_for_go_running_state(&go_state);
    let child_start = go_record["process_identity"]["child"]["start_time_ticks"]
        .as_u64()
        .unwrap();
    go_record["process_identity"]["child"]["start_time_ticks"] = json!(child_start + 1);
    fs::write(&go_state, serde_json::to_vec(&go_record).unwrap()).unwrap();
    kill_coordinator(child);

    let resumed = resume_real_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        1,
    );
    let error = error_document(&resumed);
    assert_eq!(
        error["data"]["reconciliation"][0]["classification"],
        "indeterminate"
    );
    let final_state = read_state(&state);
    assert_eq!(record(&final_state, "sleep")["status"], "indeterminate");
    assert_eq!(attempts(&final_state, "sleep").len(), 1);
    kill_real_attempt(&interrupted, "sleep");
}

#[test]
fn real_go_untrusted_or_missing_records_never_become_rerunnable() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    for case in [
        "missing",
        "partial",
        "malformed",
        "missing_identity",
        "legacy_active",
        "mismatched_uuid",
        "mismatched_state_path",
        "mismatched_program",
        "mismatched_argv",
        "stale_prior_state",
        "mismatched_protocol",
        "mismatched_supervisor_version",
    ] {
        let directory = tempfile::tempdir().expect("create untrusted record directory");
        let temporary_root = directory.path().join("tmp");
        fs::create_dir(&temporary_root).unwrap();
        let plan = directory.path().join("plan.json");
        fs::write(
            &plan,
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "tasks": [{
                    "id": "sleep",
                    "program": "/usr/bin/sleep",
                    "arguments": ["30"],
                    "timeout_ms": 60_000
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let state = directory.path().join("aggregate.json");
        let child = real_relay_command(&supervisor, directory.path(), &temporary_root)
            .args([
                "--jobs",
                "1",
                "--state-path",
                state.to_str().unwrap(),
                plan.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn untrusted record run");
        let interrupted = wait_for_running_task(&state, "sleep");
        let go_state = PathBuf::from(
            record(&interrupted, "sleep")["supervisor_state_path"]
                .as_str()
                .unwrap(),
        );
        let go_record = wait_for_go_running_state(&go_state);
        kill_coordinator(child);
        kill_real_attempt(&interrupted, "sleep");

        match case {
            "missing" => fs::remove_file(&go_state).unwrap(),
            "partial" => fs::write(&go_state, b"{\"run_state_version\":2").unwrap(),
            "malformed" => fs::write(&go_state, b"not-json").unwrap(),
            "missing_identity" => {
                let mut value = go_record.clone();
                value.as_object_mut().unwrap().remove("process_identity");
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "legacy_active" => {
                let mut value = go_record.clone();
                value["run_state_version"] = json!(1);
                value.as_object_mut().unwrap().remove("state_path");
                value.as_object_mut().unwrap().remove("process_identity");
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "mismatched_uuid" => {
                let mut value = go_record.clone();
                value["execution"]["run_id"] = json!("stale-prior-attempt");
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "mismatched_state_path" => {
                let mut value = go_record.clone();
                value["state_path"] = json!(directory.path().join("other.json"));
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "mismatched_program" => {
                let mut value = go_record.clone();
                value["execution"]["program"] = json!("/bin/false");
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "mismatched_argv" => {
                let mut value = go_record.clone();
                value["execution"]["arguments"] = json!(["unexpected"]);
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "stale_prior_state" => {
                let mut value = go_record;
                value["execution"]["run_id"] = json!("old-attempt");
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "mismatched_protocol" => {
                let mut value = go_record.clone();
                value["execution"]["protocol_version"] = json!(2);
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "mismatched_supervisor_version" => {
                let mut value = go_record;
                value["supervisor_version"] = json!("other-supervisor");
                fs::write(&go_state, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }

        let resumed = resume_real_run(
            &supervisor,
            directory.path(),
            &temporary_root,
            &plan,
            &state,
            1,
        );
        let error = error_document(&resumed);
        assert_eq!(
            error["data"]["reconciliation"][0]["classification"], "indeterminate",
            "case {case}"
        );
        let final_state = read_state(&state);
        assert_eq!(
            record(&final_state, "sleep")["status"],
            "indeterminate",
            "case {case}"
        );
        assert_eq!(attempts(&final_state, "sleep").len(), 1, "case {case}");
    }
}

#[test]
fn real_go_reconciles_persisted_cancellation_without_retry() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create cancellation recovery directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let plan = directory.path().join("plan.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "tasks": [{
                "id": "sleep",
                "program": "/usr/bin/sleep",
                "arguments": ["30"],
                "timeout_ms": 60_000,
                "retry": {"max_attempts": 3, "delay_ms": 0}
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let state = directory.path().join("aggregate.json");
    let marker = directory.path().join("pause.marker");
    let child = spawn_real_crash_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        &marker,
        1,
    );
    let running = wait_for_running_task(&state, "sleep");
    let go_state = PathBuf::from(
        record(&running, "sleep")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    wait_for_go_running_state(&go_state);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    wait_for_path(&marker);
    let go_record = read_state(&go_state);
    assert_eq!(go_record["execution"]["lifecycle"], "cancelled");
    kill_coordinator(child);

    let resumed = resume_real_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        1,
    );
    let document = run_document(&resumed);
    let final_state = read_state(&state);
    assert_eq!(final_state["status"], "cancelled");
    assert_eq!(record(&final_state, "sleep")["status"], "cancelled");
    assert_eq!(attempts(&final_state, "sleep").len(), 1);
    assert_eq!(
        document["data"]["reconciliation"][0]["classification"],
        "cancelled"
    );
}

#[test]
fn real_go_reconciles_parallel_attempts_independently() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create parallel recovery directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let completed_marker = directory.path().join("completed");
    let plan = directory.path().join("plan.json");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "tasks": [
                {
                    "id": "completed",
                    "program": "/usr/bin/touch",
                    "arguments": [completed_marker.to_str().unwrap()],
                    "timeout_ms": 5_000
                },
                {
                    "id": "active",
                    "program": "/usr/bin/sleep",
                    "arguments": ["30"],
                    "timeout_ms": 60_000
                },
                {
                    "id": "uncertain",
                    "program": "/usr/bin/sleep",
                    "arguments": ["30"],
                    "timeout_ms": 60_000
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let state = directory.path().join("aggregate.json");
    let marker = directory.path().join("pause.marker");
    let child = spawn_real_crash_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        &marker,
        3,
    );
    wait_for_path(&marker);
    let interrupted = read_state(&state);
    let active_go_state = PathBuf::from(
        record(&interrupted, "active")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    let uncertain_go_state = PathBuf::from(
        record(&interrupted, "uncertain")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    wait_for_go_running_state(&active_go_state);
    wait_for_go_running_state(&uncertain_go_state);
    assert_eq!(
        read_state(&PathBuf::from(
            record(&interrupted, "completed")["supervisor_state_path"]
                .as_str()
                .unwrap()
        ))["execution"]["lifecycle"],
        "completed"
    );
    kill_real_attempt(&interrupted, "uncertain");
    kill_coordinator(child);

    let resumed = resume_real_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &plan,
        &state,
        3,
    );
    let error = error_document(&resumed);
    let reports = error["data"]["reconciliation"].as_array().unwrap();
    assert_eq!(
        reports
            .iter()
            .find(|report| report["task_id"] == "completed")
            .unwrap()["classification"],
        "completed"
    );
    assert_eq!(
        reports
            .iter()
            .find(|report| report["task_id"] == "active")
            .unwrap()["classification"],
        "still_running"
    );
    assert_eq!(
        reports
            .iter()
            .find(|report| report["task_id"] == "uncertain")
            .unwrap()["classification"],
        "indeterminate"
    );
    let final_state = read_state(&state);
    assert_eq!(record(&final_state, "completed")["status"], "succeeded");
    assert_eq!(record(&final_state, "active")["status"], "running");
    assert_eq!(record(&final_state, "uncertain")["status"], "indeterminate");
    assert_eq!(attempts(&final_state, "completed").len(), 1);
    assert_eq!(attempts(&final_state, "active").len(), 1);
    assert_eq!(attempts(&final_state, "uncertain").len(), 1);
    assert!(completed_marker.is_file());
    kill_real_attempt(&final_state, "active");
}
fn workflow_document(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "workflow envelope unexpectedly wrote stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "workflow stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("workflow JSON envelope");
    assert_eq!(document["output_schema_version"], 1);
    assert_eq!(document["ok"], true);
    assert_eq!(document["command"], "workflow");
    document
}

fn reusable_verify_definition() -> Value {
    json!({
        "schema_version": 1,
        "name": "verify",
        "inputs": {
            "profile": {"type":"string", "default":"dev"}
        },
        "tasks": [
            {
                "id":"inspect",
                "program":"/fake/inspect",
                "arguments":["--profile", {"workflow_input":"profile"}],
                "outputs":[
                    {"name":"summary", "type":"json", "path":"summary.json"},
                    {"name":"report", "type":"file", "path":"report.txt"}
                ]
            },
            {
                "id":"test",
                "depends_on":["inspect"],
                "program":"/fake/test",
                "arguments":["--profile", {"workflow_input":"profile"}],
                "inputs":[
                    {"from_task":"inspect", "output":"summary", "as":"--summary"},
                    {"from_task":"inspect", "output":"report", "as":"--report"}
                ]
            }
        ]
    })
}

#[test]
fn reusable_workflow_inspection_and_execution_reuse_existing_outputs() {
    let harness = Harness::new();
    harness.workflow_file("verify", &reusable_verify_definition());

    let list = workflow_document(&harness.workflow_list_json());
    assert_eq!(list["data"]["workflows"], json!(["verify"]));
    let show = workflow_document(&harness.workflow_show_json("verify"));
    assert_eq!(show["data"]["workflow"]["name"], "verify");
    assert_eq!(
        show["data"]["workflow"]["tasks"][0]["outputs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let human = harness
        .command()
        .args(["--scope", "current", "workflow", "show", "verify"])
        .output()
        .unwrap();
    assert_eq!(human.status.code(), Some(0));
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("Workflow: verify"));
    assert!(human.contains("Tasks:"));
    let checked =
        workflow_document(&harness.workflow_validate_json("verify", &["profile=release"]));
    assert_eq!(checked["data"]["inputs"]["profile"], "release");
    assert_eq!(
        checked["data"]["plan"]["tasks"][0]["arguments"],
        json!(["--profile", "release"])
    );
    assert!(harness.requests().is_empty());

    let first_state = harness.state("workflow-first");
    let first =
        workflow_document(&harness.workflow_run_json("verify", &first_state, &["profile=release"]));
    assert_eq!(first["data"]["run"]["status"], "completed");
    let persisted = read_state(&first_state);
    assert_eq!(persisted["workflow"]["name"], "verify");
    assert_eq!(persisted["workflow"]["inputs"]["profile"], "release");
    assert_eq!(
        persisted["workflow"]["definition_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(record(&persisted, "inspect")["status"], "succeeded");
    assert_eq!(
        record(&persisted, "inspect")["outputs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let requests = harness.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["arguments"], json!(["--profile", "release"]));
    assert_eq!(requests[1]["program"], "/fake/test");
    assert_eq!(requests[1]["arguments"][0], "--profile");
    assert_eq!(requests[1]["arguments"][1], "release");
    assert_eq!(requests[1]["arguments"][2], "--summary");
    assert_eq!(requests[1]["arguments"][4], "--report");

    let second_state = harness.state("workflow-second");
    let second = workflow_document(&harness.workflow_run_json(
        "verify",
        &second_state,
        &["profile=release"],
    ));
    assert_eq!(second["data"]["run"]["status"], "completed");
    assert_eq!(harness.requests().len(), 4);
}

#[test]
fn workflow_resume_keeps_original_plan_after_change_delete_and_malformed_file() {
    let harness = Harness::new();
    let original = reusable_verify_definition();
    harness.workflow_file("verify", &original);
    let state = harness.state("workflow-resume");
    let marker = harness.directory.path().join("workflow-resume.marker");
    let mut child = harness.spawn_workflow_json("verify", &state, &marker);
    harness.wait_for_requests(1);
    let running = wait_for_running_task(&state, "inspect");
    assert_eq!(running["status"], "running");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();

    let mut changed = original.clone();
    changed["tasks"][1]["program"] = json!("/fake/changed");
    harness.workflow_file("verify", &changed);
    let resumed = workflow_document(&harness.resume_workflow_json("verify", &state));
    assert_eq!(resumed["data"]["run"]["status"], "completed");
    assert_eq!(
        resumed["data"]["workflow_notice"],
        "workflow definition changed since run started"
    );
    assert_eq!(
        resumed["data"]["run"]["workflow"]["definition_sha256"],
        running["workflow"]["definition_sha256"]
    );
    let requests = harness.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["program"], "/fake/test");
    assert_ne!(requests[1]["program"], "/fake/changed");
    let completed = read_state(&state);
    assert_eq!(record(&completed, "inspect")["status"], "succeeded");
    assert_eq!(record(&completed, "test")["status"], "succeeded");
    assert_eq!(attempts(&completed, "inspect").len(), 1);

    harness.workflow_file("verify", &original);
    let deleted_state = harness.state("workflow-resume-deleted");
    let deleted_marker = harness
        .directory
        .path()
        .join("workflow-resume-deleted.marker");
    let mut deleted_child = harness.spawn_workflow_json("verify", &deleted_state, &deleted_marker);
    harness.wait_for_requests(3);
    let deleted_running = wait_for_running_task(&deleted_state, "inspect");
    assert_eq!(deleted_running["status"], "running");
    assert_eq!(
        unsafe { libc::kill(deleted_child.id() as i32, libc::SIGKILL) },
        0
    );
    deleted_child.wait().unwrap();
    fs::remove_file(harness.directory.path().join("relay-workflows/verify.json")).unwrap();
    let deleted = workflow_document(&harness.resume_workflow_json("verify", &deleted_state));
    assert_eq!(deleted["data"]["run"]["status"], "completed");
    assert_eq!(attempts(&read_state(&deleted_state), "inspect").len(), 1);
    assert_eq!(harness.requests().len(), 4);
    fs::write(
        harness.directory.path().join("relay-workflows/verify.json"),
        b"{not-json",
    )
    .unwrap();
    let malformed = workflow_document(&harness.resume_workflow_json("verify", &deleted_state));
    assert_eq!(malformed["data"]["run"]["status"], "completed");
    assert_eq!(harness.requests().len(), 4);
}
#[test]
fn workflow_resume_with_redacted_arguments_is_rejected_before_launching() {
    let harness = Harness::new();
    let mut second = task("second", &["first"]);
    second["arguments"] = json!(["workflow-secret"]);
    second["redact_argument_indexes"] = json!([0]);
    harness.workflow_file(
        "redacted-resume",
        &json!({
            "schema_version": 1,
            "name": "redacted-resume",
            "tasks": [task("first", &[]), second]
        }),
    );

    let state = harness.state("workflow-redacted-resume");
    let marker = harness
        .directory
        .path()
        .join("workflow-redacted-resume.marker");
    let mut child = harness.spawn_workflow_json("redacted-resume", &state, &marker);
    harness.wait_for_requests(1);
    let running = wait_for_running_task(&state, "first");
    let first_supervisor_state = PathBuf::from(
        record(&running, "first")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    wait_for_path(&first_supervisor_state);
    wait_for_path(&marker);
    assert_eq!(
        read_state(&first_supervisor_state)["execution"]["lifecycle"],
        "completed"
    );
    assert_eq!(request_names(&harness.requests()), ["first"]);

    let persisted_before = fs::read(&state).unwrap();
    let persisted_text = String::from_utf8(persisted_before.clone()).unwrap();
    assert!(persisted_text.contains("<redacted>"));
    assert!(!persisted_text.contains("workflow-secret"));
    assert_eq!(record(&read_state(&state), "second")["status"], "pending");

    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();
    let before_resume_requests = harness.requests().len();

    let output = harness.resume_workflow_json("redacted-resume", &state);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["command"], "workflow");
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("redacted_argument_indexes")
    );
    assert_eq!(harness.requests().len(), before_resume_requests);
    assert_eq!(fs::read(&state).unwrap(), persisted_before);
}
#[test]
fn invalid_workflow_launches_no_child_process() {
    let harness = Harness::new();
    harness.workflow_file(
        "invalid",
        &json!({
            "schema_version": 1,
            "name": "invalid",
            "tasks": [{"id":"task", "program":"/fake/task"}],
            "unknown": true
        }),
    );
    let state = harness.state("invalid-workflow");
    let output = harness.workflow_run_json("invalid", &state, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert!(!state.exists());
    assert!(harness.requests().is_empty());

    let invalid_name = harness.workflow_run_json("../escape", &harness.state("invalid-name"), &[]);
    assert_eq!(invalid_name.status.code(), Some(1));
    assert!(harness.requests().is_empty());
    let composed = Harness::new();
    composed.workflow_file(
        "release",
        &json!({
            "schema_version": 2,
            "name": "release",
            "tasks": [{"id":"verify","workflow":"missing"}]
        }),
    );
    let composed_state = composed.state("invalid-composition");
    let composed_output = composed.workflow_run_json("release", &composed_state, &[]);
    assert_eq!(composed_output.status.code(), Some(1));
    assert!(composed_output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&composed_output.stderr).contains("missing workflow"));
    assert!(!composed_state.exists());
    assert!(composed.requests().is_empty());
}
#[test]
fn tampered_workflow_metadata_is_rejected_on_resume() {
    let harness = Harness::new();
    harness.workflow_file("verify", &reusable_verify_definition());
    let state = harness.state("tampered-workflow");
    workflow_document(&harness.workflow_run_json("verify", &state, &["profile=release"]));
    let mut persisted = read_state(&state);
    persisted["workflow"]["name"] = json!("../escape");
    fs::write(&state, serde_json::to_vec(&persisted).unwrap()).unwrap();
    let before = harness.requests().len();

    let output = harness.resume_workflow_json("verify", &state);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["command"], "workflow");
    assert_eq!(error["error"]["code"], "run_state_invalid");
    assert_eq!(harness.requests().len(), before);
}

#[test]
fn workflow_reuses_retry_and_failure_propagation_semantics() {
    let harness = Harness::new();
    harness.configure_sequences(&[("flaky", &["nonzero", "success"])]);
    harness.workflow_file(
        "retry",
        &json!({
            "schema_version": 1,
            "name": "retry",
            "tasks": [{
                "id":"flaky",
                "program":"/fake/flaky",
                "retry":{"max_attempts":2,"delay_ms":0}
            }]
        }),
    );
    let retry_state = harness.state("workflow-retry");
    workflow_document(&harness.workflow_run_json("retry", &retry_state, &[]));
    let retry = read_state(&retry_state);
    assert_eq!(retry["status"], "completed");
    assert_eq!(attempts(&retry, "flaky").len(), 2);

    let failure = Harness::new();
    failure.configure(&[("fail", "nonzero")]);
    failure.workflow_file(
        "failure",
        &json!({
            "schema_version": 1,
            "name": "failure",
            "tasks": [
                {"id":"fail","program":"/fake/fail"},
                {"id":"blocked","depends_on":["fail"],"program":"/fake/blocked"}
            ]
        }),
    );
    let failure_state = failure.state("workflow-failure");
    let output = failure.workflow_run_json("failure", &failure_state, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["ok"], true);
    assert_eq!(document["data"]["run"]["status"], "failed");
    let state = read_state(&failure_state);
    assert_eq!(record(&state, "fail")["status"], "failed");
    assert_eq!(record(&state, "blocked")["status"], "blocked");
    assert_eq!(failure.requests().len(), 1);
}

#[test]
fn workflow_cancellation_keeps_existing_coordinator_semantics() {
    let harness = Harness::new();
    harness.configure(&[("waiting", "wait_cancel")]);
    harness.workflow_file(
        "cancel",
        &json!({
            "schema_version": 1,
            "name": "cancel",
            "tasks": [{"id":"waiting", "program":"/fake/waiting"}]
        }),
    );
    let state = harness.state("workflow-cancel");
    let child = harness.spawn_workflow_without_pause("cancel", &state);
    harness.wait_for_requests(1);
    let output = terminate(child);
    assert!(output.stderr.is_empty());
    assert_eq!(output.status.code(), Some(1));
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["command"], "workflow");
    assert_eq!(document["ok"], true);
    assert_eq!(document["data"]["run"]["status"], "cancelled");
    let persisted = read_state(&state);
    assert_eq!(record(&persisted, "waiting")["status"], "cancelled");
    assert_eq!(attempts(&persisted, "waiting").len(), 1);
}

#[test]
fn workflow_honors_bounded_parallelism() {
    let harness = Harness::new();
    harness.workflow_file(
        "parallel",
        &json!({
            "schema_version": 1,
            "name": "parallel",
            "tasks": [
                {"id":"left", "program":"/fake/left"},
                {"id":"right", "program":"/fake/right"}
            ]
        }),
    );
    let state = harness.state("workflow-parallel");
    workflow_document(&harness.workflow_run_json_jobs("parallel", &state, 2, &[]));
    let persisted = read_state(&state);
    assert_eq!(persisted["jobs"], 2);
    assert_eq!(record(&persisted, "left")["status"], "succeeded");
    assert_eq!(record(&persisted, "right")["status"], "succeeded");
    assert_eq!(harness.requests().len(), 2);
}

fn real_workflow_command(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    name: &str,
    state: &Path,
    resume: bool,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_relay"));
    command
        .current_dir(directory)
        .env("TMPDIR", temporary_root)
        .args([
            "--json",
            "--scope",
            "current",
            "workflow",
            "run",
            name,
            "--jobs",
            "1",
            "--supervisor",
        ])
        .arg(supervisor);
    if resume {
        command.args(["--resume", state.to_str().unwrap()]);
    } else {
        command.args([
            "--state-path",
            state.to_str().unwrap(),
            "--input",
            "profile=release",
        ]);
    }
    command
}

fn real_workflow_run(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    name: &str,
    state: &Path,
) -> Output {
    real_workflow_command(supervisor, directory, temporary_root, name, state, false)
        .output()
        .expect("run real reusable workflow")
}

fn real_workflow_resume(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    name: &str,
    state: &Path,
) -> Output {
    real_workflow_command(supervisor, directory, temporary_root, name, state, true)
        .output()
        .expect("resume real reusable workflow")
}

fn spawn_real_workflow_crash(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    name: &str,
    state: &Path,
    marker: &Path,
) -> Child {
    let release = marker.with_extension("release");
    let mut command =
        real_workflow_command(supervisor, directory, temporary_root, name, state, false);
    command
        .env("RELAY_TEST_RUN_PAUSE", "after_supervisor_result")
        .env("RELAY_TEST_RUN_MARKER", marker)
        .env("RELAY_TEST_RUN_RELEASE", release)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn().expect("spawn real reusable workflow")
}

fn real_verify_definition(counter: &Path) -> Value {
    let counter = counter.to_str().unwrap();
    let inspect = r#"import json,os,sys
with open(sys.argv[1],"a") as f: f.write("inspect\n")
manifest=json.loads(os.environ["RELAY_OUTPUT_MANIFEST"])
with open(manifest["summary"],"w") as f: json.dump({"kind":"inspect","ok":True},f)
with open(manifest["report"],"w") as f: f.write("inspect artifact\n")
"#;
    let format = r#"import sys
with open(sys.argv[1],"a") as f: f.write("format\n")
"#;
    let test = r#"import json,os,sys
assert sys.argv[2] == "--profile" and sys.argv[3] == "release"
assert sys.argv[4] == "--summary" and json.loads(sys.argv[5])["kind"] == "inspect"
assert sys.argv[6] == "--report" and os.path.isfile(sys.argv[7])
with open(sys.argv[1],"a") as f: f.write("test\n")
manifest=json.loads(os.environ["RELAY_OUTPUT_MANIFEST"])
with open(manifest["summary"],"w") as f: json.dump({"kind":"test","ok":True},f)
with open(manifest["report"],"w") as f: f.write("test artifact\n")
"#;
    let report = r#"import os,sys
assert sys.argv[2] == "--test-report" and os.path.isfile(sys.argv[3])
with open(sys.argv[1],"a") as f: f.write("report\n")
manifest=os.environ["RELAY_OUTPUT_MANIFEST"]
paths=__import__("json").loads(manifest)
with open(paths["final"],"w") as f: f.write("final report\n")
"#;
    json!({
        "schema_version": 1,
        "name": "verify",
        "inputs": {"profile": {"type":"string", "required":true}},
        "tasks": [
            {
                "id":"inspect",
                "program":"/usr/bin/python3",
                "arguments":["-c", inspect, counter],
                "outputs":[
                    {"name":"summary", "type":"json", "path":"inspect.json"},
                    {"name":"report", "type":"file", "path":"inspect.txt"}
                ]
            },
            {
                "id":"format",
                "program":"/usr/bin/python3",
                "arguments":["-c", format, counter]
            },
            {
                "id":"test",
                "depends_on":["inspect","format"],
                "program":"/usr/bin/python3",
                "arguments":["-c", test, counter, "--profile", {"workflow_input":"profile"}],
                "outputs":[
                    {"name":"summary", "type":"json", "path":"test.json"},
                    {"name":"report", "type":"file", "path":"test.txt"}
                ],
                "inputs":[
                    {"from_task":"inspect", "output":"summary", "as":"--summary"},
                    {"from_task":"inspect", "output":"report", "as":"--report"}
                ]
            },
            {
                "id":"report",
                "depends_on":["test"],
                "program":"/usr/bin/python3",
                "arguments":["-c", report, counter],
                "outputs":[{"name":"final", "type":"file", "path":"final.txt"}],
                "inputs":[{"from_task":"test", "output":"report", "as":"--test-report"}]
            }
        ]
    })
}

fn real_composed_verify_definition(
    counter: &Path,
    retry_flag: &Path,
    ready_flag: &Path,
    verify_complete: &Path,
) -> Value {
    let counter = counter.to_str().unwrap();
    let retry_flag = retry_flag.to_str().unwrap();
    let ready_flag = ready_flag.to_str().unwrap();
    let verify_complete = verify_complete.to_str().unwrap();
    let inspect = r#"import json,os,sys
counter,ready,profile=sys.argv[1],sys.argv[2],sys.argv[3]
assert profile == "release" and os.path.isfile(ready)
with open(counter,"a") as f: f.write("inspect\n")
paths=json.loads(os.environ["RELAY_OUTPUT_MANIFEST"])
with open(paths["summary"],"w") as f: json.dump({"kind":"inspect","profile":profile},f)
with open(paths["report"],"w") as f: f.write("inspect report\n")
"#;
    let format = r#"import os,sys
retry_flag,counter,ready=sys.argv[1],sys.argv[2],sys.argv[3]
assert os.path.isfile(ready)
if not os.path.exists(retry_flag):
    open(retry_flag,"w").close()
    sys.exit(17)
with open(counter,"a") as f: f.write("format\n")
"#;
    let test = r#"import json,os,sys
counter,profile,complete=sys.argv[1],sys.argv[2],sys.argv[3]
assert profile == "release"
assert sys.argv[4] == "--summary"
assert json.loads(sys.argv[5])["kind"] == "inspect"
assert sys.argv[6] == "--report" and os.path.isfile(sys.argv[7])
with open(counter,"a") as f: f.write("test\n")
open(complete,"w").close()
paths=json.loads(os.environ["RELAY_OUTPUT_MANIFEST"])
with open(paths["summary"],"w") as f: json.dump({"kind":"test"},f)
with open(paths["report"],"w") as f: f.write("test report\n")
"#;
    json!({
        "schema_version": 2,
        "name": "verify",
        "inputs": {"profile": {"type":"string", "required":true}},
        "tasks": [
            {"id":"inspect", "program":"/usr/bin/python3", "arguments":["-c", inspect, counter, ready_flag, {"workflow_input":"profile"}],
             "outputs":[{"name":"summary","type":"json","path":"summary.json"},{"name":"report","type":"file","path":"inspect.txt"}]},
            {"id":"format", "program":"/usr/bin/python3", "arguments":["-c", format, retry_flag, counter, ready_flag], "retry":{"max_attempts":2,"delay_ms":0}},
            {"id":"test", "depends_on":["inspect","format"], "program":"/usr/bin/python3", "arguments":["-c", test, counter, {"workflow_input":"profile"}, verify_complete],
             "outputs":[{"name":"summary","type":"json","path":"test.json"},{"name":"report","type":"file","path":"test.txt"}],
             "inputs":[{"from_task":"inspect","output":"summary","as":"--summary"},{"from_task":"inspect","output":"report","as":"--report"}]}
        ],
        "exports":{"report":{"from_task":"test","output":"report"}}
    })
}

fn real_composed_package_definition(
    counter: &Path,
    verify_complete: &Path,
    package_complete: &Path,
) -> Value {
    let counter = counter.to_str().unwrap();
    let verify_complete = verify_complete.to_str().unwrap();
    let package_complete = package_complete.to_str().unwrap();
    let prepare = r#"import os,sys
assert os.path.isfile(sys.argv[1])
with open(sys.argv[2],"a") as f: f.write("package-prepare\n")
"#;
    let package = r#"import json,os,sys
with open(sys.argv[1],"a") as f: f.write("package\n")
open(sys.argv[2],"w").close()
paths=json.loads(os.environ["RELAY_OUTPUT_MANIFEST"])
with open(paths["package"],"w") as f: f.write("package\n")
"#;
    json!({
        "schema_version": 1,
        "name": "package",
        "tasks": [
            {"id":"package-prepare","program":"/usr/bin/python3","arguments":["-c",prepare,verify_complete,counter]},
            {"id":"package","depends_on":["package-prepare"],"program":"/usr/bin/python3","arguments":["-c",package,counter,package_complete],"outputs":[{"name":"package","type":"file","path":"package.txt"}]}
        ]
    })
}

fn real_composed_release_definition(
    counter: &Path,
    ready_flag: &Path,
    package_complete: &Path,
) -> Value {
    let counter = counter.to_str().unwrap();
    let ready_flag = ready_flag.to_str().unwrap();
    let package_complete = package_complete.to_str().unwrap();
    let prepare = r#"import sys
with open(sys.argv[1],"w") as f: f.write("ready\n")
"#;
    let publish = r#"import json,os,sys
assert os.path.isfile(sys.argv[2])
assert sys.argv[3] == "--report" and os.path.isfile(sys.argv[4])
with open(sys.argv[1],"a") as f: f.write("publish\n")
paths=json.loads(os.environ["RELAY_OUTPUT_MANIFEST"])
with open(paths["final"],"w") as f: f.write("published\n")
"#;
    json!({
        "schema_version": 2,
        "name": "release",
        "inputs": {"profile": {"type":"string", "required":true}},
        "tasks": [
            {"id":"prepare","program":"/usr/bin/python3","arguments":["-c",prepare,ready_flag]},
            {"id":"verify-instance","workflow":"verify","depends_on":["prepare"],"inputs":{"profile":{"workflow_input":"profile"}}},
            {"id":"package-instance","workflow":"package","depends_on":["verify-instance"]},
            {"id":"publish","depends_on":["verify-instance","package-instance"],"program":"/usr/bin/python3",
             "arguments":["-c",publish,counter,package_complete],"inputs":[{"from_task":"verify-instance","output":"report","as":"--report"}],
             "outputs":[{"name":"final","type":"file","path":"final.txt"}]}
        ]
    })
}

fn real_composed_workflow_command(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    state: &Path,
    resume: bool,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_relay"));
    command
        .current_dir(directory)
        .env("TMPDIR", temporary_root)
        .args([
            "--json",
            "--scope",
            "current",
            "workflow",
            "run",
            "release",
            "--jobs",
            "2",
            "--supervisor",
        ])
        .arg(supervisor);
    if resume {
        command.args(["--resume", state.to_str().unwrap()]);
    } else {
        command.args([
            "--state-path",
            state.to_str().unwrap(),
            "--input",
            "profile=release",
        ]);
    }
    command
}

fn real_composed_workflow_run(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    state: &Path,
) -> Output {
    real_composed_workflow_command(supervisor, directory, temporary_root, state, false)
        .output()
        .expect("run real composed workflow")
}

fn real_composed_workflow_resume(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    state: &Path,
) -> Output {
    real_composed_workflow_command(supervisor, directory, temporary_root, state, true)
        .output()
        .expect("resume real composed workflow")
}

fn spawn_real_composed_workflow_crash(
    supervisor: &Path,
    directory: &Path,
    temporary_root: &Path,
    state: &Path,
    marker: &Path,
) -> Child {
    let release = marker.with_extension("release");
    real_composed_workflow_command(supervisor, directory, temporary_root, state, false)
        .env("RELAY_TEST_RUN_PAUSE", "after_supervisor_result")
        .env("RELAY_TEST_RUN_MARKER", marker)
        .env("RELAY_TEST_RUN_RELEASE", release)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real composed workflow")
}

#[test]
fn real_go_composed_workflow_expands_retries_and_resumes_immutably() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create composed workflow directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let workflows = directory.path().join("relay-workflows");
    fs::create_dir(&workflows).unwrap();
    let counter = directory.path().join("counter");
    let retry_flag = directory.path().join("retry.flag");
    let ready_flag = directory.path().join("ready.flag");
    let verify_complete = directory.path().join("verify.complete");
    let package_complete = directory.path().join("package.complete");
    let verify =
        real_composed_verify_definition(&counter, &retry_flag, &ready_flag, &verify_complete);
    fs::write(
        workflows.join("verify.json"),
        serde_json::to_vec_pretty(&verify).unwrap(),
    )
    .unwrap();
    fs::write(
        workflows.join("package.json"),
        serde_json::to_vec_pretty(&real_composed_package_definition(
            &counter,
            &verify_complete,
            &package_complete,
        ))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        workflows.join("release.json"),
        serde_json::to_vec_pretty(&real_composed_release_definition(
            &counter,
            &ready_flag,
            &package_complete,
        ))
        .unwrap(),
    )
    .unwrap();

    let state = directory.path().join("release.state.json");
    let output = workflow_document(&real_composed_workflow_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        &state,
    ));
    assert_eq!(output["data"]["run"]["status"], "completed");
    let persisted = read_state(&state);
    assert_eq!(persisted["jobs"], 2);
    assert_eq!(
        record(&persisted, "verify-instance.format")["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        record(&persisted, "verify-instance.inspect")["outputs"][0]["producer_task"],
        "verify-instance.inspect"
    );
    assert_eq!(
        record(&persisted, "publish")["outputs"][0]["producer_task"],
        "publish"
    );
    let inspect = &record(&persisted, "verify-instance.inspect")["attempts"][0]["summary"];
    let format = &record(&persisted, "verify-instance.format")["attempts"][0]["summary"];
    assert!(
        inspect["started_at"].as_str().unwrap() < format["completed_at"].as_str().unwrap()
            && format["started_at"].as_str().unwrap() < inspect["completed_at"].as_str().unwrap()
    );
    assert!(ready_flag.is_file());
    assert!(verify_complete.is_file());
    assert!(package_complete.is_file());
    assert_eq!(
        record(&persisted, "package-instance.package-prepare")["status"],
        "succeeded"
    );
    let counter_text = fs::read_to_string(&counter).unwrap();
    assert!(counter_text.contains("inspect"));
    assert_eq!(counter_text.matches("format").count(), 1);
    assert!(counter_text.contains("test"));
    assert!(counter_text.contains("package-prepare"));
    assert!(counter_text.contains("package"));
    assert!(counter_text.contains("publish"));

    let resume_counter = directory.path().join("resume-counter");
    let resume_retry = directory.path().join("resume.retry.flag");
    let resume_ready = directory.path().join("resume.ready");
    let resume_verify_complete = directory.path().join("resume.verify.complete");
    let resume_package_complete = directory.path().join("resume.package.complete");
    fs::write(
        workflows.join("verify.json"),
        serde_json::to_vec_pretty(&real_composed_verify_definition(
            &resume_counter,
            &resume_retry,
            &resume_ready,
            &resume_verify_complete,
        ))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        workflows.join("package.json"),
        serde_json::to_vec_pretty(&real_composed_package_definition(
            &resume_counter,
            &resume_verify_complete,
            &resume_package_complete,
        ))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        workflows.join("release.json"),
        serde_json::to_vec_pretty(&real_composed_release_definition(
            &resume_counter,
            &resume_ready,
            &resume_package_complete,
        ))
        .unwrap(),
    )
    .unwrap();
    let resume_state = directory.path().join("resume.state.json");
    let marker = directory.path().join("resume.marker");
    let mut child = spawn_real_composed_workflow_crash(
        &supervisor,
        directory.path(),
        &temporary_root,
        &resume_state,
        &marker,
    );
    wait_for_path(&marker);
    let interrupted = read_state(&resume_state);
    let original_definition_hash = interrupted["workflow"]["definition_sha256"].clone();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();
    fs::write(workflows.join("verify.json"), b"{\"schema_version\":2,").unwrap();
    fs::remove_file(workflows.join("package.json")).unwrap();
    let resumed = workflow_document(&real_composed_workflow_resume(
        &supervisor,
        directory.path(),
        &temporary_root,
        &resume_state,
    ));
    assert_eq!(resumed["data"]["run"]["status"], "completed");
    assert_eq!(
        resumed["data"]["run"]["workflow"]["definition_sha256"],
        original_definition_hash
    );
    let final_state = read_state(&resume_state);
    assert_eq!(
        record(&final_state, "verify-instance.inspect")["attempts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        record(&final_state, "package-instance.package")["attempts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(resume_ready.is_file());
    assert!(resume_verify_complete.is_file());
    assert!(resume_package_complete.is_file());
    assert_eq!(
        record(&final_state, "package-instance.package-prepare")["status"],
        "succeeded"
    );
    for task in [
        "verify-instance.inspect",
        "verify-instance.test",
        "package-instance.package",
        "publish",
    ] {
        assert_eq!(record(&final_state, task)["status"], "succeeded");
        for output in record(&final_state, task)["outputs"].as_array().unwrap() {
            assert_eq!(output["producer_task"], task);
            let latest = attempts(&final_state, task).last().unwrap();
            assert_eq!(output["attempt_run_id"], latest["run_id"]);
            assert_eq!(output["run_id"], final_state["run_id"]);
        }
    }
}
#[test]
fn real_go_reusable_workflow_runs_repeats_and_resumes_original_plan() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let directory = tempfile::tempdir().expect("create real workflow directory");
    let temporary_root = directory.path().join("tmp");
    fs::create_dir(&temporary_root).unwrap();
    let workflows = directory.path().join("relay-workflows");
    fs::create_dir(&workflows).unwrap();

    let first_counter = directory.path().join("first-counter");
    let first_definition = real_verify_definition(&first_counter);
    let workflow_path = workflows.join("verify.json");
    fs::write(
        &workflow_path,
        serde_json::to_vec_pretty(&first_definition).unwrap(),
    )
    .unwrap();
    let first_state = directory.path().join("first.state.json");
    let first = workflow_document(&real_workflow_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        "verify",
        &first_state,
    ));
    assert_eq!(first["data"]["run"]["status"], "completed");
    assert_eq!(
        fs::read_to_string(&first_counter).unwrap().lines().count(),
        4
    );
    let first_persisted = read_state(&first_state);
    assert_eq!(first_persisted["workflow"]["inputs"]["profile"], "release");
    let inspect_output = &record(&first_persisted, "inspect")["outputs"][0];
    assert_eq!(
        inspect_output["attempt_run_id"],
        attempts(&first_persisted, "inspect")[0]["run_id"]
    );
    assert_eq!(inspect_output["producer_task"], "inspect");

    let second_counter = directory.path().join("second-counter");
    fs::write(
        &workflow_path,
        serde_json::to_vec_pretty(&real_verify_definition(&second_counter)).unwrap(),
    )
    .unwrap();
    let second_state = directory.path().join("second.state.json");
    let second = workflow_document(&real_workflow_run(
        &supervisor,
        directory.path(),
        &temporary_root,
        "verify",
        &second_state,
    ));
    assert_eq!(second["data"]["run"]["status"], "completed");
    assert_eq!(
        fs::read_to_string(&second_counter).unwrap().lines().count(),
        4
    );

    let resume_counter = directory.path().join("resume-counter");
    let original = real_verify_definition(&resume_counter);
    fs::write(
        &workflow_path,
        serde_json::to_vec_pretty(&original).unwrap(),
    )
    .unwrap();
    let resume_state = directory.path().join("resume.state.json");
    let marker = directory.path().join("resume.marker");
    let mut child = spawn_real_workflow_crash(
        &supervisor,
        directory.path(),
        &temporary_root,
        "verify",
        &resume_state,
        &marker,
    );
    let running = wait_for_running_task(&resume_state, "inspect");
    assert_eq!(running["status"], "running");
    let go_state = PathBuf::from(
        record(&running, "inspect")["supervisor_state_path"]
            .as_str()
            .unwrap(),
    );
    wait_for_path(&marker);
    let mut go_record = read_state(&go_state);
    assert_eq!(go_record["run_state_version"], 2);
    assert_eq!(go_record["execution"]["lifecycle"], "completed");
    go_record["run_state_version"] = json!(1);
    go_record.as_object_mut().unwrap().remove("state_path");
    go_record
        .as_object_mut()
        .unwrap()
        .remove("process_identity");
    fs::write(&go_state, serde_json::to_vec(&go_record).unwrap()).unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    child.wait().unwrap();

    let mut changed = original.clone();
    changed["tasks"][3]["program"] = json!("/usr/bin/false");
    fs::write(&workflow_path, serde_json::to_vec_pretty(&changed).unwrap()).unwrap();
    let resumed = workflow_document(&real_workflow_resume(
        &supervisor,
        directory.path(),
        &temporary_root,
        "verify",
        &resume_state,
    ));
    assert_eq!(resumed["data"]["run"]["status"], "completed");
    assert_eq!(
        resumed["data"]["workflow_notice"],
        "workflow definition changed since run started"
    );
    assert_eq!(
        resumed["data"]["run"]["workflow"]["definition_sha256"],
        running["workflow"]["definition_sha256"]
    );
    assert_eq!(
        fs::read_to_string(&resume_counter).unwrap().lines().count(),
        4
    );
    let final_state = read_state(&resume_state);
    assert_eq!(attempts(&final_state, "inspect").len(), 1);
    assert_eq!(record(&final_state, "inspect")["status"], "succeeded");
    assert_eq!(record(&final_state, "test")["status"], "succeeded");
    assert_eq!(record(&final_state, "report")["status"], "succeeded");
    for task in ["inspect", "test", "report"] {
        for output in record(&final_state, task)["outputs"].as_array().unwrap() {
            assert_eq!(output["producer_task"], task);
            assert_eq!(
                output["attempt_run_id"],
                attempts(&final_state, task)[0]["run_id"]
            );
        }
    }
}
#[test]
fn handoff_carries_trusted_run_outputs_without_copying_artifacts() {
    let harness = Harness::new();
    let start = harness
        .command()
        .args(["--scope", "current", "start", "handoff run"])
        .output()
        .unwrap();
    assert!(start.status.success(), "start failed: {:?}", start);

    let mut producer = task("produce", &[]);
    producer["outputs"] = json!([
        {"name":"summary","type":"json","path":"summary.json"},
        {"name":"report","type":"file","path":"report.txt"}
    ]);
    producer["arguments"] = json!(["handoff-secret"]);
    producer["redact_argument_indexes"] = json!([0]);
    let plan = harness.plan_v3("handoff-run", json!([producer]));
    let state = harness
        .directory
        .path()
        .join(".relay/runs/handoff-run.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let run = harness.run_json(&plan, &state);
    let run_document = run_document(&run);
    assert_eq!(run_document["data"]["run"]["status"], "completed");

    let handoff = harness
        .command()
        .args([
            "--json",
            "--scope",
            "current",
            "handoff",
            "create",
            "--to",
            "agent-b",
            "--source-agent",
            "agent-a",
            "--run",
            state.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        handoff.status.success(),
        "handoff failed: {}",
        String::from_utf8_lossy(&handoff.stderr)
    );
    let document: Value = serde_json::from_slice(&handoff.stdout).unwrap();
    let record = &document["data"]["handoff"];
    assert_eq!(record["schema_version"], 2);
    assert_eq!(record["source_agent"], "agent-a");
    assert_eq!(record["target_agent"], "agent-b");
    assert_eq!(record["references"]["runs"][0]["status"], "completed");
    let record_json = serde_json::to_string(record).unwrap();
    assert!(!record_json.contains("handoff-secret"));
    assert!(!record_json.contains("<redacted>"));
    let outputs = record["references"]["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 2);
    assert!(outputs.iter().all(|output| {
        output["run_id"] == run_document["data"]["run"]["run_id"]
            && output["attempt"] == 1
            && output["attempt_run_id"].is_string()
            && output["sha256"]
                .as_str()
                .is_some_and(|hash| hash.len() == 64)
            && output["path"]
                .as_str()
                .is_some_and(|path| path.starts_with(".relay/runs/outputs/"))
    }));
    assert!(
        !fs::read_to_string(harness.directory.path().join(".relay/handoffs.jsonl"))
            .unwrap()
            .contains("fake report")
    );
    let ledger = harness.directory.path().join(".relay/handoffs.jsonl");
    let ledger_before = fs::read(&ledger).unwrap();
    let output_path = harness
        .directory
        .path()
        .join(outputs[0]["path"].as_str().unwrap());
    let id = record["id"].as_str().unwrap();
    let assert_snapshot = |args: &[&str]| {
        let reread = harness.command().args(args).output().unwrap();
        assert!(
            reread.status.success(),
            "historical handoff read failed: {}",
            String::from_utf8_lossy(&reread.stderr)
        );
        let reread_document: Value = serde_json::from_slice(&reread.stdout).unwrap();
        assert_eq!(&reread_document["data"]["handoff"], record);
    };
    for args in [
        ["--json", "--scope", "current", "handoff", "--show", id],
        ["--json", "--scope", "current", "context", "--handoff", id],
    ] {
        assert_snapshot(&args);
    }
    fs::write(&output_path, b"tampered").unwrap();
    for args in [
        ["--json", "--scope", "current", "handoff", "--show", id],
        ["--json", "--scope", "current", "context", "--handoff", id],
    ] {
        assert_snapshot(&args);
    }
    fs::remove_file(&output_path).unwrap();
    for args in [
        ["--json", "--scope", "current", "handoff", "--show", id],
        ["--json", "--scope", "current", "context", "--handoff", id],
    ] {
        assert_snapshot(&args);
    }
    fs::write(&state, b"changed live run state").unwrap();
    for args in [
        ["--json", "--scope", "current", "handoff", "--show", id],
        ["--json", "--scope", "current", "context", "--handoff", id],
    ] {
        assert_snapshot(&args);
    }
    fs::remove_file(&state).unwrap();
    for args in [
        ["--json", "--scope", "current", "handoff", "--show", id],
        ["--json", "--scope", "current", "context", "--handoff", id],
    ] {
        assert_snapshot(&args);
    }
    assert_eq!(fs::read(&ledger).unwrap(), ledger_before);

    let second = harness
        .command()
        .args(["--json", "--scope", "current", "handoff", "agent-c"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "second handoff failed: {:?}",
        second
    );
    let second_document: Value = serde_json::from_slice(&second.stdout).unwrap();
    let second_id = second_document["data"]["handoff"]["id"].as_str().unwrap();
    assert_ne!(id, second_id);

    let listed = harness
        .command()
        .args(["--json", "--scope", "current", "handoff", "--list"])
        .output()
        .unwrap();
    assert!(listed.status.success(), "listing stale handoff failed");
    let listed_document: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(
        listed_document["data"]["handoffs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let latest = harness
        .command()
        .args(["--json", "--scope", "current", "handoff", "--latest"])
        .output()
        .unwrap();
    assert!(latest.status.success(), "latest stale handoff failed");
    let latest_document: Value = serde_json::from_slice(&latest.stdout).unwrap();
    assert_eq!(latest_document["data"]["handoff"]["id"], second_id);
    let unrelated = harness
        .command()
        .args([
            "--json", "--scope", "current", "handoff", "--show", second_id,
        ])
        .output()
        .unwrap();
    assert!(unrelated.status.success(), "unrelated handoff show failed");
    let unrelated_document: Value = serde_json::from_slice(&unrelated.stdout).unwrap();
    assert_eq!(unrelated_document["data"]["handoff"]["id"], second_id);
}
#[test]
fn handoff_reads_running_run_without_taking_aggregate_lock() {
    let harness = Harness::new();
    let start = harness
        .command()
        .args(["--scope", "current", "start", "running handoff"])
        .output()
        .unwrap();
    assert!(start.status.success(), "start failed: {:?}", start);

    let plan = harness.plan_v3("running-handoff", json!([task("holding", &[])]));
    let state = harness
        .directory
        .path()
        .join(".relay/runs/running-handoff.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let marker = harness.directory.path().join("running.marker");
    let release = harness.directory.path().join("running.release");
    let mut child = harness
        .command()
        .env("RELAY_TEST_RUN_PAUSE", "before_supervisor_launch")
        .env("RELAY_TEST_RUN_MARKER", &marker)
        .env("RELAY_TEST_RUN_RELEASE", &release)
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--state-path",
            state.to_str().unwrap(),
            plan.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    wait_for_path(&marker);

    let handoff = harness
        .command()
        .args([
            "--json",
            "--scope",
            "current",
            "handoff",
            "create",
            "--to",
            "agent-b",
            "--run",
            state.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(handoff.status.success(), "handoff failed: {:?}", handoff);
    let document: Value = serde_json::from_slice(&handoff.stdout).unwrap();
    assert_eq!(
        document["data"]["handoff"]["references"]["runs"][0]["status"],
        "running"
    );
    assert!(
        document["data"]["handoff"]["references"]["outputs"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    fs::write(&release, b"release").unwrap();
    assert!(child.wait().unwrap().success());
}
#[test]
fn failed_cancelled_and_indeterminate_runs_never_publish_handoff_outputs() {
    for (label, mode, expected_status) in [
        ("failed", "nonzero", "failed"),
        ("cancelled", "cancelled", "cancelled"),
        ("indeterminate", "malformed_output", "indeterminate"),
    ] {
        let harness = Harness::new();
        let start = harness
            .command()
            .args(["--scope", "current", "start", label])
            .output()
            .unwrap();
        assert!(start.status.success(), "start failed: {:?}", start);
        let task_id = format!("{label}-task");
        harness.configure(&[(task_id.as_str(), mode)]);
        let mut producer = task(&task_id, &[]);
        producer["outputs"] = json!([
            {"name":"summary","type":"json","path":"summary.json"},
            {"name":"report","type":"file","path":"report.txt"}
        ]);
        let plan = harness.plan_v3(label, json!([producer]));
        let state = harness
            .directory
            .path()
            .join(format!(".relay/runs/{label}.json"));
        fs::create_dir_all(state.parent().unwrap()).unwrap();
        let run = run_document(&harness.run_json(&plan, &state));
        assert_eq!(run["data"]["run"]["status"], expected_status);

        let handoff = harness
            .command()
            .args([
                "--json",
                "--scope",
                "current",
                "handoff",
                "create",
                "--to",
                "agent-b",
                "--run",
                state.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(handoff.status.success(), "handoff failed: {:?}", handoff);
        let document: Value = serde_json::from_slice(&handoff.stdout).unwrap();
        assert_eq!(
            document["data"]["handoff"]["references"]["runs"][0]["status"],
            expected_status
        );
        assert!(
            document["data"]["handoff"]["references"]["outputs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
#[test]
fn stale_run_output_is_rejected_before_handoff_creation() {
    let harness = Harness::new();
    let start = harness
        .command()
        .args(["--scope", "current", "start", "stale output"])
        .output()
        .unwrap();
    assert!(start.status.success(), "start failed: {:?}", start);
    let mut producer = task("produce", &[]);
    producer["outputs"] = json!([{"name":"report","type":"file","path":"report.txt"}]);
    let plan = harness.plan_v3("stale-output", json!([producer]));
    let state = harness
        .directory
        .path()
        .join(".relay/runs/stale-output.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let run = run_document(&harness.run_json(&plan, &state));
    assert_eq!(run["data"]["run"]["status"], "completed");
    let output_path = PathBuf::from(
        record(&read_state(&state), "produce")["outputs"][0]["path"]
            .as_str()
            .unwrap(),
    );
    fs::write(&output_path, b"tampered").unwrap();

    let handoff = harness
        .command()
        .args([
            "--json",
            "--scope",
            "current",
            "handoff",
            "create",
            "--to",
            "agent-b",
            "--run",
            state.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!handoff.status.success());
    assert!(handoff.stdout.is_empty());
    assert!(String::from_utf8_lossy(&handoff.stderr).contains("changed after persistence"));
    assert!(
        !harness
            .directory
            .path()
            .join(".relay/handoffs.jsonl")
            .exists()
    );
}
#[test]
fn handoff_uses_latest_successful_retry_provenance() {
    let harness = Harness::new();
    let start = harness
        .command()
        .args(["--scope", "current", "start", "retry handoff"])
        .output()
        .unwrap();
    assert!(start.status.success(), "start failed: {:?}", start);
    harness.configure_sequences(&[("produce", &["nonzero", "success"])]);
    let mut producer = task("produce", &[]);
    producer["retry"] = json!({"max_attempts":2,"delay_ms":0});
    producer["outputs"] = json!([{"name":"report","type":"file","path":"report.txt"}]);
    let plan = harness.plan_v3("retry-handoff", json!([producer]));
    let state = harness
        .directory
        .path()
        .join(".relay/runs/retry-handoff.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let run = run_document(&harness.run_json(&plan, &state));
    assert_eq!(run["data"]["run"]["status"], "completed");
    let persisted = read_state(&state);
    assert_eq!(attempts(&persisted, "produce").len(), 2);
    assert_eq!(record(&persisted, "produce")["outputs"][0]["attempt"], 2);

    let handoff = harness
        .command()
        .args([
            "--json",
            "--scope",
            "current",
            "handoff",
            "create",
            "--to",
            "agent-b",
            "--run",
            state.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(handoff.status.success(), "handoff failed: {:?}", handoff);
    let document: Value = serde_json::from_slice(&handoff.stdout).unwrap();
    let output = &document["data"]["handoff"]["references"]["outputs"][0];
    assert_eq!(output["attempt"], 2);
    assert_eq!(
        output["attempt_run_id"],
        attempts(&persisted, "produce")[1]["run_id"]
    );
}
#[test]
fn forged_output_declaration_is_rejected_before_outside_reads() {
    let harness = Harness::new();
    let start = harness
        .command()
        .args(["--scope", "current", "start", "forged plan"])
        .output()
        .unwrap();
    assert!(start.status.success(), "start failed: {:?}", start);
    let mut producer = task("produce", &[]);
    producer["outputs"] = json!([{"name":"report","type":"file","path":"report.txt"}]);
    let plan = harness.plan_v3("forged-plan", json!([producer]));
    let state = harness
        .directory
        .path()
        .join(".relay/runs/forged-plan.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    run_document(&harness.run_json(&plan, &state));

    let outside = harness
        .directory
        .path()
        .parent()
        .unwrap()
        .join("forged-outside.txt");
    fs::write(&outside, b"sentinel").unwrap();
    let mut persisted = read_state(&state);
    persisted["plan"]["tasks"][0]["outputs"][0]["path"] = json!("../../../../forged-outside.txt");
    fs::write(&state, serde_json::to_vec(&persisted).unwrap()).unwrap();

    let handoff = harness
        .command()
        .args([
            "--json",
            "--scope",
            "current",
            "handoff",
            "create",
            "--to",
            "agent-b",
            "--run",
            state.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!handoff.status.success());
    assert!(handoff.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&handoff.stderr).contains("output path must be a relative path")
    );
    assert_eq!(fs::read(&outside).unwrap(), b"sentinel");
    assert!(
        !harness
            .directory
            .path()
            .join(".relay/handoffs.jsonl")
            .exists()
    );
}
#[test]
fn missing_and_cross_project_run_references_are_rejected() {
    let harness = Harness::new();
    let start = harness
        .command()
        .args(["--scope", "current", "start", "run references"])
        .output()
        .unwrap();
    assert!(start.status.success(), "start failed: {:?}", start);
    let missing = harness.directory.path().join(".relay/runs/missing.json");
    fs::create_dir_all(missing.parent().unwrap()).unwrap();
    let outside = tempfile::tempdir().unwrap();
    for requested in [missing, outside.path().join("outside.json")] {
        let handoff = harness
            .command()
            .args([
                "--json",
                "--scope",
                "current",
                "handoff",
                "create",
                "--to",
                "agent-b",
                "--run",
                requested.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(!handoff.status.success());
        assert!(handoff.stdout.is_empty());
    }
    assert!(
        !harness
            .directory
            .path()
            .join(".relay/handoffs.jsonl")
            .exists()
    );
}

fn inspection_snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, std::time::SystemTime)> {
    fn visit(
        root: &Path,
        path: &Path,
        entries: &mut Vec<(PathBuf, Option<Vec<u8>>, std::time::SystemTime)>,
    ) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let contents = if path.is_dir() {
                None
            } else {
                Some(fs::read(&path).unwrap())
            };
            entries.push((
                path.strip_prefix(root).unwrap().to_owned(),
                contents,
                fs::metadata(&path).unwrap().modified().unwrap(),
            ));
            if path.is_dir() {
                visit(root, &path, entries);
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort();
    entries
}

#[test]
fn workflow_explain_and_dry_run_are_read_only_and_match_real_identity() {
    let harness = Harness::new();
    harness.workflow_file("verify", &reusable_verify_definition());
    let before = inspection_snapshot(harness.directory.path());
    let explain = workflow_document(
        &harness
            .command()
            .args([
                "--json",
                "workflow",
                "explain",
                "verify",
                "--input",
                "profile=release",
            ])
            .output()
            .unwrap(),
    );
    let dry = workflow_document(
        &harness
            .command()
            .args([
                "--json",
                "workflow",
                "run",
                "verify",
                "--dry-run",
                "--input",
                "profile=release",
            ])
            .arg("--supervisor")
            .arg(&harness.supervisor)
            .arg("--state-path")
            .arg(harness.state("dry"))
            .output()
            .unwrap(),
    );
    assert_eq!(explain["data"], dry["data"]);
    let data = &dry["data"];
    assert_eq!(data["roots"], json!(["inspect"]));
    assert_eq!(data["terminals"], json!(["test"]));
    assert_eq!(data["workflow"]["inputs"]["profile"], "release");
    assert_eq!(data["task_sources"]["inspect"]["workflow"], "verify");
    assert_eq!(data["plan"]["tasks"][1]["depends_on"], json!(["inspect"]));
    assert_eq!(
        data["plan"]["tasks"][1]["inputs"][0]["from_task"],
        "inspect"
    );
    assert_eq!(before, inspection_snapshot(harness.directory.path()));
    assert!(!harness.log.exists());
    let state_path = harness.state("inspection");
    let real =
        workflow_document(&harness.workflow_run_json("verify", &state_path, &["profile=release"]));
    assert_eq!(real["data"]["run"]["status"], "completed");
    assert_eq!(data["workflow"], read_state(&state_path)["workflow"]);
}

#[test]
fn workflow_explain_and_dry_run_allow_root_named_instance() {
    let harness = Harness::new();
    harness.workflow_file(
        "child",
        &json!({
            "schema_version": 1,
            "name": "child",
            "tasks": [{"id": "leaf", "program": "/fake/leaf"}]
        }),
    );
    harness.workflow_file(
        "release",
        &json!({
            "schema_version": 2,
            "name": "release",
            "tasks": [
                {"id": "root", "workflow": "child"},
                {"id": "other", "workflow": "child"}
            ]
        }),
    );

    let explain = workflow_document(
        &harness
            .command()
            .args([
                "--json", "--scope", "current", "workflow", "explain", "release",
            ])
            .output()
            .unwrap(),
    );
    let dry_run = workflow_document(
        &harness
            .command()
            .args([
                "--json",
                "--scope",
                "current",
                "workflow",
                "run",
                "release",
                "--dry-run",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(explain["data"], dry_run["data"]);

    let data = &dry_run["data"];
    assert_eq!(
        data["graph"]["task_ids"],
        json!(["root.leaf", "other.leaf"])
    );
    assert_eq!(
        data["plan"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["root.leaf", "other.leaf"]
    );

    let tree = &data["composition_tree"]["root"];
    assert_eq!(tree["workflow"], "release");
    let children = tree["children"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(
        children
            .iter()
            .map(|child| child["instance"].clone())
            .collect::<Vec<_>>(),
        vec![json!("root"), json!("other")]
    );
    assert_eq!(
        children
            .iter()
            .filter(|child| child["instance"] == "root")
            .count(),
        1
    );
    assert!(children.iter().all(|child| child["children"] == json!([])));
    assert!(!harness.log.exists(), "inspection launched a supervisor");
}

#[test]
fn workflow_explain_nested_root_named_instances_remain_structural() {
    let harness = Harness::new();
    harness.workflow_file(
        "child",
        &json!({
            "schema_version": 1,
            "name": "child",
            "tasks": [{"id": "leaf", "program": "/fake/leaf"}]
        }),
    );
    harness.workflow_file(
        "middle",
        &json!({
            "schema_version": 2,
            "name": "middle",
            "tasks": [{"id": "inner", "workflow": "child"}]
        }),
    );
    harness.workflow_file(
        "release",
        &json!({
            "schema_version": 2,
            "name": "release",
            "tasks": [{"id": "root", "workflow": "middle"}]
        }),
    );

    let data = &workflow_document(
        &harness
            .command()
            .args([
                "--json",
                "--scope",
                "current",
                "workflow",
                "run",
                "release",
                "--dry-run",
            ])
            .output()
            .unwrap(),
    )["data"];
    let tree = &data["composition_tree"]["root"];
    assert_eq!(tree["workflow"], "release");
    let root_instance = &tree["children"][0];
    assert_eq!(root_instance["instance"], "root");
    assert_eq!(root_instance["workflow"], "middle");
    let inner_instance = &root_instance["children"][0];
    assert_eq!(inner_instance["instance"], "root.inner");
    assert_eq!(inner_instance["workflow"], "child");
    assert_eq!(inner_instance["children"], json!([]));
    assert_eq!(data["graph"]["task_ids"], json!(["root.inner.leaf"]));
    assert!(!harness.log.exists(), "inspection launched a supervisor");
}

#[test]
fn direct_run_uses_explicit_path_for_plan_named_explain() {
    let harness = Harness::new();
    let plan = harness.directory.path().join("explain");
    fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "tasks": [task("work", &[])]
        }))
        .unwrap(),
    )
    .unwrap();
    let state = harness.state("explain-plan");
    let output = harness
        .command()
        .args([
            "--json",
            "run",
            "--supervisor",
            harness.supervisor.to_str().unwrap(),
            "--state-path",
            state.to_str().unwrap(),
            "./explain",
        ])
        .output()
        .unwrap();
    let data = run_document(&output);
    assert_eq!(data["data"]["run"]["status"], "completed");
    assert_eq!(harness.requests().len(), 1);
}

#[test]
fn workflow_explain_nested_repeated_composition_retains_exports_and_sources() {
    let harness = Harness::new();
    harness.workflow_file("leaf", &json!({"schema_version":2,"name":"leaf","tasks":[
        {"id":"a","program":"/fake/a"},
        {"id":"b","program":"/fake/b"},
        {"id":"c","program":"/fake/c","depends_on":["a","b"],"outputs":[{"name":"result","type":"json","path":"result.json"}]}
    ],"exports":{"result":{"from_task":"c","output":"result"}}}));
    harness.workflow_file("middle", &json!({"schema_version":2,"name":"middle","tasks":[{"id":"inner","workflow":"leaf"}],"exports":{"forward":{"from_task":"inner","output":"result"}}}));
    harness.workflow_file("root", &json!({"schema_version":2,"name":"root","tasks":[{"id":"left","workflow":"middle"},{"id":"right","workflow":"middle","depends_on":["left"]}],"exports":{"final":{"from_task":"right","output":"forward"}}}));
    let output = workflow_document(
        &harness
            .command()
            .args([
                "--json",
                "workflow",
                "run",
                "root",
                "--dry-run",
                "--jobs",
                "4",
            ])
            .output()
            .unwrap(),
    );
    let data = &output["data"];
    assert_eq!(data["roots"], json!(["left.inner.a", "left.inner.b"]));
    assert_eq!(data["terminals"], json!(["right.inner.c"]));
    assert_eq!(
        data["workflow"]["composition"]["instances"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let tree = &data["composition_tree"]["root"];
    assert_eq!(tree["instance"], "root");
    assert_eq!(tree["workflow"], "root");
    let children = tree["children"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(
        children
            .iter()
            .find(|child| child["instance"] == "left")
            .unwrap()["children"][0]["instance"],
        "left.inner"
    );
    assert_eq!(
        data["task_sources"]["right.inner.c"],
        json!({"workflow":"leaf","instance":"right.inner","task":"c", "definition_sha256": data["workflow"]["composition"]["instances"][3]["definition_sha256"]})
    );
    assert_eq!(
        data["plan"]["tasks"][3]["depends_on"],
        json!(["left.inner.c"])
    );
    assert_eq!(data["concurrency"]["dag_width"], 2);
    assert_eq!(data["concurrency"]["maximum_parallel_tasks"], 2);
    assert_eq!(data["bounds"]["depth"]["used"], 2);
    assert_eq!(data["bounds"]["tasks"]["used"], 6);
    let export = data["exports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "final")
        .unwrap();
    assert_eq!(export["producer_task"], "right.inner.c");
    assert_eq!(export["producer_output"], "result");
    let human = harness
        .command()
        .args(["workflow", "explain", "root"])
        .output()
        .unwrap();
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("right.inner -> leaf"));
    assert!(text.contains("right.inner.c"));
    assert!(!harness.log.exists());
    let state = harness.state("composed-inspection");
    workflow_document(&harness.workflow_run_json("root", &state, &[]));
    assert_eq!(data["workflow"], read_state(&state)["workflow"]);
}

#[test]
fn workflow_explain_concurrency_uses_antichain_not_largest_layer() {
    let harness = Harness::new();
    harness.workflow_file("width", &json!({"schema_version":1,"name":"width","tasks":[
        {"id":"a","program":"/fake/a"},{"id":"b","program":"/fake/b"},
        {"id":"c","program":"/fake/c","depends_on":["b"]},{"id":"d","program":"/fake/d","depends_on":["b"]}
    ]}));
    let data = workflow_document(
        &harness
            .command()
            .args([
                "--json",
                "workflow",
                "run",
                "width",
                "--dry-run",
                "--jobs",
                "3",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(data["data"]["concurrency"]["dag_width"], 3);
    assert_eq!(data["data"]["concurrency"]["maximum_parallel_tasks"], 3);
}

#[test]
fn invalid_dry_run_rejects_before_any_execution_or_mutation() {
    let harness = Harness::new();
    let mut definition = reusable_verify_definition();
    definition["tasks"][1]["depends_on"] = json!(["missing"]);
    harness.workflow_file("verify", &definition);
    let before = inspection_snapshot(harness.directory.path());
    let output = harness
        .command()
        .args(["--json", "workflow", "run", "verify", "--dry-run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert_eq!(before, inspection_snapshot(harness.directory.path()));
    assert!(!harness.log.exists());
    let output = harness
        .command()
        .args([
            "workflow",
            "run",
            "verify",
            "--dry-run",
            "--resume",
            "/tmp/unused",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(before, inspection_snapshot(harness.directory.path()));
}

#[test]
fn workflow_explain_redacts_inputs_arguments_and_terminal_controls() {
    let harness = Harness::new();
    harness.workflow_file("secret", &json!({"schema_version":1,"name":"secret","inputs":{"token":{"type":"string","default":"never-print-this-secret"}},"tasks":[{"id":"task","program":"/fake/task","arguments":[{"workflow_input":"token"},"terminal\u{1b}[31m\nspoof"],"redact_argument_indexes":[0]}]}));
    for args in [
        vec!["--json", "workflow", "explain", "secret"],
        vec!["workflow", "run", "secret", "--dry-run"],
    ] {
        let output = harness.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("never-print-this-secret"));
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("<redacted>"));
    }
}

#[test]
fn real_go_inspection_dry_run_matches_execution_and_survives_deleted_definitions() {
    let Some(supervisor) = real_supervisor_path() else {
        return;
    };
    let harness = Harness::new();
    harness.workflow_file("child", &json!({"schema_version":2,"name":"child","tasks":[{"id":"produce","program":"/usr/bin/python3","arguments":["-c","import json,os; p=json.loads(os.environ['RELAY_OUTPUT_MANIFEST'])['result']; open(p,'w').write('{\"ok\":true}')"],"outputs":[{"name":"result","type":"json","path":"result.json"}]}],"exports":{"result":{"from_task":"produce","output":"result"}}}));
    harness.workflow_file("root", &json!({"schema_version":2,"name":"root","tasks":[{"id":"child-instance","workflow":"child"}]}));
    let before = inspection_snapshot(harness.directory.path());
    let dry = workflow_document(
        &harness
            .command()
            .args(["--json", "workflow", "run", "root", "--dry-run"])
            .output()
            .unwrap(),
    );
    assert_eq!(before, inspection_snapshot(harness.directory.path()));
    let state = harness.state("real-inspection");
    let real = workflow_document(
        &harness
            .command()
            .args(["--json", "workflow", "run", "root", "--supervisor"])
            .arg(supervisor)
            .arg("--state-path")
            .arg(&state)
            .output()
            .unwrap(),
    );
    assert_eq!(real["data"]["run"]["status"], "completed");
    assert_eq!(dry["data"]["workflow"], read_state(&state)["workflow"]);
    let inspect = || {
        harness
            .command()
            .args(["--json", "run", "explain"])
            .arg(&state)
            .output()
            .unwrap()
    };
    let original = inspect();
    assert!(
        original.status.success(),
        "{}",
        String::from_utf8_lossy(&original.stderr)
    );
    fs::write(
        harness.directory.path().join("relay-workflows/root.json"),
        b"not JSON",
    )
    .unwrap();
    fs::write(
        harness.directory.path().join("relay-workflows/child.json"),
        b"changed",
    )
    .unwrap();
    assert_eq!(original.stdout, inspect().stdout);
    fs::remove_dir_all(harness.directory.path().join("relay-workflows")).unwrap();
    assert_eq!(original.stdout, inspect().stdout);
    let output: Value = serde_json::from_slice(&original.stdout).unwrap();
    let text = output.to_string();
    assert!(text.contains("child-instance.produce"));
    let state_value = read_state(&state);
    let producer = &record(&state_value, "child-instance.produce")["outputs"][0];
    assert!(text.contains(producer["attempt_run_id"].as_str().unwrap()));
}

#[test]
fn workflow_dry_run_preserves_symlink_rejection_and_terminal_safe_errors() {
    let harness = Harness::new();
    let outside = tempfile::tempdir().unwrap();
    let definition = outside.path().join("source.json");
    fs::write(
        &definition,
        serde_json::to_vec(&reusable_verify_definition()).unwrap(),
    )
    .unwrap();
    let directory = harness.directory.path().join("relay-workflows");
    fs::create_dir(&directory).unwrap();
    symlink(&definition, directory.join("unsafe\u{1b}[31m.json")).unwrap();
    let output = harness
        .command()
        .args(["workflow", "run", "verify", "--dry-run"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("symlink"));
    assert!(!error.contains('\u{1b}'));
    assert!(!harness.log.exists());
    assert!(!harness.directory.path().join(".relay").exists());
}

#[test]
fn workflow_explain_redacts_composed_input_bindings() {
    let harness = Harness::new();
    harness.workflow_file("child", &json!({"schema_version":2,"name":"child","inputs":{"token":{"type":"string","required":true}},"tasks":[{"id":"task","program":"/fake/task","arguments":[{"workflow_input":"token"}],"redact_argument_indexes":[0]}]}));
    harness.workflow_file("root", &json!({"schema_version":2,"name":"root","inputs":{"token":{"type":"string","default":"composed-secret-value"}},"tasks":[{"id":"child","workflow":"child","inputs":{"token":{"workflow_input":"token"}}}]}));
    let output = harness
        .command()
        .args(["--json", "workflow", "explain", "root"])
        .output()
        .unwrap();
    let data = workflow_document(&output);
    assert_eq!(data["data"]["workflow"]["inputs"]["token"], "<redacted>");
    assert_eq!(
        data["data"]["workflow"]["composition"]["instances"][0]["resolved_inputs"]["token"],
        "<redacted>"
    );
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("composed-secret-value")
    );
}

fn run_explanation_document(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "run explanation unexpectedly wrote stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "run explanation failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("run explanation JSON");
    assert_eq!(document["output_schema_version"], 1);
    assert_eq!(document["ok"], true);
    assert_eq!(document["command"], "run");
    document
}

#[test]
fn run_explain_reports_retry_and_persisted_output_provenance() {
    let harness = Harness::new();
    harness.configure_sequences(&[("produce", &["nonzero", "success"])]);
    let mut producer = retry_task("produce", &[], 2, 0);
    producer["outputs"] = json!([{"name":"summary","type":"json","path":"summary.json"}]);
    let plan = harness.plan_v3("explain-success", json!([producer]));
    let state = harness
        .directory
        .path()
        .join(".relay/runs/explain-success.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    run_document(&harness.run_json(&plan, &state));

    let persisted = read_state(&state);
    let persisted_output = PathBuf::from(
        record(&persisted, "produce")["outputs"][0]["path"]
            .as_str()
            .unwrap(),
    );
    fs::write(&persisted_output, b"changed-after-persistence").unwrap();
    let output = harness
        .command()
        .args(["--json", "run", "explain", state.to_str().unwrap()])
        .output()
        .unwrap();
    let document = run_explanation_document(&output);
    let task = &document["data"]["tasks"][0];
    assert_eq!(document["data"]["run"]["status"], "completed");
    assert_eq!(
        document["data"]["run"]["aggregate_run_id"],
        persisted["run_id"]
    );
    assert_eq!(task["retry"]["status"], "succeeded_after_retry");
    assert_eq!(task["attempts_used"], 2);
    assert_eq!(
        task["attempts"][0]["attempt_run_id"],
        attempts(&persisted, "produce")[0]["run_id"]
    );
    assert_eq!(task["trusted_outputs"][0]["producer_task"], "produce");
    assert_eq!(
        task["trusted_outputs"][0]["aggregate_run_id"],
        persisted["run_id"]
    );
    assert_eq!(
        task["trusted_outputs"][0]["attempt_run_id"],
        attempts(&persisted, "produce")[1]["run_id"]
    );
    assert_eq!(
        task["trusted_outputs"][0]["sha256"].as_str().unwrap().len(),
        64
    );
    let by_id = harness
        .command()
        .args([
            "--json",
            "run",
            "explain",
            persisted["run_id"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(by_id.stdout, output.stdout);
}

#[test]
fn run_explain_reports_failed_and_blocked_tasks() {
    let harness = Harness::new();
    harness.configure_sequences(&[("failed", &["nonzero", "nonzero"])]);
    let plan = harness.plan_v3(
        "explain-failure",
        json!([
            retry_task("failed", &[], 2, 0),
            task("blocked", &["failed"])
        ]),
    );
    let state = harness
        .directory
        .path()
        .join(".relay/runs/explain-failure.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let run = run_document(&harness.run_json(&plan, &state));
    assert_eq!(run["data"]["run"]["status"], "failed");

    let document = run_explanation_document(
        &harness
            .command()
            .args(["--json", "run", "explain", state.to_str().unwrap()])
            .output()
            .unwrap(),
    );
    let tasks = document["data"]["tasks"].as_array().unwrap();
    let failed = tasks.iter().find(|task| task["id"] == "failed").unwrap();
    let blocked = tasks.iter().find(|task| task["id"] == "blocked").unwrap();
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["latest_reason"], "exit code 17");
    assert_eq!(failed["attempts_used"], 2);
    assert_eq!(failed["retry"]["status"], "exhausted");
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["blocking_dependencies"][0]["task"], "failed");
    assert_eq!(blocked["blocking_dependencies"][0]["status"], "failed");
}

#[test]
fn run_explain_reports_indeterminate_reconciliation_without_revalidating_outputs() {
    let harness = Harness::new();
    harness.configure(&[("broken", "malformed_output")]);
    let mut broken = task("broken", &[]);
    broken["outputs"] = json!([{"name":"summary","type":"json","path":"summary.json"}]);
    let plan = harness.plan_v3("explain-indeterminate", json!([broken]));
    let state = harness
        .directory
        .path()
        .join(".relay/runs/explain-indeterminate.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let run = run_document(&harness.run_json(&plan, &state));
    assert_eq!(run["data"]["run"]["status"], "indeterminate");
    let output = harness
        .command()
        .args(["--json", "run", "explain", state.to_str().unwrap()])
        .output()
        .unwrap();
    let document = run_explanation_document(&output);
    assert_eq!(
        document["data"]["reconciliation"]["status"],
        "indeterminate"
    );
    assert_eq!(
        document["data"]["tasks"][0]["reconciliation"],
        "indeterminate_no_automatic_retry"
    );
    assert_eq!(
        document["data"]["tasks"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn run_explain_redacts_persisted_workflow_inputs() {
    let harness = Harness::new();
    harness.workflow_file(
        "secret-run",
        &json!({
            "schema_version":1,
            "name":"secret-run",
            "inputs":{"token":{"type":"string","default":"persisted-secret-value"}},
            "tasks":[{"id":"task","program":"/fake/task","arguments":[{"workflow_input":"token"}],"redact_argument_indexes":[0]}]
        }),
    );
    let state = harness.directory.path().join(".relay/runs/secret-run.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let run = harness.workflow_run_json("secret-run", &state, &[]);
    assert!(run.status.success(), "workflow run failed: {:?}", run);
    fs::remove_dir_all(harness.directory.path().join("relay-workflows")).unwrap();
    let output = harness
        .command()
        .args(["--json", "run", "explain", state.to_str().unwrap()])
        .output()
        .unwrap();
    let document = run_explanation_document(&output);
    assert_eq!(
        document["data"]["run"]["workflow"]["inputs"]["token"],
        "<redacted>"
    );
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("persisted-secret-value")
    );
}
