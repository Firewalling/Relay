use crate::{app::RunOutput, cli::SuperviseArgs};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env,
    ffi::OsStr,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
};
use thiserror::Error;
use uuid::Uuid;

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

const OUTPUT_SCHEMA_VERSION: u32 = 1;
const EXECUTION_PROTOCOL_VERSION: u32 = 1;
const MAX_SUPERVISOR_REQUEST_BYTES: usize = 1 << 20;
const MAX_SUPERVISOR_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_GRACE_MS: i64 = 60_000;
const MAX_OUTPUT_BYTES: i64 = 8 * 1024 * 1024;
const MAX_LOG_BYTES: i64 = 4 * 1024 * 1024;
const MAX_REDACTIONS: usize = 32;
const MAX_REDACTED_BYTES: usize = 64 * 1024;

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
    data: Value,
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

pub fn run(args: SuperviseArgs, invocation_directory: &Path) -> Result<RunOutput, SupervisorError> {
    let request = build_request(&args, invocation_directory)?;
    let supervisor = discover_supervisor(args.supervisor.as_deref())?;
    let request_bytes = serde_json::to_vec(&request).map_err(|error| {
        SupervisorError::InvalidRequest(format!("could not encode request: {error}"))
    })?;
    if request_bytes.len().saturating_add(1) > MAX_SUPERVISOR_REQUEST_BYTES {
        return Err(SupervisorError::InvalidRequest(format!(
            "encoded execution request exceeds {MAX_SUPERVISOR_REQUEST_BYTES} bytes"
        )));
    }
    let blocked_signals = BlockedSignals::new()?;
    let mut command = Command::new(&supervisor);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "linux")]
    blocked_signals.restore_in_child(&mut command);
    let mut child = command.spawn().map_err(|source| SupervisorError::Spawn {
        path: supervisor,
        source,
    })?;
    let signals = match SignalForwarder::install(blocked_signals, child.id() as i32) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin is present");
    let stdout = child.stdout.take().expect("piped stdout is present");
    let stderr = child.stderr.take().expect("piped stderr is present");
    let stdout_reader = thread::spawn(move || read_bounded(stdout, "stdout"));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, "stderr"));
    let write_error = stdin
        .write_all(&request_bytes)
        .and_then(|_| stdin.write_all(b"\n"))
        .err();
    drop(stdin);
    let status = wait_for_supervisor(&mut child, signals);
    let stdout = stdout_reader
        .join()
        .map_err(|_| SupervisorError::Transport(io::Error::other("stdout reader panicked")));
    let stderr = stderr_reader
        .join()
        .map_err(|_| SupervisorError::Transport(io::Error::other("stderr reader panicked")));
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
            &request.run_id,
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
    Ok(RunOutput {
        human: human_output(&request.state_path, &result),
        data: json!({"state_path": request.state_path, "execution": result}),
    })
}

#[cfg(target_os = "linux")]
fn wait_for_supervisor(
    child: &mut std::process::Child,
    signals: SignalForwarder,
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
        drop(signals);
        let _ = child.kill();
        let _ = child.wait();
        return Err(SupervisorError::Transport(error));
    }
    // WNOWAIT keeps the exited supervisor as a zombie, so its PID cannot be
    // reused until forwarding is disarmed and Child::wait performs the reap.
    drop(signals);
    child.wait().map_err(SupervisorError::Transport)
}

