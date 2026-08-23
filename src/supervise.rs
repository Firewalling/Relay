use crate::{app::RunOutput, cli::SuperviseArgs};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use std::{
    env,
    ffi::OsStr,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::{fs::MetadataExt, process::CommandExt};

const OUTPUT_SCHEMA_VERSION: u32 = 1;
const EXECUTION_PROTOCOL_VERSION: u32 = 1;
const MAX_SUPERVISOR_REQUEST_BYTES: usize = 1 << 20;
const MAX_SUPERVISOR_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_LEGACY_STATE_PROOF_BYTES: u64 = 140 << 20;
const MAX_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_GRACE_MS: i64 = 60_000;
const MAX_OUTPUT_BYTES: i64 = 8 * 1024 * 1024;
const MAX_LOG_BYTES: i64 = 4 * 1024 * 1024;
const MAX_REDACTIONS: usize = 32;
const MAX_REDACTED_BYTES: usize = 64 * 1024;
const SUPERVISOR_READY_ENV: &str = "RELAY_SUPERVISOR_READY_FD";
const SUPERVISOR_READY_BYTE: u8 = b'R';
const SUPERVISOR_ACK_BYTE: u8 = b'A';
const SUPERVISOR_READY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error(
        "supervisor executable was not found; pass --supervisor, set RELAY_SUPERVISOR, or place relay-supervisor beside relay"
    )]
    NotFound,
    #[error("invalid supervise request: {0}")]
    InvalidRequest(String),
    #[error("could not start supervisor {path}: {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not communicate with supervisor: {0}")]
    Transport(#[source] io::Error),
    #[error("supervisor response exceeded the 32 MiB limit on {stream}")]
    ResponseTooLarge { stream: &'static str },
    #[error("supervisor produced an invalid response: {0}")]
    InvalidResponse(String),
    #[error("supervisor error {code}: {message}")]
    Remote { code: String, message: String },
}

impl SupervisorError {
    pub fn code(&self) -> &str {
        match self {
            Self::NotFound => "supervisor_not_found",
            Self::InvalidRequest(_) => "supervisor_invalid_request",
            Self::Spawn { .. } => "supervisor_spawn",
            Self::Transport(_) => "supervisor_transport",
            Self::ResponseTooLarge { .. } => "supervisor_response_too_large",
            Self::InvalidResponse(_) => "supervisor_invalid_response",
            Self::Remote { code, .. } => code,
        }
    }
}

#[derive(Debug, Serialize)]
struct Request {
    protocol_version: u32,
    run_id: String,
    program: String,
    arguments: Vec<String>,
    redact_argument_indexes: Vec<usize>,
    working_directory: String,
    environment: Vec<EnvironmentVariable>,
    timeout_ms: i64,
    termination_grace_ms: i64,
    stdout_limit_bytes: i64,
    stderr_limit_bytes: i64,
    log_limit_bytes: i64,
    state_path: String,
}

#[derive(Debug, Serialize)]
struct EnvironmentVariable {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuccessEnvelope {
    output_schema_version: u32,
    ok: bool,
    command: String,
    data: ExecutionResult,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Accepted,
    Starting,
    Running,
    Completed,
    FailedToStart,
    TimedOut,
    Cancelled,
    Killed,
    ProtocolFailure,
}

impl Lifecycle {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::FailedToStart => "failed_to_start",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Killed => "killed",
            Self::ProtocolFailure => "protocol_failure",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    ExitZero,
    ExitNonzero,
    StartError,
    TimeoutTerminated,
    CancelledTerminated,
    TimeoutKilled,
    CancelledKilled,
    DescendantsTerminated,
    DescendantsKilled,
    ProtocolFailure,
}

impl TerminationReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExitZero => "exit_zero",
            Self::ExitNonzero => "exit_nonzero",
            Self::StartError => "start_error",
            Self::TimeoutTerminated => "timeout_terminated",
            Self::CancelledTerminated => "cancelled_terminated",
            Self::TimeoutKilled => "timeout_killed",
            Self::CancelledKilled => "cancelled_killed",
            Self::DescendantsTerminated => "descendants_terminated",
            Self::DescendantsKilled => "descendants_killed",
            Self::ProtocolFailure => "protocol_failure",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentEntryMetadata {
    pub name: String,
    pub source: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentMetadata {
    pub inherited_allowlist: Vec<String>,
    pub effective: Vec<EnvironmentEntryMetadata>,
    pub blocked_inherited_count: i64,
    pub duplicate_user_overrides: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleEvent {
    pub sequence: u64,
    pub lifecycle: Lifecycle,
    pub timestamp: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputObservation {
    pub sequence: u64,
    pub stream: String,
    pub bytes: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationLog {
    pub limit_bytes: i64,
    pub bytes_used: i64,
    pub truncated: bool,
    #[serde(deserialize_with = "deserialize_null_as_empty")]
    pub entries: Vec<OutputObservation>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamResult {
    pub text: String,
    pub bytes_observed: i64,
    pub bytes_retained: i64,
    pub truncated: bool,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub first_sequence: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub last_sequence: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeoutDetails {
    pub configured_ms: i64,
    pub triggered: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationDetails {
    pub requested: bool,
    pub source: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminationDetails {
    pub grace_ms: i64,
    pub graceful_attempted: bool,
    pub escalated: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionFailure {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionResult {
    pub run_id: String,
    pub protocol_version: u32,
    pub supervisor_version: String,
    pub program: String,
    #[serde(deserialize_with = "deserialize_null_as_empty")]
    pub arguments: Vec<String>,
    pub environment: EnvironmentMetadata,
    pub accepted_at: String,
    pub started_at: String,
    pub completed_at: String,
    pub duration_ms: i64,
    pub lifecycle: Lifecycle,
    pub termination_reason: TerminationReason,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub exit_code: Option<i64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub failure: Option<ExecutionFailure>,
    pub timeout: TimeoutDetails,
    pub cancellation: CancellationDetails,
    pub termination: TerminationDetails,
    pub stdout: StreamResult,
    pub stderr: StreamResult,
    pub lifecycle_history: Vec<LifecycleEvent>,
    pub output_observations: ObservationLog,
}

fn deserialize_null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl ExecutionResult {
    pub fn task_succeeded(&self) -> bool {
        self.lifecycle == Lifecycle::Completed
            && self.termination_reason == TerminationReason::ExitZero
            && self.exit_code == Some(0)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorEnvelope {
    output_schema_version: u32,
    ok: bool,
    command: String,
    error: RemoteError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteError {
    code: String,
    message: String,
}

#[derive(Debug)]
pub struct SupervisedExecution {
    pub state_path: String,
    pub execution: ExecutionResult,
}

pub fn run(args: SuperviseArgs, invocation_directory: &Path) -> Result<RunOutput, SupervisorError> {
    let supervised = execute(args, invocation_directory)?;
    Ok(RunOutput {
        human: human_output(&supervised.state_path, &supervised.execution),
        data: json!({
            "state_path": supervised.state_path,
            "execution": supervised.execution,
        }),
    })
}

pub fn preflight(args: &SuperviseArgs, invocation_directory: &Path) -> Result<(), SupervisorError> {
    let request = build_request(args, invocation_directory)?;
    encode_request(&request).map(|_| ())
}

pub fn execute(
    args: SuperviseArgs,
    invocation_directory: &Path,
) -> Result<SupervisedExecution, SupervisorError> {
    execute_inner(args, invocation_directory, None)
}

pub fn execute_coordinated(
    args: SuperviseArgs,
    invocation_directory: &Path,
    coordinator: &CoordinatorSignals,
) -> Result<SupervisedExecution, SupervisorError> {
    execute_inner(args, invocation_directory, Some(coordinator))
}

fn encode_request(request: &Request) -> Result<Vec<u8>, SupervisorError> {
    let request_bytes = serde_json::to_vec(request).map_err(|error| {
        SupervisorError::InvalidRequest(format!("could not encode request: {error}"))
    })?;
    if request_bytes.len().saturating_add(1) > MAX_SUPERVISOR_REQUEST_BYTES {
        return Err(SupervisorError::InvalidRequest(format!(
            "encoded execution request exceeds {MAX_SUPERVISOR_REQUEST_BYTES} bytes"
        )));
    }
    Ok(request_bytes)
}

fn execute_inner(
    args: SuperviseArgs,
    invocation_directory: &Path,
    coordinator: Option<&CoordinatorSignals>,
) -> Result<SupervisedExecution, SupervisorError> {
    let request = build_request(&args, invocation_directory)?;
    let request_bytes = encode_request(&request)?;
    let supervisor = discover_supervisor(args.supervisor.as_deref())?;
    let blocked_signals = if coordinator.is_none() {
        CANCELLATION_SIGNAL.store(0, std::sync::atomic::Ordering::Release);
        Some(BlockedSignals::new()?)
    } else {
        None
    };
    let (ready_parent, ready_child) = supervisor_ready_socketpair()?;
    let mut command = Command::new(&supervisor);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_supervisor_child(&mut command, ready_child.as_raw_fd())?;
    let state_path = Path::new(&request.state_path);
    let initial_state = snapshot_state_path(state_path)?;
    let mut child = command.spawn().map_err(|source| SupervisorError::Spawn {
        path: supervisor,
        source,
    })?;
    drop(ready_child);

    // Start request delivery before readiness for request-first legacy
    // supervisors, but do not block the readiness barrier on pipe capacity.
    // Current supervisors leave the request buffered until Relay acknowledges
    // their installed signal handling.
    let mut stdin = child.stdin.take().expect("piped stdin is present");
    let request_writer = thread::spawn(move || {
        let write_error = stdin
            .write_all(&request_bytes)
            .and_then(|_| stdin.write_all(b"\n"))
            .err();
        drop(stdin);
        write_error
    });

    let readiness = match test_pause_after_supervisor_spawn().and_then(|_| {
        wait_for_supervisor_ready(
            &mut child,
            ready_parent,
            state_path,
            initial_state,
            &request.run_id,
        )
    }) {
        Ok(readiness) => readiness,
        Err(error) => {
            kill_and_reap(&mut child);
            join_failed_request_writer(request_writer)?;
            return Err(error);
        }
    };
    let signals = if let Some(coordinator) = coordinator {
        SignalRegistration::Coordinator(coordinator.activate(child.id() as i32))
    } else {
        match SignalForwarder::install(
            blocked_signals.expect("standalone execution blocks signals"),
            child.id() as i32,
        ) {
            Ok(guard) => SignalRegistration::Standalone(Box::new(guard)),
            Err(error) => {
                kill_and_reap(&mut child);
                join_failed_request_writer(request_writer)?;
                return Err(error);
            }
        }
    };
    if let Err(error) = finish_supervisor_readiness(readiness) {
        kill_disarm_and_reap(&mut child, signals);
        join_failed_request_writer(request_writer)?;
        return Err(error);
    }
    let stdout = child.stdout.take().expect("piped stdout is present");
    let stderr = child.stderr.take().expect("piped stderr is present");
    let stdout_reader = thread::spawn(move || read_bounded(stdout, "stdout"));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, "stderr"));
    let status = wait_for_supervisor(&mut child, signals);
    let stdout = stdout_reader
        .join()
        .map_err(|_| SupervisorError::Transport(io::Error::other("stdout reader panicked")));
    let stderr = stderr_reader
        .join()
        .map_err(|_| SupervisorError::Transport(io::Error::other("stderr reader panicked")));
    let write_error = request_writer
        .join()
        .map_err(|_| SupervisorError::Transport(io::Error::other("stdin writer panicked")))?;
    let status = status?;
    let stdout = stdout??;
    let stderr = stderr??;
    let result = if status.success() {
        if let Some(error) = write_error {
            return Err(SupervisorError::Transport(error));
        }
        if !stderr.is_empty() {
            return Err(SupervisorError::InvalidResponse(
                "successful supervisor wrote to stderr".into(),
            ));
        }
        let response: SuccessEnvelope = decode_single(&stdout)?;
        if !response.ok {
            return Err(SupervisorError::InvalidResponse(
                "successful supervisor response has ok=false".into(),
            ));
        }
        validate_envelope(
            response.output_schema_version,
            &response.command,
            &request,
            &response.data,
        )?;
        response.data
    } else {
        if !stdout.is_empty() {
            return Err(SupervisorError::InvalidResponse(
                "failed supervisor wrote to stdout".into(),
            ));
        }
        let response: ErrorEnvelope = decode_single(&stderr)?;
        if response.ok {
            return Err(SupervisorError::InvalidResponse(
                "failed supervisor response has ok=true".into(),
            ));
        }
        validate_header(response.output_schema_version, &response.command)?;
        if !valid_error_code(&response.error.code) || response.error.message.is_empty() {
            return Err(SupervisorError::InvalidResponse(
                "supervisor error code is invalid or its message is empty".into(),
            ));
        }
        return Err(SupervisorError::Remote {
            code: response.error.code,
            message: response.error.message,
        });
    };
    Ok(SupervisedExecution {
        state_path: request.state_path,
        execution: result,
    })
}

#[cfg(target_os = "linux")]
fn wait_for_supervisor(
    child: &mut std::process::Child,
    signals: SignalRegistration,
) -> Result<std::process::ExitStatus, SupervisorError> {
    loop {
        let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut information,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        signals.disarm();
        let _ = child.kill();
        let _ = child.wait();
        return Err(SupervisorError::Transport(error));
    }
    // WNOWAIT keeps the exited supervisor as a zombie, so its PID cannot be
    // reused until forwarding is disarmed and Child::wait performs the reap.
    signals.disarm();
    child.wait().map_err(SupervisorError::Transport)
}

#[cfg(not(target_os = "linux"))]
fn wait_for_supervisor(
    child: &mut std::process::Child,
    signals: SignalRegistration,
) -> Result<std::process::ExitStatus, SupervisorError> {
    let status = child.wait().map_err(SupervisorError::Transport);
    signals.disarm();
    status
}

fn discover_supervisor(explicit: Option<&Path>) -> Result<PathBuf, SupervisorError> {
    if let Some(path) = explicit {
        return executable_path(path);
    }
    if let Some(path) = env::var_os("RELAY_SUPERVISOR") {
        return executable_path(Path::new(&path));
    }
    let relay = env::current_exe().map_err(SupervisorError::Transport)?;
    executable_path(&relay.with_file_name("relay-supervisor"))
}

fn executable_path(path: &Path) -> Result<PathBuf, SupervisorError> {
    if !path.is_absolute() {
        return Err(invalid("supervisor executable path must be absolute"));
    }
    if path.is_file() {
        Ok(path.to_owned())
    } else {
        Err(SupervisorError::NotFound)
    }
}

fn build_request(
    args: &SuperviseArgs,
    invocation_directory: &Path,
) -> Result<Request, SupervisorError> {
    let program = args
        .program_and_args
        .first()
        .expect("clap requires program");
    validate_absolute("program", Path::new(program))?;
    validate_string("program", program)?;
    for (index, argument) in args.program_and_args[1..].iter().enumerate() {
        validate_string(&format!("arguments[{index}]"), argument)?;
    }
    let run_id = args
        .run_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if !valid_run_id(&run_id) {
        return Err(invalid(
            "run_id must match [A-Za-z0-9][A-Za-z0-9._-]{0,127}",
        ));
    }
    let working_directory = args
        .working_directory
        .as_deref()
        .unwrap_or(invocation_directory);
    validate_absolute("working_directory", working_directory)?;
    let state_path = args.state_path.clone().unwrap_or_else(|| {
        env::temp_dir()
            .join("relay-runs")
            .join(format!("{run_id}.json"))
    });
    validate_absolute("state_path", &state_path)?;
    if state_path
        .components()
        .any(|part| part == Component::Normal(OsStr::new(".relay")))
    {
        return Err(invalid(
            "state_path must not be inside the Rust-owned .relay directory",
        ));
    }
    let working_directory = working_directory
        .to_str()
        .ok_or_else(|| invalid("working_directory must be valid UTF-8"))?;
    let state_path = state_path
        .to_str()
        .ok_or_else(|| invalid("state_path must be valid UTF-8"))?;
    let redacted_bytes =
        args.redact_argument_indexes
            .iter()
            .try_fold(0usize, |total, &index| {
                args.program_and_args[1..]
                    .get(index)
                    .map(|arg| total + arg.len())
                    .ok_or_else(|| {
                        invalid(format!(
                            "redact_argument_indexes contains out-of-range index {index}"
                        ))
                    })
            })?;
    if args.redact_argument_indexes.len() > MAX_REDACTIONS {
        return Err(invalid(format!(
            "redact_argument_indexes must contain at most {MAX_REDACTIONS} entries"
        )));
    }
    let mut sorted = args.redact_argument_indexes.clone();
    sorted.sort_unstable();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("redact_argument_indexes contains duplicate index"));
    }
    if redacted_bytes > MAX_REDACTED_BYTES {
        return Err(invalid(format!(
            "redacted argument values must total at most {MAX_REDACTED_BYTES} bytes"
        )));
    }
    if !(0..=MAX_TIMEOUT_MS).contains(&args.timeout_ms) {
        return Err(invalid(format!(
            "timeout_ms must be between 0 and {MAX_TIMEOUT_MS}"
        )));
    }
    if !(1..=MAX_GRACE_MS).contains(&args.termination_grace_ms) {
        return Err(invalid(format!(
            "termination_grace_ms must be between 1 and {MAX_GRACE_MS}"
        )));
    }
    for (name, value, maximum) in [
        (
            "stdout_limit_bytes",
            args.stdout_limit_bytes,
            MAX_OUTPUT_BYTES,
        ),
        (
            "stderr_limit_bytes",
            args.stderr_limit_bytes,
            MAX_OUTPUT_BYTES,
        ),
        ("log_limit_bytes", args.log_limit_bytes, MAX_LOG_BYTES),
    ] {
        if !(1..=maximum).contains(&value) {
            return Err(invalid(format!("{name} must be between 1 and {maximum}")));
        }
    }
    Ok(Request {
        protocol_version: EXECUTION_PROTOCOL_VERSION,
        run_id,
        program: program.clone(),
        arguments: args.program_and_args[1..].to_vec(),
        redact_argument_indexes: sorted,
        working_directory: working_directory.to_owned(),
        environment: Vec::new(),
        timeout_ms: args.timeout_ms,
        termination_grace_ms: args.termination_grace_ms,
        stdout_limit_bytes: args.stdout_limit_bytes,
        stderr_limit_bytes: args.stderr_limit_bytes,
        log_limit_bytes: args.log_limit_bytes,
        state_path: state_path.to_owned(),
    })
}

fn validate_absolute(name: &str, path: &Path) -> Result<(), SupervisorError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(invalid(format!("{name} must be an absolute path")))
    }
}
fn validate_string(name: &str, value: &str) -> Result<(), SupervisorError> {
    if value.contains('\0') {
        Err(invalid(format!("{name} must not contain a NUL byte")))
    } else {
        Ok(())
    }
}
fn invalid(message: impl Into<String>) -> SupervisorError {
    SupervisorError::InvalidRequest(message.into())
}
fn valid_run_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_error_code(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn read_bounded(mut reader: impl Read, stream: &'static str) -> Result<Vec<u8>, SupervisorError> {
    let mut retained = Vec::new();
    let mut buffer = [0; 8192];
    let mut exceeded = false;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(SupervisorError::Transport)?;
        if count == 0 {
            break;
        }
        let remaining = MAX_SUPERVISOR_RESPONSE_BYTES.saturating_sub(retained.len());
        let kept = remaining.min(count);
        retained.extend_from_slice(&buffer[..kept]);
        exceeded |= kept != count;
    }
    if exceeded {
        Err(SupervisorError::ResponseTooLarge { stream })
    } else {
        Ok(retained)
    }
}

fn decode_single<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, SupervisorError> {
    let mut stream = serde_json::Deserializer::from_slice(bytes).into_iter::<T>();
    let value = stream
        .next()
        .ok_or_else(|| SupervisorError::InvalidResponse("expected one JSON document".into()))?
        .map_err(|error| SupervisorError::InvalidResponse(error.to_string()))?;
    if stream.next().is_some() {
        return Err(SupervisorError::InvalidResponse(
            "expected exactly one JSON document".into(),
        ));
    }
    Ok(value)
}

fn validate_header(schema: u32, command: &str) -> Result<(), SupervisorError> {
    if schema != OUTPUT_SCHEMA_VERSION {
        return Err(SupervisorError::InvalidResponse(format!(
            "unsupported output schema version {schema}"
        )));
    }
    if command != "supervise" {
        return Err(SupervisorError::InvalidResponse(format!(
            "unexpected supervisor command {command:?}"
        )));
    }
    Ok(())
}
fn validate_envelope(
    schema: u32,
    command: &str,
    request: &Request,
    execution: &ExecutionResult,
) -> Result<(), SupervisorError> {
    validate_header(schema, command)?;
    execution.validate(request)
}

impl ExecutionResult {
    fn validate(&self, request: &Request) -> Result<(), SupervisorError> {
        if self.protocol_version != EXECUTION_PROTOCOL_VERSION {
            return response_invalid("unexpected execution protocol version");
        }
        if self.run_id != request.run_id {
            return response_invalid("execution run_id did not match request");
        }
        if self.program != request.program {
            return response_invalid("execution program did not match request");
        }
        if self.arguments.len() != request.arguments.len()
            || self
                .arguments
                .iter()
                .zip(&request.arguments)
                .enumerate()
                .any(|(index, (returned, requested))| {
                    let expected = if request
                        .redact_argument_indexes
                        .binary_search(&index)
                        .is_ok()
                    {
                        "<redacted>"
                    } else {
                        requested
                    };
                    returned != expected
                })
        {
            return response_invalid("execution arguments did not match sanitized request");
        }
        if self.timeout.configured_ms != request.timeout_ms {
            return response_invalid("execution timeout did not match request");
        }
        if self.termination.grace_ms != request.termination_grace_ms {
            return response_invalid("execution termination grace did not match request");
        }
        if self.output_observations.limit_bytes != request.log_limit_bytes {
            return response_invalid("execution observation limit did not match request");
        }
        if self.supervisor_version.is_empty() {
            return response_invalid("execution supervisor_version is missing");
        }
        if self.program.is_empty() {
            return response_invalid("execution program is empty");
        }
        if !matches!(
            (self.lifecycle, self.termination_reason),
            (
                Lifecycle::Completed,
                TerminationReason::ExitZero
                    | TerminationReason::ExitNonzero
                    | TerminationReason::DescendantsTerminated
            ) | (Lifecycle::FailedToStart, TerminationReason::StartError)
                | (Lifecycle::TimedOut, TerminationReason::TimeoutTerminated)
                | (Lifecycle::Cancelled, TerminationReason::CancelledTerminated)
                | (
                    Lifecycle::Killed,
                    TerminationReason::TimeoutKilled
                        | TerminationReason::CancelledKilled
                        | TerminationReason::DescendantsKilled
                )
        ) {
            return response_invalid("inconsistent execution lifecycle and termination_reason");
        }
        if self.duration_ms < 0
            || self.environment.blocked_inherited_count < 0
            || self.environment.duplicate_user_overrides < 0
            || self.timeout.configured_ms < 0
            || self.termination.grace_ms < 0
            || (self.termination_reason != TerminationReason::ExitNonzero
                && self.exit_code.is_some_and(|code| code < 0))
        {
            return response_invalid("execution contains a negative duration, count, or limit");
        }
        for entry in &self.environment.effective {
            if entry.name.is_empty()
                || !matches!(entry.source.as_str(), "inherited" | "user" | "required")
            {
                return response_invalid("execution environment metadata is invalid");
            }
        }
        self.validate_timestamps()?;
        self.validate_outcome()?;
        validate_stream("stdout", &self.stdout, request.stdout_limit_bytes)?;
        validate_stream("stderr", &self.stderr, request.stderr_limit_bytes)?;
        self.validate_sequences_and_observations()
    }

    fn validate_timestamps(&self) -> Result<(), SupervisorError> {
        let accepted = parse_timestamp("accepted_at", &self.accepted_at)?;
        let started = parse_timestamp("started_at", &self.started_at)?;
        let completed = parse_timestamp("completed_at", &self.completed_at)?;
        if accepted > started || started > completed {
            return response_invalid("execution timestamps are not chronological");
        }
        let mut previous = None;
        for event in &self.lifecycle_history {
            let timestamp = parse_timestamp("lifecycle event", &event.timestamp)?;
            if timestamp < accepted
                || timestamp > completed
                || previous.is_some_and(|prior| timestamp <= prior)
            {
                return response_invalid("lifecycle event timestamps are not chronological");
            }
            previous = Some(timestamp);
        }
        Ok(())
    }

    fn validate_outcome(&self) -> Result<(), SupervisorError> {
        match self.termination_reason {
            TerminationReason::ExitZero if self.exit_code != Some(0) => {
                return response_invalid("exit_zero requires exit_code 0");
            }
            TerminationReason::ExitNonzero if self.exit_code == Some(0) => {
                return response_invalid("exit_nonzero cannot have exit_code 0");
            }
            TerminationReason::StartError if self.exit_code.is_some() => {
                return response_invalid("start_error cannot have an exit_code");
            }
            _ => {}
        }
        let requires_failure = matches!(
            self.termination_reason,
            TerminationReason::StartError | TerminationReason::ProtocolFailure
        );
        if self.failure.is_some() != requires_failure {
            return response_invalid("execution failure details are inconsistent with termination");
        }
        if let Some(failure) = &self.failure
            && (!valid_error_code(&failure.code) || failure.message.is_empty())
        {
            return response_invalid("execution failure details are invalid");
        }
        let timed_out = matches!(
            self.termination_reason,
            TerminationReason::TimeoutTerminated | TerminationReason::TimeoutKilled
        );
        if self.timeout.triggered != timed_out || (timed_out && self.timeout.configured_ms == 0) {
            return response_invalid("execution timeout details are inconsistent with termination");
        }
        let cancelled = matches!(
            self.termination_reason,
            TerminationReason::CancelledTerminated | TerminationReason::CancelledKilled
        );
        let cancellation_invalid = if self.termination_reason == TerminationReason::ProtocolFailure
        {
            self.cancellation.source.is_empty()
                || (self.cancellation.requested && self.cancellation.source == "none")
        } else {
            self.cancellation.requested != cancelled
                || self.cancellation.source.is_empty()
                || (cancelled == (self.cancellation.source == "none"))
        };
        if cancellation_invalid {
            return response_invalid(
                "execution cancellation details are inconsistent with termination",
            );
        }
        let killed = matches!(
            self.termination_reason,
            TerminationReason::TimeoutKilled
                | TerminationReason::CancelledKilled
                | TerminationReason::DescendantsKilled
        );
        if self.termination_reason == TerminationReason::ProtocolFailure {
            if self.termination.escalated && !self.termination.graceful_attempted {
                return response_invalid("execution escalated without a graceful attempt");
            }
        } else if self.termination.escalated != killed
            || (self.termination.escalated && !self.termination.graceful_attempted)
        {
            return response_invalid(
                "execution termination details are inconsistent with termination reason",
            );
        }
        let requires_graceful = matches!(
            self.termination_reason,
            TerminationReason::TimeoutTerminated
                | TerminationReason::TimeoutKilled
                | TerminationReason::DescendantsTerminated
                | TerminationReason::DescendantsKilled
                | TerminationReason::CancelledKilled
        ) || (self.termination_reason
            == TerminationReason::CancelledTerminated
            && self
                .lifecycle_history
                .iter()
                .any(|event| event.lifecycle == Lifecycle::Running));
        if !matches!(self.termination_reason, TerminationReason::ProtocolFailure)
            && self.termination.graceful_attempted != requires_graceful
        {
            return response_invalid("execution graceful termination details are inconsistent");
        }
        Ok(())
    }

    fn validate_sequences_and_observations(&self) -> Result<(), SupervisorError> {
        let history = &self.lifecycle_history;
        if history
            .first()
            .map(|event| (event.sequence, event.lifecycle))
            != Some((1, Lifecycle::Accepted))
            || history.last().map(|event| event.lifecycle) != Some(self.lifecycle)
            || history.first().map(|event| event.timestamp.as_str())
                != Some(self.accepted_at.as_str())
            || history.last().map(|event| event.timestamp.as_str())
                != Some(self.completed_at.as_str())
        {
            return response_invalid("lifecycle history has an unexpected first or final event");
        }
        let expected_history: &[Lifecycle] = match self.lifecycle {
            Lifecycle::Completed | Lifecycle::TimedOut | Lifecycle::Killed => &[
                Lifecycle::Accepted,
                Lifecycle::Starting,
                Lifecycle::Running,
                self.lifecycle,
            ],
            Lifecycle::FailedToStart => &[
                Lifecycle::Accepted,
                Lifecycle::Starting,
                Lifecycle::FailedToStart,
            ],
            Lifecycle::Cancelled if history.len() == 2 => {
                &[Lifecycle::Accepted, Lifecycle::Cancelled]
            }
            Lifecycle::Cancelled => &[
                Lifecycle::Accepted,
                Lifecycle::Starting,
                Lifecycle::Running,
                Lifecycle::Cancelled,
            ],
            _ => {
                return response_invalid("execution lifecycle cannot appear in a success envelope");
            }
        };
        if history.len() != expected_history.len()
            || history
                .iter()
                .zip(expected_history)
                .any(|(event, expected)| event.lifecycle != *expected)
        {
            return response_invalid("lifecycle history does not follow a legal state machine");
        }
        if history.len() == 2 {
            if self.started_at != self.accepted_at {
                return response_invalid(
                    "pre-start cancellation started_at does not match accepted_at",
                );
            }
        } else if history[1].timestamp != self.started_at {
            return response_invalid("starting lifecycle timestamp does not match started_at");
        }
        if history
            .windows(2)
            .any(|events| events[0].sequence >= events[1].sequence)
            || history.iter().any(|event| event.sequence == 0)
        {
            return response_invalid("lifecycle event sequences are not strictly increasing");
        }
        let final_sequence = history.last().expect("history was checked").sequence;
        let output_floor = history
            .iter()
            .find(|event| event.lifecycle == Lifecycle::Starting)
            .map_or(1, |event| event.sequence);
        for (name, stream) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            if let (Some(first), Some(last)) = (stream.first_sequence, stream.last_sequence)
                && (first <= output_floor || last >= final_sequence)
            {
                return response_invalid(format!(
                    "execution {name} sequence range falls outside output-capable lifecycle events"
                ));
            }
            if let (Some(first), Some(last)) = (stream.first_sequence, stream.last_sequence)
                && history
                    .iter()
                    .any(|event| event.sequence == first || event.sequence == last)
            {
                return response_invalid(format!(
                    "execution {name} sequence range collides with lifecycle history"
                ));
            }
        }
        let log = &self.output_observations;
        if log.limit_bytes < 0 || log.bytes_used < 0 || log.bytes_used > log.limit_bytes {
            return response_invalid("output observation log limits are invalid");
        }
        if log
            .entries
            .windows(2)
            .any(|entries| entries[0].sequence >= entries[1].sequence)
        {
            return response_invalid("output observation sequences are not strictly increasing");
        }
        let mut stdout_observed = 0i64;
        let mut stderr_observed = 0i64;
        let mut accounted = 0i64;
        for entry in &log.entries {
            if entry.sequence == 0 || entry.bytes <= 0 {
                return response_invalid("output observation entry is invalid");
            }
            if history.iter().any(|event| event.sequence == entry.sequence) {
                return response_invalid("execution observation sequences are not unique");
            }
            let stream = match entry.stream.as_str() {
                "stdout" => {
                    stdout_observed =
                        stdout_observed.checked_add(entry.bytes).ok_or_else(|| {
                            SupervisorError::InvalidResponse(
                                "stdout observation count overflowed".into(),
                            )
                        })?;
                    &self.stdout
                }
                "stderr" => {
                    stderr_observed =
                        stderr_observed.checked_add(entry.bytes).ok_or_else(|| {
                            SupervisorError::InvalidResponse(
                                "stderr observation count overflowed".into(),
                            )
                        })?;
                    &self.stderr
                }
                _ => return response_invalid("unknown output observation stream"),
            };
            if !sequence_contains(stream, entry.sequence) {
                return response_invalid(
                    "output observation falls outside its stream sequence range",
                );
            }
            accounted = accounted
                .checked_add(observation_cost(entry))
                .ok_or_else(|| {
                    SupervisorError::InvalidResponse(
                        "output observation accounting overflowed".into(),
                    )
                })?;
        }
        if accounted != log.bytes_used {
            return response_invalid("output observation bytes_used accounting is invalid");
        }
        if stdout_observed > self.stdout.bytes_observed
            || stderr_observed > self.stderr.bytes_observed
        {
            return response_invalid("output observations exceed observed stream bytes");
        }
        if !log.truncated {
            if stdout_observed != self.stdout.bytes_observed
                || stderr_observed != self.stderr.bytes_observed
            {
                return response_invalid("complete output log does not account for observed bytes");
            }
            validate_complete_stream_sequences("stdout", &self.stdout, &log.entries)?;
            validate_complete_stream_sequences("stderr", &self.stderr, &log.entries)?;
        } else if stdout_observed == self.stdout.bytes_observed
            && stderr_observed == self.stderr.bytes_observed
        {
            return response_invalid("truncated output log contains every observation");
        }
        if log
            .entries
            .last()
            .is_some_and(|entry| entry.sequence >= final_sequence)
        {
            return response_invalid("output observation follows final lifecycle event");
        }
        Ok(())
    }
}

fn parse_timestamp(name: &str, value: &str) -> Result<DateTime<FixedOffset>, SupervisorError> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|_| SupervisorError::InvalidResponse(format!("execution {name} is not RFC3339")))
}

fn validate_stream(
    name: &str,
    stream: &StreamResult,
    retained_limit: i64,
) -> Result<(), SupervisorError> {
    if stream.bytes_observed < 0
        || stream.bytes_retained < 0
        || stream.bytes_retained > stream.bytes_observed
        || stream.bytes_retained > retained_limit
        || stream.truncated != (stream.bytes_retained < stream.bytes_observed)
    {
        return response_invalid(format!("execution {name} byte counts are inconsistent"));
    }
    match (stream.first_sequence, stream.last_sequence) {
        (None, None) if stream.bytes_observed == 0 => Ok(()),
        (Some(first), Some(last)) if stream.bytes_observed > 0 && first > 0 && first <= last => {
            Ok(())
        }
        _ => response_invalid(format!("execution {name} sequence range is inconsistent")),
    }
}

fn sequence_contains(stream: &StreamResult, sequence: u64) -> bool {
    matches!(
        (stream.first_sequence, stream.last_sequence),
        (Some(first), Some(last)) if first <= sequence && sequence <= last
    )
}

fn validate_complete_stream_sequences(
    name: &str,
    stream: &StreamResult,
    entries: &[OutputObservation],
) -> Result<(), SupervisorError> {
    let mut matching = entries
        .iter()
        .filter(|entry| entry.stream == name)
        .map(|entry| entry.sequence);
    let first = matching.next();
    let last = matching.next_back().or(first);
    if first != stream.first_sequence || last != stream.last_sequence {
        return response_invalid(format!(
            "execution {name} sequence range does not match its observations"
        ));
    }
    Ok(())
}

fn observation_cost(entry: &OutputObservation) -> i64 {
    48 + entry.stream.len() as i64
        + decimal_digits(entry.sequence) as i64
        + decimal_digits(entry.bytes as u64) as i64
}

fn decimal_digits(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn response_invalid<T>(message: impl Into<String>) -> Result<T, SupervisorError> {
    Err(SupervisorError::InvalidResponse(message.into()))
}

fn human_output(state_path: &str, execution: &ExecutionResult) -> String {
    let exit = execution
        .exit_code
        .map_or_else(|| "null".into(), |value| value.to_string());
    format!(
        "Supervised execution: {}\nReason: {}\nExit code: {exit}\nDuration: {} ms\nState: {state_path}\nRetained stdout: {} bytes\nRetained stderr: {} bytes",
        execution.lifecycle.as_str(),
        execution.termination_reason.as_str(),
        execution.duration_ms,
        execution.stdout.bytes_retained,
        execution.stderr.bytes_retained
    )
}

#[cfg(debug_assertions)]
fn test_pause_after_supervisor_spawn() -> Result<(), SupervisorError> {
    if env::var_os("RELAY_TEST_SUPERVISOR_PAUSE").as_deref()
        != Some(OsStr::new("after_spawn_before_activation"))
    {
        return Ok(());
    }
    let marker = env::var_os("RELAY_TEST_SUPERVISOR_MARKER")
        .map(PathBuf::from)
        .ok_or_else(|| {
            SupervisorError::Transport(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RELAY_TEST_SUPERVISOR_MARKER is required",
            ))
        })?;
    let release = env::var_os("RELAY_TEST_SUPERVISOR_RELEASE")
        .map(PathBuf::from)
        .ok_or_else(|| {
            SupervisorError::Transport(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RELAY_TEST_SUPERVISOR_RELEASE is required",
            ))
        })?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .map_err(SupervisorError::Transport)?;
    while !release.exists() {
        thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn test_pause_after_supervisor_spawn() -> Result<(), SupervisorError> {
    Ok(())
}

fn supervisor_ready_socketpair() -> Result<(OwnedFd, OwnedFd), SupervisorError> {
    let mut descriptors = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(SupervisorError::Transport(io::Error::last_os_error()));
    }
    // SAFETY: socketpair initialized both descriptors, and ownership is
    // transferred exactly once before any fallible descriptor duplication.
    let parent = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let child = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    Ok((stable_child_fd(parent)?, stable_child_fd(child)?))
}

fn stable_child_fd(descriptor: OwnedFd) -> Result<OwnedFd, SupervisorError> {
    if descriptor.as_raw_fd() >= 3 {
        return Ok(descriptor);
    }
    let duplicated = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated == -1 {
        return Err(SupervisorError::Transport(io::Error::last_os_error()));
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new, owned descriptor. `descriptor`
    // remains guarded and is closed when this function returns.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn configure_supervisor_child(command: &mut Command, ready_fd: i32) -> Result<(), SupervisorError> {
    let mut descriptor_limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut descriptor_limit) } != 0 {
        return Err(SupervisorError::Transport(io::Error::last_os_error()));
    }
    let descriptor_limit = descriptor_limit.rlim_cur.min(i32::MAX as libc::rlim_t) as i32;

    command.env(SUPERVISOR_READY_ENV, ready_fd.to_string());
    unsafe {
        command.pre_exec(move || {
            // Do not let unrelated ambient descriptors cross exec. close_range
            // is one async-signal-safe syscall; fcntl is the async-signal-safe
            // fallback for kernels or sandboxes where it is unavailable.
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            if libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            ) == -1
            {
                for descriptor in 3..descriptor_limit {
                    let flags = libc::fcntl(descriptor, libc::F_GETFD);
                    if flags == -1 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() == Some(libc::EBADF) {
                            continue;
                        }
                        return Err(error);
                    }
                    if libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
            }

            let descriptor_flags = libc::fcntl(ready_fd, libc::F_GETFD);
            if descriptor_flags == -1
                || libc::fcntl(
                    ready_fd,
                    libc::F_SETFD,
                    descriptor_flags & !libc::FD_CLOEXEC,
                ) == -1
            {
                return Err(io::Error::last_os_error());
            }

            // Relay blocks cancellation signals while a supervisor PID is
            // unpublished. The child must not inherit that implementation
            // detail; its own runtime owns signal setup and reports readiness.
            let mut signal_set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut signal_set);
            libc::sigaddset(&mut signal_set, libc::SIGINT);
            libc::sigaddset(&mut signal_set, libc::SIGTERM);
            if libc::sigprocmask(libc::SIG_UNBLOCK, &signal_set, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatePathSnapshot {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

impl StatePathSnapshot {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
}

#[derive(Deserialize)]
struct LegacyStateProof {
    run_state_version: u32,
    supervisor_version: String,
    execution: LegacyExecutionProof,
}

#[derive(Deserialize)]
struct LegacyExecutionProof {
    run_id: String,
    protocol_version: u32,
}

fn snapshot_state_path(state_path: &Path) -> Result<Option<StatePathSnapshot>, SupervisorError> {
    match std::fs::symlink_metadata(state_path) {
        Ok(metadata) => Ok(Some(StatePathSnapshot::from_metadata(&metadata))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SupervisorError::Transport(error)),
    }
}

fn legacy_state_ready(
    state_path: &Path,
    initial_state: Option<StatePathSnapshot>,
    run_id: &str,
) -> Result<bool, SupervisorError> {
    let metadata = match std::fs::symlink_metadata(state_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(SupervisorError::Transport(error)),
    };
    if !metadata.file_type().is_file()
        || metadata.len() > MAX_LEGACY_STATE_PROOF_BYTES
        || initial_state == Some(StatePathSnapshot::from_metadata(&metadata))
    {
        return Ok(false);
    }

    let file = std::fs::File::open(state_path).map_err(SupervisorError::Transport)?;
    let proof: LegacyStateProof =
        match serde_json::from_reader(file.take(MAX_LEGACY_STATE_PROOF_BYTES)) {
            Ok(proof) => proof,
            Err(_) => return Ok(false),
        };
    Ok(proof.run_state_version == 1
        && !proof.supervisor_version.is_empty()
        && proof.execution.protocol_version == EXECUTION_PROTOCOL_VERSION
        && proof.execution.run_id == run_id)
}

enum SupervisorReadiness {
    Current(OwnedFd),
    Legacy(OwnedFd),
}

fn finish_supervisor_readiness(readiness: SupervisorReadiness) -> Result<(), SupervisorError> {
    let socket = match readiness {
        SupervisorReadiness::Current(socket) => socket,
        SupervisorReadiness::Legacy(socket) => {
            // A legacy supervisor never consumes an acknowledgement.
            // Publishing its proven PID is sufficient; closing our endpoint
            // supplies EOF.
            drop(socket);
            return Ok(());
        }
    };
    loop {
        let written = unsafe {
            libc::send(
                socket.as_raw_fd(),
                (&SUPERVISOR_ACK_BYTE as *const u8).cast(),
                1,
                libc::MSG_NOSIGNAL,
            )
        };
        if written == 1 {
            break;
        }
        if written == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(SupervisorError::Transport(error));
        }
        return Err(SupervisorError::Transport(io::Error::new(
            io::ErrorKind::WriteZero,
            "could not acknowledge supervisor readiness",
        )));
    }
    if unsafe { libc::shutdown(socket.as_raw_fd(), libc::SHUT_RDWR) } != 0 {
        return Err(SupervisorError::Transport(io::Error::last_os_error()));
    }
    Ok(())
}

fn wait_for_supervisor_ready(
    child: &mut Child,
    ready_socket: OwnedFd,
    state_path: &Path,
    initial_state: Option<StatePathSnapshot>,
    run_id: &str,
) -> Result<SupervisorReadiness, SupervisorError> {
    let deadline = Instant::now() + SUPERVISOR_READY_TIMEOUT;
    let mut readiness = [0_u8; 2];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SupervisorError::Transport(io::Error::new(
                io::ErrorKind::TimedOut,
                "supervisor readiness timed out",
            )));
        }
        let mut poll_descriptor = libc::pollfd {
            fd: ready_socket.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        let poll_ms = remaining.as_millis().clamp(1, 50) as i32;
        let poll_result = unsafe { libc::poll(&mut poll_descriptor, 1, poll_ms) };
        if poll_result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(SupervisorError::Transport(error));
        }
        if poll_result == 0 {
            // A protocol-v1 supervisor does not know about the readiness
            // socket, but a fresh matching state record proves activation.
            if legacy_state_ready(state_path, initial_state, run_id)? {
                return Ok(SupervisorReadiness::Legacy(ready_socket));
            }
            if child
                .try_wait()
                .map_err(SupervisorError::Transport)?
                .is_some()
            {
                return Err(SupervisorError::Transport(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "supervisor exited before reporting readiness",
                )));
            }
            continue;
        }

        let read_result = unsafe {
            libc::read(
                ready_socket.as_raw_fd(),
                readiness.as_mut_ptr().cast(),
                readiness.len(),
            )
        };
        if read_result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(SupervisorError::Transport(error));
        }
        if read_result == 0 {
            if legacy_state_ready(state_path, initial_state, run_id)? {
                return Ok(SupervisorReadiness::Legacy(ready_socket));
            }
            return Err(SupervisorError::Transport(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "supervisor closed readiness channel without exactly one readiness byte",
            )));
        }
        if read_result != 1 || readiness[0] != SUPERVISOR_READY_BYTE {
            return Err(SupervisorError::Transport(io::Error::new(
                io::ErrorKind::InvalidData,
                "supervisor sent invalid readiness data",
            )));
        }
        return Ok(SupervisorReadiness::Current(ready_socket));
    }
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_disarm_and_reap(child: &mut Child, signals: SignalRegistration) {
    // Keep the failed supervisor as an unreaped child until forwarding is
    // cleared, so its published PID cannot be reused while still active.
    let _ = child.kill();
    signals.disarm();
    let _ = child.wait();
}

fn join_failed_request_writer(
    request_writer: thread::JoinHandle<Option<io::Error>>,
) -> Result<(), SupervisorError> {
    // Expected pipe errors are secondary to the startup failure. A panic is
    // not: surface it after cleanup rather than detaching the writer.
    request_writer
        .join()
        .map(|_| ())
        .map_err(|_| SupervisorError::Transport(io::Error::other("stdin writer panicked")))
}

#[cfg(target_os = "linux")]
struct BlockedSignals {
    previous_mask: libc::sigset_t,
    active: bool,
}
#[cfg(target_os = "linux")]
impl BlockedSignals {
    fn new() -> Result<Self, SupervisorError> {
        unsafe {
            let mut signal_set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut signal_set);
            libc::sigaddset(&mut signal_set, libc::SIGINT);
            libc::sigaddset(&mut signal_set, libc::SIGTERM);
            let mut previous_mask: libc::sigset_t = std::mem::zeroed();
            let code = libc::pthread_sigmask(libc::SIG_BLOCK, &signal_set, &mut previous_mask);
            if code != 0 {
                return Err(SupervisorError::Transport(io::Error::from_raw_os_error(
                    code,
                )));
            }
            Ok(Self {
                previous_mask,
                active: true,
            })
        }
    }

    fn unblock(mut self) -> Result<libc::sigset_t, SupervisorError> {
        let code = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous_mask, std::ptr::null_mut())
        };
        if code != 0 {
            return Err(SupervisorError::Transport(io::Error::from_raw_os_error(
                code,
            )));
        }
        self.active = false;
        Ok(unsafe { std::ptr::read(&self.previous_mask) })
    }
}
#[cfg(target_os = "linux")]
impl Drop for BlockedSignals {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous_mask, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct SignalForwarder {
    previous_int: libc::sigaction,
    previous_term: libc::sigaction,
    previous_mask: libc::sigset_t,
}
#[cfg(target_os = "linux")]
static ACTIVE_SUPERVISOR_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
#[cfg(target_os = "linux")]
static CANCELLATION_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
#[cfg(target_os = "linux")]
extern "C" fn forward_signal(signal: libc::c_int) {
    CANCELLATION_SIGNAL.store(signal, std::sync::atomic::Ordering::Release);
    let pid = ACTIVE_SUPERVISOR_PID.load(std::sync::atomic::Ordering::Acquire);
    if pid > 0 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}
#[cfg(target_os = "linux")]
impl SignalForwarder {
    fn install(blocked: BlockedSignals, pid: i32) -> Result<Self, SupervisorError> {
        // The caller blocks both signals before handler installation. Pending
        // signals are delivered only after the target (or cancellation recorder)
        // is ready.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward_signal as *const () as usize;
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous_int: libc::sigaction = std::mem::zeroed();
            let mut previous_term: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGINT, &action, &mut previous_int) != 0 {
                return Err(SupervisorError::Transport(io::Error::last_os_error()));
            }
            if libc::sigaction(libc::SIGTERM, &action, &mut previous_term) != 0 {
                let error = io::Error::last_os_error();
                libc::sigaction(libc::SIGINT, &previous_int, std::ptr::null_mut());
                return Err(SupervisorError::Transport(error));
            }
            ACTIVE_SUPERVISOR_PID.store(pid, std::sync::atomic::Ordering::Release);
            let previous_mask = match blocked.unblock() {
                Ok(mask) => mask,
                Err(error) => {
                    ACTIVE_SUPERVISOR_PID.store(0, std::sync::atomic::Ordering::Release);
                    libc::sigaction(libc::SIGINT, &previous_int, std::ptr::null_mut());
                    libc::sigaction(libc::SIGTERM, &previous_term, std::ptr::null_mut());
                    return Err(error);
                }
            };
            Ok(Self {
                previous_int,
                previous_term,
                previous_mask,
            })
        }
    }
}
#[cfg(target_os = "linux")]
impl Drop for SignalForwarder {
    fn drop(&mut self) {
        unsafe {
            let mut signal_set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut signal_set);
            libc::sigaddset(&mut signal_set, libc::SIGINT);
            libc::sigaddset(&mut signal_set, libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_BLOCK, &signal_set, std::ptr::null_mut());
            ACTIVE_SUPERVISOR_PID.store(0, std::sync::atomic::Ordering::Release);
            libc::sigaction(libc::SIGINT, &self.previous_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous_mask, std::ptr::null_mut());
        }
    }
}