#[cfg(not(target_os = "linux"))]
fn wait_for_supervisor(
    child: &mut std::process::Child,
    signals: SignalForwarder,
) -> Result<std::process::ExitStatus, SupervisorError> {
    let status = child.wait().map_err(SupervisorError::Transport);
    drop(signals);
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
    run_id: &str,
    execution: &Value,
) -> Result<(), SupervisorError> {
    validate_header(schema, command)?;
    if execution.get("protocol_version").and_then(Value::as_u64)
        != Some(EXECUTION_PROTOCOL_VERSION.into())
    {
        return Err(SupervisorError::InvalidResponse(
            "unexpected execution protocol version".into(),
        ));
    }
    if execution.get("run_id").and_then(Value::as_str) != Some(run_id) {
        return Err(SupervisorError::InvalidResponse(
            "execution run_id did not match request".into(),
        ));
    }
    if execution
        .get("supervisor_version")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(SupervisorError::InvalidResponse(
            "execution supervisor_version is missing".into(),
        ));
    }
    let lifecycle = execution
        .get("lifecycle")
        .and_then(Value::as_str)
        .ok_or_else(|| SupervisorError::InvalidResponse("execution lifecycle is missing".into()))?;
    if !matches!(
        lifecycle,
        "completed" | "failed_to_start" | "timed_out" | "cancelled" | "killed"
    ) {
        return Err(SupervisorError::InvalidResponse(format!(
            "unknown or non-final execution lifecycle {lifecycle:?}"
        )));
    }
    let termination_reason = execution
        .get("termination_reason")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SupervisorError::InvalidResponse("execution termination_reason is missing".into())
        })?;
    if !matches!(
        termination_reason,
        "exit_zero"
            | "exit_nonzero"
            | "start_error"
            | "timeout_terminated"
            | "cancelled_terminated"
            | "timeout_killed"
            | "cancelled_killed"
            | "descendants_terminated"
            | "descendants_killed"
    ) {
        return Err(SupervisorError::InvalidResponse(format!(
            "unknown execution termination_reason {termination_reason:?}"
        )));
    }
    if !matches!(
        (lifecycle, termination_reason),
        (
            "completed",
            "exit_zero" | "exit_nonzero" | "descendants_terminated"
        ) | ("failed_to_start", "start_error")
            | ("timed_out", "timeout_terminated")
            | ("cancelled", "cancelled_terminated")
            | (
                "killed",
                "timeout_killed" | "cancelled_killed" | "descendants_killed"
            )
    ) {
        return Err(SupervisorError::InvalidResponse(format!(
            "inconsistent execution lifecycle and termination_reason: {lifecycle:?}/{termination_reason:?}"
        )));
    }
    if execution
        .get("duration_ms")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err(SupervisorError::InvalidResponse(
            "execution duration_ms is missing".into(),
        ));
    }
    if !execution
        .get("exit_code")
        .is_some_and(|value| value.is_null() || value.as_i64().is_some())
    {
        return Err(SupervisorError::InvalidResponse(
            "execution exit_code must be an integer or null".into(),
        ));
    }
    for stream in ["stdout", "stderr"] {
        if execution
            .pointer(&format!("/{stream}/bytes_retained"))
            .and_then(Value::as_u64)
            .is_none()
        {
            return Err(SupervisorError::InvalidResponse(format!(
                "execution {stream}.bytes_retained is missing"
            )));
        }
    }
    Ok(())
}

fn human_output(state_path: &str, execution: &Value) -> String {
    let text = |key| {
        execution
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    };
    let exit = execution
        .get("exit_code")
        .map_or("unknown".into(), |value| value.to_string());
    let duration = execution
        .get("duration_ms")
        .map_or("unknown".into(), |value| value.to_string());
    let stdout = execution
        .pointer("/stdout/bytes_retained")
        .map_or("unknown".into(), |value| value.to_string());
    let stderr = execution
        .pointer("/stderr/bytes_retained")
        .map_or("unknown".into(), |value| value.to_string());
    format!(
        "Supervised execution: {}\nReason: {}\nExit code: {exit}\nDuration: {duration} ms\nState: {state_path}\nRetained stdout: {stdout} bytes\nRetained stderr: {stderr} bytes",
        text("lifecycle"),
        text("termination_reason")
    )
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

    fn restore_in_child(&self, command: &mut Command) {
        // sigset_t is plain C data. The child restores the pre-launch mask before exec,
        // so forwarded signals cannot remain blocked in relay-supervisor.
        let previous_mask = unsafe { std::ptr::read(&self.previous_mask) };
        unsafe {
            command.pre_exec(move || {
                if libc::sigprocmask(libc::SIG_SETMASK, &previous_mask, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
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
extern "C" fn forward_signal(signal: libc::c_int) {
    let pid = ACTIVE_SUPERVISOR_PID.load(std::sync::atomic::Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}
#[cfg(target_os = "linux")]
impl SignalForwarder {
    fn install(blocked: BlockedSignals, pid: i32) -> Result<Self, SupervisorError> {
        // The caller blocks both signals before spawn. A signal delivered anywhere
        // through spawn, handler setup, or PID publication stays pending until the
        // forwarding target is publishable.
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
        use std::os::unix::process::ExitStatusExt;
        let blocked = BlockedSignals::new().unwrap();
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        let mut command = Command::new("/bin/sleep");
        command.arg("10");
        blocked.restore_in_child(&mut command);
        let mut child = command.spawn().unwrap();
        let forwarder = SignalForwarder::install(blocked, child.id() as i32).unwrap();
        let status = wait_for_supervisor(&mut child, forwarder).unwrap();

        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}