#[cfg(target_os = "linux")]
pub struct CoordinatorSignals {
    _forwarder: SignalForwarder,
}
#[cfg(target_os = "linux")]
impl CoordinatorSignals {
    pub fn install() -> Result<Self, SupervisorError> {
        let blocked = BlockedSignals::new()?;
        CANCELLATION_SIGNAL.store(0, std::sync::atomic::Ordering::Release);
        Ok(Self {
            _forwarder: SignalForwarder::install(blocked, 0)?,
        })
    }

    pub fn cancellation_requested(&self) -> bool {
        CANCELLATION_SIGNAL.load(std::sync::atomic::Ordering::Acquire) != 0
    }

    fn activate(&self, pid: i32) -> ActiveSupervisor {
        ACTIVE_SUPERVISOR_PID.store(pid, std::sync::atomic::Ordering::Release);
        let signal = CANCELLATION_SIGNAL.load(std::sync::atomic::Ordering::Acquire);
        if signal != 0 {
            unsafe {
                libc::kill(pid, signal);
            }
        }
        ActiveSupervisor { pid }
    }
}

#[cfg(target_os = "linux")]
struct ActiveSupervisor {
    pid: i32,
}
#[cfg(target_os = "linux")]
impl Drop for ActiveSupervisor {
    fn drop(&mut self) {
        let _ = ACTIVE_SUPERVISOR_PID.compare_exchange(
            self.pid,
            0,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
}
#[cfg(target_os = "linux")]
enum SignalRegistration {
    Standalone(Box<SignalForwarder>),
    Coordinator(ActiveSupervisor),
}

#[cfg(not(target_os = "linux"))]
static CANCELLATION_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
#[cfg(not(target_os = "linux"))]
struct BlockedSignals;
#[cfg(not(target_os = "linux"))]
impl BlockedSignals {
    fn new() -> Result<Self, SupervisorError> {
        Ok(Self)
    }
}
#[cfg(not(target_os = "linux"))]
struct SignalForwarder;
#[cfg(not(target_os = "linux"))]
impl SignalForwarder {
    fn install(_: BlockedSignals, _: i32) -> Result<Self, SupervisorError> {
        Ok(Self)
    }
}
#[cfg(not(target_os = "linux"))]
pub struct CoordinatorSignals;
#[cfg(not(target_os = "linux"))]
impl CoordinatorSignals {
    pub fn install() -> Result<Self, SupervisorError> {
        Ok(Self)
    }
    pub fn cancellation_requested(&self) -> bool {
        false
    }
    fn activate(&self, _: i32) -> ActiveSupervisor {
        ActiveSupervisor
    }
}
#[cfg(not(target_os = "linux"))]
struct ActiveSupervisor;
#[cfg(not(target_os = "linux"))]
enum SignalRegistration {
    Standalone(Box<SignalForwarder>),
    Coordinator(ActiveSupervisor),
}
impl SignalRegistration {
    fn disarm(self) {
        match self {
            Self::Standalone(forwarder) => drop(forwarder),
            Self::Coordinator(supervisor) => drop(supervisor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_protocol_run_ids_and_rejects_invalid_ones() {
        assert!(valid_run_id("run_1.2-3"));
        assert!(!valid_run_id(""));
        assert!(!valid_run_id("-first"));
        assert!(!valid_run_id(&"a".repeat(129)));
    }

    #[test]
    fn rejects_multiple_response_documents() {
        let error = decode_single::<SuccessEnvelope>(
            br#"{"output_schema_version":1,"ok":true,"command":"supervise","data":{}} {}"#,
        )
        .unwrap_err();
        assert!(matches!(error, SupervisorError::InvalidResponse(_)));
    }

    #[test]
    fn rejects_state_path_with_lexical_relay_component() {
        let args = SuperviseArgs {
            supervisor: None,
            run_id: Some("valid".into()),
            state_path: Some(PathBuf::from("/tmp/project/.relay/run.json")),
            working_directory: None,
            timeout_ms: 0,
            termination_grace_ms: 2_000,
            stdout_limit_bytes: 1 << 20,
            stderr_limit_bytes: 1 << 20,
            log_limit_bytes: 64 << 10,
            redact_argument_indexes: vec![],
            program_and_args: vec!["/usr/bin/true".into()],
        };
        let error = build_request(&args, Path::new("/tmp")).unwrap_err();
        assert!(
            matches!(error, SupervisorError::InvalidRequest(message) if message.contains(".relay"))
        );
    }

    #[test]
    fn oversized_response_is_drained_before_rejection() {
        struct CountingReader {
            remaining: usize,
            observed: usize,
        }

        impl Read for CountingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = self.remaining.min(buffer.len());
                buffer[..count].fill(b'x');
                self.remaining -= count;
                self.observed += count;
                Ok(count)
            }
        }

        let mut reader = CountingReader {
            remaining: MAX_SUPERVISOR_RESPONSE_BYTES + 1,
            observed: 0,
        };
        let error = read_bounded(&mut reader, "stdout").unwrap_err();
        assert!(matches!(
            error,
            SupervisorError::ResponseTooLarge { stream: "stdout" }
        ));
        assert_eq!(reader.remaining, 0);
        assert_eq!(reader.observed, MAX_SUPERVISOR_RESPONSE_BYTES + 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forwards_a_signal_delivered_before_supervisor_spawn() {
        let blocked = BlockedSignals::new().unwrap();
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        let (ready_parent, ready_child) = supervisor_ready_socketpair().unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command.args([
            "-c",
            "import os, signal, sys, time\ncancelled = False\ndef cancel(*_):\n global cancelled\n cancelled = True\nsignal.signal(signal.SIGTERM, cancel)\nfd = int(os.environ['RELAY_SUPERVISOR_READY_FD'])\nos.write(fd, b'R')\nack = os.read(fd, 2)\neof = os.read(fd, 1)\nos.close(fd)\nif ack != b'A' or eof != b'': sys.exit(2)\nif cancelled: sys.exit(0)\ntime.sleep(10)",
        ]);
        configure_supervisor_child(&mut command, ready_child.as_raw_fd()).unwrap();
        let mut child = command.spawn().unwrap();
        drop(ready_child);
        let state_path = env::temp_dir().join(format!("relay-ready-{}.json", Uuid::new_v4()));
        let initial_state = snapshot_state_path(&state_path).unwrap();
        let readiness = wait_for_supervisor_ready(
            &mut child,
            ready_parent,
            &state_path,
            initial_state,
            "readiness-unit-test",
        )
        .unwrap();
        let forwarder = SignalForwarder::install(blocked, child.id() as i32).unwrap();
        finish_supervisor_readiness(readiness).unwrap();
        let status = wait_for_supervisor(
            &mut child,
            SignalRegistration::Standalone(Box::new(forwarder)),
        )
        .unwrap();

        assert!(status.success());
    }
}
