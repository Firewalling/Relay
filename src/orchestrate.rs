use crate::{
    app::RunOutput,
    cli::{RunArgs, SuperviseArgs},
    supervise::{self, ExecutionResult, Lifecycle, SupervisorError, TerminationReason},
};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(any(debug_assertions, not(target_os = "linux")))]
use std::fs::OpenOptions;
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};
#[cfg(target_os = "linux")]
use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;
const MAX_PLAN_BYTES: u64 = 1 << 20;
const MAX_STATE_BYTES: u64 = 4 << 20;
const MAX_TASKS: usize = 64;
const MAX_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_GRACE_MS: i64 = 60_000;
const MAX_OUTPUT_BYTES: i64 = 8 * 1024 * 1024;
const MAX_LOG_BYTES: i64 = 4 * 1024 * 1024;
const MAX_REDACTIONS: usize = 32;
const MAX_REDACTED_BYTES: usize = 64 * 1024;
const MAX_SUMMARY_MESSAGE_BYTES: usize = 4096;

#[derive(Debug, Error)]
pub enum RunError {
    #[error("invalid orchestration plan: {0}")]
    Plan(String),
    #[error("invalid orchestration state: {0}")]
    State(String),
    #[error("orchestration state is busy: {0}")]
    Busy(String),
    #[error("supervisor failed for run {run_id}; aggregate state {state_path}: {source}")]
    Supervisor {
        run_id: String,
        state_path: String,
        state: Box<RunState>,
        #[source]
        source: SupervisorError,
    },
}

impl RunError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Plan(_) => "run_plan_invalid",
            Self::State(_) => "run_state_invalid",
            Self::Busy(_) => "run_busy",
            Self::Supervisor { .. } => "run_supervisor",
        }
    }

    pub fn state(&self) -> Option<&RunState> {
        match self {
            Self::Supervisor { state, .. } => Some(state),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    schema_version: u32,
    tasks: Vec<PlanTask>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PlanTask {
    id: String,
    #[serde(default)]
    depends_on: Vec<String>,
    program: String,
    #[serde(default)]
    arguments: Vec<String>,
    working_directory: Option<String>,
    timeout_ms: i64,
    #[serde(default = "default_grace_ms")]
    termination_grace_ms: i64,
    #[serde(default = "default_output_limit")]
    stdout_limit_bytes: i64,
    #[serde(default = "default_output_limit")]
    stderr_limit_bytes: i64,
    #[serde(default = "default_log_limit")]
    log_limit_bytes: i64,
    #[serde(default)]
    redact_argument_indexes: Vec<usize>,
}

const fn default_grace_ms() -> i64 {
    2_000
}
const fn default_output_limit() -> i64 {
    1 << 20
}
const fn default_log_limit() -> i64 {
    64 << 10
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Indeterminate,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Indeterminate => "indeterminate",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
    Indeterminate,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
            Self::Indeterminate => "indeterminate",
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSummary {
    pub lifecycle: Lifecycle,
    pub termination_reason: TerminationReason,
    pub exit_code: Option<i64>,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    pub accepted_at: String,
    pub started_at: String,
    pub completed_at: String,
    pub duration_ms: i64,
    pub stdout_bytes_observed: i64,
    pub stdout_bytes_retained: i64,
    pub stdout_truncated: bool,
    pub stderr_bytes_observed: i64,
    pub stderr_bytes_retained: i64,
    pub stderr_truncated: bool,
    pub cancellation_requested: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRecord {
    pub id: String,
    pub status: TaskStatus,
    pub reason: Option<String>,
    pub attempt_run_id: Option<String>,
    pub supervisor_state_path: Option<String>,
    pub summary: Option<ExecutionSummary>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunState {
    pub schema_version: u32,
    pub run_id: String,
    pub state_path: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: RunStatus,
    pub cancellation_requested: bool,
    plan: Plan,
    pub tasks: Vec<TaskRecord>,
}

pub struct RunResult {
    pub output: RunOutput,
    pub status: RunStatus,
}

impl RunResult {
    pub fn succeeded(&self) -> bool {
        self.status == RunStatus::Completed
    }
}

pub fn run(
    args: RunArgs,
    invocation_directory: &Path,
    project_root: &Path,
) -> Result<RunResult, RunError> {
    let raw_plan = read_bounded(&args.plan, MAX_PLAN_BYTES).map_err(|error| {
        RunError::Plan(format!("could not read {}: {error}", args.plan.display()))
    })?;
    let mut plan: Plan = serde_json::from_slice(&raw_plan).map_err(|error| {
        RunError::Plan(format!("could not decode {}: {error}", args.plan.display()))
    })?;
    validate_plan(&plan, invocation_directory)?;

    if args.state_path.is_some() && args.resume.is_some() {
        return Err(RunError::State(
            "state_path and resume are mutually exclusive".into(),
        ));
    }
    if let Some(supervisor) = args.supervisor.as_deref()
        && !supervisor.is_absolute()
    {
        return Err(RunError::Plan("supervisor path must be absolute".into()));
    }

    let resuming = args.resume.is_some();
    if resuming
        && plan
            .tasks
            .iter()
            .any(|task| !task.redact_argument_indexes.is_empty())
    {
        return Err(RunError::State(
            "resume is unavailable for plans containing redacted_argument_indexes".into(),
        ));
    }
    let requested_state_path = match args.resume.or(args.state_path) {
        Some(path) => path,
        None => project_root
            .join(".relay")
            .join("runs")
            .join(format!("{}.json", Uuid::new_v4())),
    };
    validate_state_path(&requested_state_path, project_root)?;

    let attempts: Vec<(String, PathBuf)> = plan
        .tasks
        .iter()
        .map(|_| {
            let run_id = Uuid::new_v4().to_string();
            let path = supervisor_state_path(&run_id)?;
            Ok((run_id, path))
        })
        .collect::<Result<_, RunError>>()?;

    let mut store;
    let mut state;
    if resuming {
        store = StateStore::open(&requested_state_path, true)?;
        state = store.read()?;
        resolve_working_directories(&mut plan, invocation_directory, Some(&state.plan))?;
        validate_plan(&plan, invocation_directory)?;
        preflight_tasks(&args.supervisor, &plan, &attempts, invocation_directory)?;
        let sanitized_plan = sanitize_plan(&plan);
        validate_resume_state(&state, &sanitized_plan, store.path())?;
        state.cancellation_requested = false;
    } else {
        resolve_working_directories(&mut plan, invocation_directory, None)?;
        validate_plan(&plan, invocation_directory)?;
        preflight_tasks(&args.supervisor, &plan, &attempts, invocation_directory)?;
        ensure_state_parent(&requested_state_path)?;
        validate_state_path(&requested_state_path, project_root)?;
        store = StateStore::open(&requested_state_path, false)?;
        let now = now();
        state = RunState {
            schema_version: SCHEMA_VERSION,
            state_path: path_string(store.path())?,
            run_id: Uuid::new_v4().to_string(),
            created_at: now.clone(),
            updated_at: now,
            status: RunStatus::Running,
            cancellation_requested: false,
            plan: sanitize_plan(&plan),
            tasks: plan
                .tasks
                .iter()
                .map(|task| TaskRecord {
                    id: task.id.clone(),
                    status: TaskStatus::Pending,
                    reason: None,
                    attempt_run_id: None,
                    supervisor_state_path: None,
                    summary: None,
                })
                .collect(),
        };
        test_pause("before_initial_install")?;
        store.write_initial(&state)?;
    }

    mark_blocked(&plan, &mut state);
    state.status = derive_status(&state.tasks, state.cancellation_requested);
    persist(&mut store, &mut state)?;
    let coordinator =
        supervise::CoordinatorSignals::install().map_err(|source| RunError::Supervisor {
            run_id: state.run_id.clone(),
            state_path: state.state_path.clone(),
            state: Box::new(state.clone()),
            source,
        })?;

    loop {
        if coordinator.cancellation_requested() {
            state.cancellation_requested = true;
            state.status = derive_status(&state.tasks, state.cancellation_requested);
            persist(&mut store, &mut state)?;
            return Ok(render(store.path(), state));
        }
        let Some(index) = next_ready(&plan, &state.tasks) else {
            state.status = derive_status(&state.tasks, state.cancellation_requested);
            persist(&mut store, &mut state)?;
            return Ok(render(store.path(), state));
        };

        let task = &plan.tasks[index];
        let (attempt_run_id, supervisor_state_path) = &attempts[index];
        state.tasks[index].status = TaskStatus::Running;
        state.tasks[index].reason = None;
        state.tasks[index].summary = None;
        state.tasks[index].attempt_run_id = Some(attempt_run_id.clone());
        state.tasks[index].supervisor_state_path = Some(path_string(supervisor_state_path)?);
        state.status = RunStatus::Running;
        persist(&mut store, &mut state)?;

        test_pause("before_supervisor_launch")?;
        if coordinator.cancellation_requested() {
            state.tasks[index].status = TaskStatus::Pending;
            state.tasks[index].reason = None;
            state.tasks[index].attempt_run_id = None;
            state.tasks[index].supervisor_state_path = None;
            state.cancellation_requested = true;
            state.status = derive_status(&state.tasks, state.cancellation_requested);
            persist(&mut store, &mut state)?;
            return Ok(render(store.path(), state));
        }

        let supervise_args = task_supervise_args(
            &args.supervisor,
            task,
            attempt_run_id.clone(),
            supervisor_state_path.clone(),
        );
        let execution = match supervise::execute_coordinated(
            supervise_args,
            invocation_directory,
            &coordinator,
        ) {
            Ok(result) => result.execution,
            Err(source) => {
                state.tasks[index].status = TaskStatus::Indeterminate;
                state.tasks[index].reason =
                    Some(truncate(&source.to_string(), MAX_SUMMARY_MESSAGE_BYTES));
                state.status = RunStatus::Indeterminate;
                persist(&mut store, &mut state)?;
                return Err(RunError::Supervisor {
                    run_id: state.run_id.clone(),
                    state_path: state.state_path.clone(),
                    state: Box::new(state),
                    source,
                });
            }
        };

        test_pause("after_supervisor_result")?;
        let coordinator_cancelled = coordinator.cancellation_requested();
        let execution_cancelled = execution.lifecycle == Lifecycle::Cancelled
            || matches!(
                execution.termination_reason,
                TerminationReason::CancelledTerminated | TerminationReason::CancelledKilled
            )
            || execution.cancellation.requested;
        state.tasks[index].summary = Some(summarize(&execution));
        if execution.task_succeeded() {
            state.tasks[index].status = TaskStatus::Succeeded;
        } else if execution_cancelled {
            state.tasks[index].status = TaskStatus::Cancelled;
            state.tasks[index].reason = result_reason(&execution);
        } else {
            state.tasks[index].status = TaskStatus::Failed;
            state.tasks[index].reason = result_reason(&execution);
        }
        mark_blocked(&plan, &mut state);
        if coordinator_cancelled || execution_cancelled {
            state.cancellation_requested = true;
        }
        state.status = derive_status(&state.tasks, state.cancellation_requested);
        persist(&mut store, &mut state)?;

        if state.cancellation_requested {
            return Ok(render(store.path(), state));
        }
    }
}

fn resolve_working_directories(
    plan: &mut Plan,
    invocation_directory: &Path,
    persisted: Option<&Plan>,
) -> Result<(), RunError> {
    let invocation = invocation_directory.to_str().ok_or_else(|| {
        RunError::Plan("invocation directory must be an absolute UTF-8 path".into())
    })?;
    for task in &mut plan.tasks {
        if task.working_directory.is_some() {
            continue;
        }
        task.working_directory = if let Some(persisted) = persisted {
            persisted
                .tasks
                .iter()
                .find(|candidate| candidate.id == task.id)
                .and_then(|candidate| candidate.working_directory.clone())
                .ok_or_else(|| {
                    RunError::State(format!(
                        "persisted plan has no working_directory for task {:?}",
                        task.id
                    ))
                })?
                .into()
        } else {
            Some(invocation.to_owned())
        };
    }
    Ok(())
}

fn task_supervise_args(
    supervisor: &Option<PathBuf>,
    task: &PlanTask,
    run_id: String,
    state_path: PathBuf,
) -> SuperviseArgs {
    SuperviseArgs {
        supervisor: supervisor.clone(),
        run_id: Some(run_id),
        state_path: Some(state_path),
        working_directory: task.working_directory.as_deref().map(PathBuf::from),
        timeout_ms: task.timeout_ms,
        termination_grace_ms: task.termination_grace_ms,
        stdout_limit_bytes: task.stdout_limit_bytes,
        stderr_limit_bytes: task.stderr_limit_bytes,
        log_limit_bytes: task.log_limit_bytes,
        redact_argument_indexes: task.redact_argument_indexes.clone(),
        program_and_args: std::iter::once(task.program.clone())
            .chain(task.arguments.iter().cloned())
            .collect(),
    }
}

fn preflight_tasks(
    supervisor: &Option<PathBuf>,
    plan: &Plan,
    attempts: &[(String, PathBuf)],
    invocation_directory: &Path,
) -> Result<(), RunError> {
    for (task, (run_id, state_path)) in plan.tasks.iter().zip(attempts) {
        let request = task_supervise_args(supervisor, task, run_id.clone(), state_path.clone());
        supervise::preflight(&request, invocation_directory).map_err(|error| {
            RunError::Plan(format!(
                "task {:?} supervisor request is invalid: {error}",
                task.id
            ))
        })?;
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn test_pause(point: &str) -> Result<(), RunError> {
    if env::var_os("RELAY_TEST_RUN_PAUSE").as_deref() != Some(OsStr::new(point)) {
        return Ok(());
    }
    let marker = env::var_os("RELAY_TEST_RUN_MARKER")
        .map(PathBuf::from)
        .ok_or_else(|| RunError::State("RELAY_TEST_RUN_MARKER is required".into()))?;
    let release = env::var_os("RELAY_TEST_RUN_RELEASE")
        .map(PathBuf::from)
        .ok_or_else(|| RunError::State("RELAY_TEST_RUN_RELEASE is required".into()))?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|error| {
            RunError::State(format!(
                "could not create test marker {}: {error}",
                marker.display()
            ))
        })?;
    while !release.exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn test_pause(_point: &str) -> Result<(), RunError> {
    Ok(())
}

fn validate_plan(plan: &Plan, invocation_directory: &Path) -> Result<(), RunError> {
    if plan.schema_version != SCHEMA_VERSION {
        return Err(RunError::Plan(format!(
            "schema_version must be {SCHEMA_VERSION}"
        )));
    }
    if !(1..=MAX_TASKS).contains(&plan.tasks.len()) {
        return Err(RunError::Plan(format!(
            "tasks must contain between 1 and {MAX_TASKS} entries"
        )));
    }
    if !invocation_directory.is_absolute() || invocation_directory.to_str().is_none() {
        return Err(RunError::Plan(
            "invocation directory must be an absolute UTF-8 path".into(),
        ));
    }

    let mut ids = HashSet::with_capacity(plan.tasks.len());
    for task in &plan.tasks {
        if !valid_task_id(&task.id) {
            return Err(RunError::Plan(format!(
                "task id {:?} must match [A-Za-z0-9][A-Za-z0-9._-]{{0,47}}",
                task.id
            )));
        }
        if !ids.insert(task.id.as_str()) {
            return Err(RunError::Plan(format!("duplicate task id {:?}", task.id)));
        }
        validate_task(task)?;
    }
    for task in &plan.tasks {
        let mut dependencies = HashSet::with_capacity(task.depends_on.len());
        for dependency in &task.depends_on {
            if dependency == &task.id {
                return Err(RunError::Plan(format!(
                    "task {:?} cannot depend on itself",
                    task.id
                )));
            }
            if !ids.contains(dependency.as_str()) {
                return Err(RunError::Plan(format!(
                    "task {:?} has unknown dependency {:?}",
                    task.id, dependency
                )));
            }
            if !dependencies.insert(dependency.as_str()) {
                return Err(RunError::Plan(format!(
                    "task {:?} repeats dependency {:?}",
                    task.id, dependency
                )));
            }
        }
    }
    validate_acyclic(plan)
}

fn validate_task(task: &PlanTask) -> Result<(), RunError> {
    let program = Path::new(&task.program);
    if !program.is_absolute() {
        return Err(RunError::Plan(format!(
            "task {:?} program must be absolute",
            task.id
        )));
    }
    validate_text(&format!("task {:?} program", task.id), &task.program)?;
    if let Some(directory) = &task.working_directory {
        if !Path::new(directory).is_absolute() {
            return Err(RunError::Plan(format!(
                "task {:?} working_directory must be absolute",
                task.id
            )));
        }
        validate_text(&format!("task {:?} working_directory", task.id), directory)?;
    }
    for (index, argument) in task.arguments.iter().enumerate() {
        validate_text(&format!("task {:?} arguments[{index}]", task.id), argument)?;
    }
    if !(1..=MAX_TIMEOUT_MS).contains(&task.timeout_ms) {
        return Err(RunError::Plan(format!(
            "task {:?} timeout_ms must be between 1 and {MAX_TIMEOUT_MS}",
            task.id
        )));
    }
    if !(1..=MAX_GRACE_MS).contains(&task.termination_grace_ms) {
        return Err(RunError::Plan(format!(
            "task {:?} termination_grace_ms must be between 1 and {MAX_GRACE_MS}",
            task.id
        )));
    }
    for (name, value, maximum) in [
        (
            "stdout_limit_bytes",
            task.stdout_limit_bytes,
            MAX_OUTPUT_BYTES,
        ),
        (
            "stderr_limit_bytes",
            task.stderr_limit_bytes,
            MAX_OUTPUT_BYTES,
        ),
        ("log_limit_bytes", task.log_limit_bytes, MAX_LOG_BYTES),
    ] {
        if !(1..=maximum).contains(&value) {
            return Err(RunError::Plan(format!(
                "task {:?} {name} must be between 1 and {maximum}",
                task.id
            )));
        }
    }
    if task.redact_argument_indexes.len() > MAX_REDACTIONS {
        return Err(RunError::Plan(format!(
            "task {:?} has more than {MAX_REDACTIONS} redactions",
            task.id
        )));
    }
    let mut indexes = task.redact_argument_indexes.clone();
    indexes.sort_unstable();
    if indexes.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(RunError::Plan(format!(
            "task {:?} has duplicate redaction indexes",
            task.id
        )));
    }
    let redacted_bytes = indexes.iter().try_fold(0usize, |total, &index| {
        task.arguments
            .get(index)
            .map(|argument| total.saturating_add(argument.len()))
            .ok_or_else(|| {
                RunError::Plan(format!(
                    "task {:?} has out-of-range redaction index {index}",
                    task.id
                ))
            })
    })?;
    if redacted_bytes > MAX_REDACTED_BYTES {
        return Err(RunError::Plan(format!(
            "task {:?} redacted values exceed {MAX_REDACTED_BYTES} bytes",
            task.id
        )));
    }
    Ok(())
}

fn validate_text(name: &str, value: &str) -> Result<(), RunError> {
    if value.contains('\0') {
        Err(RunError::Plan(format!(
            "{name} must not contain a NUL byte"
        )))
    } else {
        Ok(())
    }
}

fn validate_acyclic(plan: &Plan) -> Result<(), RunError> {
    let indexes: HashMap<&str, usize> = plan
        .tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id.as_str(), index))
        .collect();
    let mut marks = vec![0u8; plan.tasks.len()];
    fn visit(index: usize, plan: &Plan, indexes: &HashMap<&str, usize>, marks: &mut [u8]) -> bool {
        if marks[index] == 1 {
            return false;
        }
        if marks[index] == 2 {
            return true;
        }
        marks[index] = 1;
        for dependency in &plan.tasks[index].depends_on {
            if !visit(indexes[dependency.as_str()], plan, indexes, marks) {
                return false;
            }
        }
        marks[index] = 2;
        true
    }
    for index in 0..plan.tasks.len() {
        if !visit(index, plan, &indexes, &mut marks) {
            return Err(RunError::Plan(
                "task dependency graph contains a cycle".into(),
            ));
        }
    }
    Ok(())
}

fn valid_task_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn sanitize_plan(plan: &Plan) -> Plan {
    let mut sanitized = plan.clone();
    for task in &mut sanitized.tasks {
        for &index in &task.redact_argument_indexes {
            task.arguments[index] = "<redacted>".into();
        }
        task.redact_argument_indexes.sort_unstable();
    }
    sanitized
}

fn validate_state_path(path: &Path, project_root: &Path) -> Result<(), RunError> {
    if !path.is_absolute() {
        return Err(RunError::State("state path must be absolute".into()));
    }
    if path.to_str().is_none() {
        return Err(RunError::State("state path must be valid UTF-8".into()));
    }

    reject_symlinked_components(path)?;
    if path
        .components()
        .any(|component| component == Component::Normal(OsStr::new(".relay")))
    {
        let runs = project_root.join(".relay").join("runs");
        let lexical_path = normalize_absolute(path)?;
        let lexical_runs = normalize_absolute(&runs)?;
        if lexical_path == lexical_runs || !lexical_path.starts_with(&lexical_runs) {
            return Err(RunError::State(format!(
                "state paths inside .relay must be under {}: {}",
                runs.display(),
                path.display()
            )));
        }

        let canonical_path = resolve_existing_ancestor(&lexical_path)?;
        let canonical_runs = resolve_existing_ancestor(&lexical_runs)?;
        if canonical_path == canonical_runs || !canonical_path.starts_with(&canonical_runs) {
            return Err(RunError::State(format!(
                "state path resolves outside {}: {}",
                runs.display(),
                path.display()
            )));
        }
    }
    Ok(())
}

fn reject_symlinked_components(path: &Path) -> Result<(), RunError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(RunError::State(format!(
                    "state path contains symlinked component {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(RunError::State(format!(
                    "could not inspect {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, RunError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(RunError::State(format!(
                        "state path escapes its filesystem root: {}",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(normalized)
}

fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf, RunError> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            RunError::State(format!(
                "state path has no existing ancestor: {}",
                path.display()
            ))
        })?;
        missing.push(name.to_owned());
        ancestor = ancestor.parent().ok_or_else(|| {
            RunError::State(format!(
                "state path has no existing ancestor: {}",
                path.display()
            ))
        })?;
    }
    let mut resolved = fs::canonicalize(ancestor).map_err(|error| {
        RunError::State(format!("could not resolve {}: {error}", ancestor.display()))
    })?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn ensure_state_parent(path: &Path) -> Result<(), RunError> {
    let parent = path
        .parent()
        .ok_or_else(|| RunError::State("state path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(|error| {
        RunError::State(format!("could not create {}: {error}", parent.display()))
    })?;
    Ok(())
}
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(target_os = "linux")]
fn c_name(name: &OsStr, description: &str) -> Result<CString, RunError> {
    CString::new(name.as_bytes())
        .map_err(|_| RunError::State(format!("{description} must not contain a NUL byte")))
}

#[cfg(target_os = "linux")]
fn open_parent_nofollow(path: &Path) -> Result<File, RunError> {
    let normalized = normalize_absolute(path)?;
    let mut directory =
        File::open("/").map_err(|error| RunError::State(format!("could not open /: {error}")))?;
    for component in normalized.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = c_name(name, "state directory component")?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(RunError::State(format!(
                "could not securely open {}: {}",
                path.display(),
                io::Error::last_os_error()
            )));
        }
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn openat(
    directory: &File,
    name: &CString,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, mode) };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(target_os = "linux")]
struct StateStore {
    path: PathBuf,
    parent_path: PathBuf,
    parent: File,
    name: CString,
    current: Option<FileIdentity>,
    _lock: StateLock,
}

#[cfg(target_os = "linux")]
impl StateStore {
    fn open(path: &Path, resume: bool) -> Result<Self, RunError> {
        let parent_path = path
            .parent()
            .ok_or_else(|| RunError::State("state path has no parent".into()))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| RunError::State("state path must name a file".into()))?;
        let parent = open_parent_nofollow(parent_path)?;
        let canonical_parent = fs::canonicalize(parent_path).map_err(|error| {
            RunError::State(format!(
                "could not canonicalize {}: {error}",
                parent_path.display()
            ))
        })?;
        let canonical_state_path = canonical_parent.join(file_name);
        if resume && normalize_absolute(path)? != canonical_state_path {
            return Err(RunError::State(format!(
                "resume must use canonical state path {}",
                canonical_state_path.display()
            )));
        }
        let path = canonical_state_path;
        let name = c_name(file_name, "state file name")?;
        let lock_name = c_name(
            OsStr::new(&format!(
                "{}.orchestration.lock",
                file_name
                    .to_str()
                    .ok_or_else(|| RunError::State("state path must be valid UTF-8".into()))?
            )),
            "lock file name",
        )?;
        let lock_file = openat(
            &parent,
            &lock_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
        .map_err(|error| {
            RunError::State(format!(
                "could not securely open lock for {}: {error}",
                path.display()
            ))
        })?;
        let lock_metadata = lock_file.metadata().map_err(|error| {
            RunError::State(format!(
                "could not inspect lock for {}: {error}",
                path.display()
            ))
        })?;
        if !lock_metadata.is_file() || lock_metadata.nlink() != 1 {
            return Err(RunError::State(format!(
                "lock for {} must be a singly-linked regular file",
                path.display()
            )));
        }
        match lock_file.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(RunError::Busy(format!(
                    "another coordinator owns {}",
                    path.display()
                )));
            }
            Err(error) => {
                return Err(RunError::State(format!(
                    "could not lock {}: {error}",
                    path.display()
                )));
            }
        }
        let mut store = Self {
            path,
            parent_path: canonical_parent,
            parent,
            name,
            current: None,
            _lock: StateLock { file: lock_file },
        };
        store.verify_parent()?;
        if resume {
            store.current = Some(store.open_owned_state()?.1);
        }
        Ok(store)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn verify_parent(&self) -> Result<(), RunError> {
        let current = open_parent_nofollow(&self.parent_path)?;
        let expected = self.parent.metadata().map_err(|error| {
            RunError::State(format!("could not inspect pinned state directory: {error}"))
        })?;
        let actual = current.metadata().map_err(|error| {
            RunError::State(format!("could not inspect state directory path: {error}"))
        })?;
        if identity(&expected) != identity(&actual) {
            return Err(RunError::State(format!(
                "state directory path no longer identifies pinned directory {}",
                self.parent_path.display()
            )));
        }
        Ok(())
    }

    fn open_owned_state(&self) -> Result<(File, FileIdentity), RunError> {
        self.verify_parent()?;
        let file = openat(
            &self.parent,
            &self.name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|error| {
            RunError::State(format!(
                "could not securely open {}: {error}",
                self.path.display()
            ))
        })?;
        let metadata = file.metadata().map_err(|error| {
            RunError::State(format!(
                "could not inspect {}: {error}",
                self.path.display()
            ))
        })?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(RunError::State(format!(
                "state file must be a singly-linked regular file: {}",
                self.path.display()
            )));
        }
        let actual = identity(&metadata);
        if let Some(expected) = self.current
            && expected != actual
        {
            return Err(RunError::State(format!(
                "state path no longer identifies the owned state file: {}",
                self.path.display()
            )));
        }
        Ok((file, actual))
    }

    fn read(&mut self) -> Result<RunState, RunError> {
        let (file, actual) = self.open_owned_state()?;
        self.current = Some(actual);
        let bytes = read_bounded_file(file, MAX_STATE_BYTES).map_err(|error| {
            RunError::State(format!("could not read {}: {error}", self.path.display()))
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            RunError::State(format!("could not decode {}: {error}", self.path.display()))
        })
    }

    fn write_initial(&mut self, state: &RunState) -> Result<(), RunError> {
        self.write(state, true)
    }

    fn write_update(&mut self, state: &RunState) -> Result<(), RunError> {
        self.write(state, false)
    }

    fn write(&mut self, state: &RunState, initial: bool) -> Result<(), RunError> {
        self.verify_parent()?;
        if initial {
            if self.current.is_some() {
                return Err(RunError::State("state is already installed".into()));
            }
        } else {
            self.open_owned_state()?;
        }
        let mut bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| RunError::State(format!("could not encode state: {error}")))?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(RunError::State(format!(
                "encoded state exceeds {MAX_STATE_BYTES} bytes"
            )));
        }
        let temporary_name =
            CString::new(format!(".relay-orchestration-{}.tmp", Uuid::new_v4())).unwrap();
        let result = (|| {
            let mut temporary = openat(
                &self.parent,
                &temporary_name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
            .map_err(|error| {
                RunError::State(format!(
                    "could not create state temporary in {}: {error}",
                    self.parent_path.display()
                ))
            })?;
            temporary.write_all(&bytes).map_err(|error| {
                RunError::State(format!("could not write state temporary: {error}"))
            })?;
            temporary.sync_all().map_err(|error| {
                RunError::State(format!("could not sync state temporary: {error}"))
            })?;
            if initial {
                let renamed = unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        self.parent.as_raw_fd(),
                        temporary_name.as_ptr(),
                        self.parent.as_raw_fd(),
                        self.name.as_ptr(),
                        libc::RENAME_NOREPLACE,
                    ) as libc::c_int
                };
                if renamed != 0 {
                    let error = io::Error::last_os_error();
                    return Err(if error.kind() == io::ErrorKind::AlreadyExists {
                        RunError::State(format!(
                            "state path already exists: {}",
                            self.path.display()
                        ))
                    } else {
                        RunError::State(format!(
                            "could not install {} without replacement: {error}",
                            self.path.display()
                        ))
                    });
                }
            } else {
                let expected = self
                    .current
                    .ok_or_else(|| RunError::State("owned state identity is missing".into()))?;
                let exchanged = unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        self.parent.as_raw_fd(),
                        temporary_name.as_ptr(),
                        self.parent.as_raw_fd(),
                        self.name.as_ptr(),
                        libc::RENAME_EXCHANGE,
                    ) as libc::c_int
                };
                if exchanged != 0 {
                    return Err(RunError::State(format!(
                        "could not atomically exchange {}: {}",
                        self.path.display(),
                        io::Error::last_os_error()
                    )));
                }
                let displaced = openat(
                    &self.parent,
                    &temporary_name,
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                );
                let displaced_owned =
                    displaced
                        .and_then(|file| file.metadata())
                        .is_ok_and(|metadata| {
                            metadata.is_file()
                                && metadata.nlink() == 1
                                && identity(&metadata) == expected
                        });
                if !displaced_owned {
                    let rolled_back = unsafe {
                        libc::syscall(
                            libc::SYS_renameat2,
                            self.parent.as_raw_fd(),
                            temporary_name.as_ptr(),
                            self.parent.as_raw_fd(),
                            self.name.as_ptr(),
                            libc::RENAME_EXCHANGE,
                        ) as libc::c_int
                    };
                    return Err(if rolled_back == 0 {
                        RunError::State(format!(
                            "state path changed before update: {}",
                            self.path.display()
                        ))
                    } else {
                        RunError::State(format!(
                            "state path changed and rollback failed for {}: {}",
                            self.path.display(),
                            io::Error::last_os_error()
                        ))
                    });
                }
                if unsafe { libc::unlinkat(self.parent.as_raw_fd(), temporary_name.as_ptr(), 0) }
                    != 0
                {
                    return Err(RunError::State(format!(
                        "could not remove replaced state inode for {}: {}",
                        self.path.display(),
                        io::Error::last_os_error()
                    )));
                }
            }
            self.parent.sync_all().map_err(|error| {
                RunError::State(format!(
                    "could not sync {}: {error}",
                    self.parent_path.display()
                ))
            })?;
            self.current = None;
            let (_, actual) = self.open_owned_state()?;
            self.current = Some(actual);
            Ok(())
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), temporary_name.as_ptr(), 0);
            }
        }
        result
    }
}

#[cfg(not(target_os = "linux"))]
struct StateStore {
    path: PathBuf,
    _lock: StateLock,
}

#[cfg(not(target_os = "linux"))]
impl StateStore {
    fn open(path: &Path, resume: bool) -> Result<Self, RunError> {
        let path = if resume {
            fs::canonicalize(path).map_err(|error| {
                RunError::State(format!(
                    "could not canonicalize {}: {error}",
                    path.display()
                ))
            })?
        } else {
            let parent = fs::canonicalize(
                path.parent()
                    .ok_or_else(|| RunError::State("state path has no parent".into()))?,
            )
            .map_err(|error| {
                RunError::State(format!("could not canonicalize state parent: {error}"))
            })?;
            parent.join(
                path.file_name()
                    .ok_or_else(|| RunError::State("state path must name a file".into()))?,
            )
        };
        let lock = acquire_lock(&path)?;
        Ok(Self { path, _lock: lock })
    }
    fn path(&self) -> &Path {
        &self.path
    }
    fn read(&mut self) -> Result<RunState, RunError> {
        read_state(&self.path)
    }
    fn write_initial(&mut self, state: &RunState) -> Result<(), RunError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|error| {
                RunError::State(format!("could not create {}: {error}", self.path.display()))
            })?;
        let mut bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| RunError::State(format!("could not encode state: {error}")))?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                RunError::State(format!(
                    "could not persist {}: {error}",
                    self.path.display()
                ))
            })
    }
    fn write_update(&mut self, state: &RunState) -> Result<(), RunError> {
        write_state(&self.path, state)
    }
}

struct StateLock {
    file: File,
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(not(target_os = "linux"))]
fn lock_path(state_path: &Path) -> Result<PathBuf, RunError> {
    let name = state_path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| RunError::State("state path must name a file".into()))?;
    Ok(state_path.with_file_name(format!("{name}.orchestration.lock")))
}

#[cfg(not(target_os = "linux"))]
fn acquire_lock(state_path: &Path) -> Result<StateLock, RunError> {
    let path = lock_path(state_path)?;
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && metadata.file_type().is_symlink()
    {
        return Err(RunError::State(format!(
            "lock path must not be a symlink: {}",
            path.display()
        )));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| RunError::State(format!("could not open {}: {error}", path.display())))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(StateLock { file }),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Err(RunError::Busy(format!(
            "another coordinator owns {}",
            path.display()
        ))),
        Err(error) => Err(RunError::State(format!(
            "could not lock {}: {error}",
            path.display()
        ))),
    }
}

fn persist(store: &mut StateStore, state: &mut RunState) -> Result<(), RunError> {
    state.updated_at = now();
    store.write_update(state)
}

#[cfg(not(target_os = "linux"))]
fn write_state(path: &Path, state: &RunState) -> Result<(), RunError> {
    let mut bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| RunError::State(format!("could not encode state: {error}")))?;
    bytes.push(b'\n');
    let parent = path
        .parent()
        .ok_or_else(|| RunError::State("state path has no parent".into()))?;
    let temporary = parent.join(format!(".relay-orchestration-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                RunError::State(format!("could not create {}: {error}", temporary.display()))
            })?;
        file.write_all(&bytes).map_err(|error| {
            RunError::State(format!("could not write {}: {error}", temporary.display()))
        })?;
        file.sync_all().map_err(|error| {
            RunError::State(format!("could not sync {}: {error}", temporary.display()))
        })?;
        fs::rename(&temporary, path).map_err(|error| {
            RunError::State(format!("could not replace {}: {error}", path.display()))
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                RunError::State(format!("could not sync {}: {error}", parent.display()))
            })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(target_os = "linux"))]
fn read_state(path: &Path) -> Result<RunState, RunError> {
    let bytes = read_bounded(path, MAX_STATE_BYTES)
        .map_err(|error| RunError::State(format!("could not read {}: {error}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| RunError::State(format!("could not decode {}: {error}", path.display())))
}

fn read_bounded_file(file: File, maximum: u64) -> io::Result<Vec<u8>> {
    if file.metadata()?.len() > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds {maximum} bytes"),
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds {maximum} bytes"),
        ));
    }
    Ok(bytes)
}

fn read_bounded(path: &Path, maximum: u64) -> io::Result<Vec<u8>> {
    read_bounded_file(File::open(path)?, maximum)
}

fn validate_resume_state(state: &RunState, plan: &Plan, state_path: &Path) -> Result<(), RunError> {
    if state.schema_version != SCHEMA_VERSION {
        return Err(RunError::State(format!(
            "schema_version must be {SCHEMA_VERSION}"
        )));
    }
    if state.state_path != path_string(state_path)? {
        return Err(RunError::State(format!(
            "resume must use the persisted canonical state_path {}",
            state.state_path
        )));
    }
    if state
        .plan
        .tasks
        .iter()
        .any(|task| !task.redact_argument_indexes.is_empty())
    {
        return Err(RunError::State(
            "resume is unavailable for plans containing redacted_argument_indexes".into(),
        ));
    }
    if &state.plan != plan {
        return Err(RunError::State(
            "resume plan does not match persisted sanitized plan".into(),
        ));
    }
    if state.tasks.len() != plan.tasks.len()
        || state
            .tasks
            .iter()
            .zip(&plan.tasks)
            .any(|(record, task)| record.id != task.id)
    {
        return Err(RunError::State(
            "persisted task records do not match the plan".into(),
        ));
    }
    if state.run_id.parse::<Uuid>().is_err() {
        return Err(RunError::State("persisted run_id is invalid".into()));
    }
    for (name, value) in [
        ("created_at", state.created_at.as_str()),
        ("updated_at", state.updated_at.as_str()),
    ] {
        chrono::DateTime::parse_from_rfc3339(value)
            .map_err(|_| RunError::State(format!("persisted {name} is invalid")))?;
    }
    if state.status != derive_status(&state.tasks, state.cancellation_requested) {
        return Err(RunError::State(
            "persisted run status is inconsistent with its tasks".into(),
        ));
    }
    if state
        .tasks
        .iter()
        .any(|task| matches!(task.status, TaskStatus::Running | TaskStatus::Indeterminate))
    {
        return Err(RunError::State(
            "resume is unsafe with running or indeterminate task records".into(),
        ));
    }
    validate_resume_dependencies(plan, &state.tasks)?;
    let mut attempt_ids = HashSet::new();
    let mut supervisor_paths = HashSet::new();
    for task in &state.tasks {
        let has_attempt = task.attempt_run_id.is_some() && task.supervisor_state_path.is_some();
        if task.attempt_run_id.is_some() != task.supervisor_state_path.is_some() {
            return Err(RunError::State(format!(
                "task {:?} has an incomplete attempt identity",
                task.id
            )));
        }
        match task.status {
            TaskStatus::Pending | TaskStatus::Blocked if has_attempt => {
                return Err(RunError::State(format!(
                    "task {:?} has an attempt despite status {}",
                    task.id,
                    task.status.as_str()
                )));
            }
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled if !has_attempt => {
                return Err(RunError::State(format!(
                    "terminal task {:?} has no attempt identity",
                    task.id
                )));
            }
            _ => {}
        }
        if let Some(attempt) = &task.attempt_run_id
            && attempt.parse::<Uuid>().is_err()
        {
            return Err(RunError::State(format!(
                "task {:?} has an invalid attempt_run_id",
                task.id
            )));
        }
        if let Some(supervisor_path) = &task.supervisor_state_path {
            let attempt = task.attempt_run_id.as_deref().ok_or_else(|| {
                RunError::State(format!(
                    "task {:?} supervisor state has no attempt identity",
                    task.id
                ))
            })?;
            let supervisor_path = Path::new(supervisor_path);
            validate_supervisor_state_path(supervisor_path)?;
            if supervisor_path.file_stem() != Some(OsStr::new(attempt))
                || supervisor_path.extension() != Some(OsStr::new("json"))
            {
                return Err(RunError::State(format!(
                    "task {:?} supervisor state path does not match its attempt identity",
                    task.id
                )));
            }
            if !attempt_ids.insert(attempt) || !supervisor_paths.insert(supervisor_path) {
                return Err(RunError::State(format!(
                    "task {:?} reuses another task's attempt identity or supervisor state path",
                    task.id
                )));
            }
        }
        if matches!(task.status, TaskStatus::Pending | TaskStatus::Blocked)
            && task.summary.is_some()
        {
            return Err(RunError::State(format!(
                "non-executed task {:?} has an execution summary",
                task.id
            )));
        }
        if matches!(
            task.status,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
        ) && task.summary.is_none()
        {
            return Err(RunError::State(format!(
                "terminal task {:?} has no execution summary",
                task.id
            )));
        }
        if let Some(summary) = &task.summary
            && matches!(
                task.status,
                TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
            )
        {
            let Some(summary_status) = task_status_from_summary(summary) else {
                return Err(RunError::State(format!(
                    "terminal task {:?} has an invalid execution summary",
                    task.id
                )));
            };
            if task.status != summary_status {
                return Err(RunError::State(format!(
                    "terminal task {:?} status {} conflicts with its execution summary",
                    task.id,
                    task.status.as_str()
                )));
            }
            let reason_is_valid = if task.status == TaskStatus::Succeeded {
                task.reason.is_none()
            } else {
                task.reason.is_some()
            };
            if !reason_is_valid {
                return Err(RunError::State(format!(
                    "terminal task {:?} has an inconsistent reason",
                    task.id
                )));
            }
        }
    }
    Ok(())
}

fn task_status_from_summary(summary: &ExecutionSummary) -> Option<TaskStatus> {
    let pair_is_valid = matches!(
        (summary.lifecycle, summary.termination_reason),
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
    );
    if !pair_is_valid
        || (summary.termination_reason == TerminationReason::ExitZero
            && summary.exit_code != Some(0))
        || (summary.termination_reason == TerminationReason::ExitNonzero
            && summary.exit_code == Some(0))
        || (summary.termination_reason == TerminationReason::StartError
            && summary.exit_code.is_some())
        || summary.failure_code.is_some() != summary.failure_message.is_some()
        || summary.failure_code.is_some()
            != (summary.termination_reason == TerminationReason::StartError)
        || summary.duration_ms < 0
        || summary.stdout_bytes_observed < 0
        || summary.stdout_bytes_retained < 0
        || summary.stdout_bytes_retained > summary.stdout_bytes_observed
        || summary.stderr_bytes_observed < 0
        || summary.stderr_bytes_retained < 0
        || summary.stderr_bytes_retained > summary.stderr_bytes_observed
    {
        return None;
    }
    let cancelled = matches!(
        summary.termination_reason,
        TerminationReason::CancelledTerminated | TerminationReason::CancelledKilled
    );
    if summary.cancellation_requested != cancelled {
        return None;
    }
    if summary.lifecycle == Lifecycle::Completed
        && summary.termination_reason == TerminationReason::ExitZero
    {
        Some(TaskStatus::Succeeded)
    } else if cancelled {
        Some(TaskStatus::Cancelled)
    } else {
        Some(TaskStatus::Failed)
    }
}

fn validate_resume_dependencies(plan: &Plan, records: &[TaskRecord]) -> Result<(), RunError> {
    let statuses: HashMap<&str, TaskStatus> = records
        .iter()
        .map(|record| (record.id.as_str(), record.status))
        .collect();
    for (index, task) in plan.tasks.iter().enumerate() {
        let record = &records[index];
        let unsuccessful_dependency = task.depends_on.iter().find(|dependency| {
            let status = statuses[dependency.as_str()];
            status.is_terminal() && status != TaskStatus::Succeeded
        });
        match record.status {
            TaskStatus::Pending => {
                if let Some(dependency) = unsuccessful_dependency {
                    return Err(RunError::State(format!(
                        "pending task {:?} has unsuccessful dependency {dependency:?}",
                        task.id
                    )));
                }
                if record.reason.is_some() {
                    return Err(RunError::State(format!(
                        "pending task {:?} has a terminal reason",
                        task.id
                    )));
                }
            }
            TaskStatus::Blocked => {
                if unsuccessful_dependency.is_none() {
                    return Err(RunError::State(format!(
                        "blocked task {:?} has no unsuccessful dependency",
                        task.id
                    )));
                }
                let reason_matches = record.reason.as_deref().is_some_and(|reason| {
                    task.depends_on.iter().any(|dependency| {
                        let status = statuses[dependency.as_str()];
                        status.is_terminal()
                            && status != TaskStatus::Succeeded
                            && reason == format!("dependency {dependency} did not succeed")
                    })
                });
                if !reason_matches {
                    return Err(RunError::State(format!(
                        "blocked task {:?} has an inconsistent reason",
                        task.id
                    )));
                }
            }
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled => {
                if let Some(dependency) = task
                    .depends_on
                    .iter()
                    .find(|dependency| statuses[dependency.as_str()] != TaskStatus::Succeeded)
                {
                    return Err(RunError::State(format!(
                        "executed task {:?} has dependency {dependency:?} that did not succeed",
                        task.id
                    )));
                }
            }
            TaskStatus::Running | TaskStatus::Indeterminate => {
                unreachable!("unsafe task statuses are rejected before dependency validation")
            }
        }
    }
    Ok(())
}

fn next_ready(plan: &Plan, records: &[TaskRecord]) -> Option<usize> {
    let statuses: HashMap<&str, TaskStatus> = records
        .iter()
        .map(|record| (record.id.as_str(), record.status))
        .collect();
    plan.tasks.iter().enumerate().find_map(|(index, task)| {
        (records[index].status == TaskStatus::Pending
            && task
                .depends_on
                .iter()
                .all(|dependency| statuses[dependency.as_str()] == TaskStatus::Succeeded))
        .then_some(index)
    })
}

fn mark_blocked(plan: &Plan, state: &mut RunState) {
    let indexes: HashMap<&str, usize> = plan
        .tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id.as_str(), index))
        .collect();
    loop {
        let mut changed = false;
        for (index, task) in plan.tasks.iter().enumerate() {
            if state.tasks[index].status != TaskStatus::Pending {
                continue;
            }
            if let Some(dependency) = task.depends_on.iter().find(|dependency| {
                let status = state.tasks[indexes[dependency.as_str()]].status;
                status.is_terminal() && status != TaskStatus::Succeeded
            }) {
                state.tasks[index].status = TaskStatus::Blocked;
                state.tasks[index].reason =
                    Some(format!("dependency {dependency} did not succeed"));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn derive_status(tasks: &[TaskRecord], cancellation_requested: bool) -> RunStatus {
    if tasks
        .iter()
        .any(|task| task.status == TaskStatus::Indeterminate)
    {
        RunStatus::Indeterminate
    } else if cancellation_requested
        || tasks
            .iter()
            .any(|task| task.status == TaskStatus::Cancelled)
    {
        RunStatus::Cancelled
    } else if tasks
        .iter()
        .any(|task| matches!(task.status, TaskStatus::Pending | TaskStatus::Running))
    {
        RunStatus::Running
    } else if tasks
        .iter()
        .any(|task| matches!(task.status, TaskStatus::Failed | TaskStatus::Blocked))
    {
        RunStatus::Failed
    } else {
        RunStatus::Completed
    }
}

fn expected_supervisor_state_path(attempt_run_id: &str) -> PathBuf {
    env::temp_dir()
        .join("relay")
        .join("supervisor")
        .join(format!("{attempt_run_id}.json"))
}

fn validate_supervisor_state_path(path: &Path) -> Result<(), RunError> {
    if !path.is_absolute() || normalize_absolute(path)? != path {
        return Err(RunError::State(format!(
            "supervisor state path must be normalized and absolute: {}",
            path.display()
        )));
    }
    if path
        .components()
        .any(|component| component == Component::Normal(OsStr::new(".relay")))
    {
        return Err(RunError::State(format!(
            "supervisor state path must not be inside .relay: {}",
            path.display()
        )));
    }
    reject_symlinked_components(path)
}

fn supervisor_state_path(attempt_run_id: &str) -> Result<PathBuf, RunError> {
    let path = expected_supervisor_state_path(attempt_run_id);
    validate_supervisor_state_path(&path)?;
    let directory = path
        .parent()
        .ok_or_else(|| RunError::State("supervisor state path has no parent".into()))?;
    fs::create_dir_all(directory).map_err(|error| {
        RunError::State(format!("could not create {}: {error}", directory.display()))
    })?;
    validate_supervisor_state_path(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
            RunError::State(format!("could not secure {}: {error}", directory.display()))
        })?;
    }
    Ok(path)
}

fn summarize(result: &ExecutionResult) -> ExecutionSummary {
    ExecutionSummary {
        lifecycle: result.lifecycle,
        termination_reason: result.termination_reason,
        exit_code: result.exit_code,
        failure_code: result.failure.as_ref().map(|failure| failure.code.clone()),
        failure_message: result
            .failure
            .as_ref()
            .map(|failure| truncate(&failure.message, MAX_SUMMARY_MESSAGE_BYTES)),
        accepted_at: result.accepted_at.clone(),
        started_at: result.started_at.clone(),
        completed_at: result.completed_at.clone(),
        duration_ms: result.duration_ms,
        stdout_bytes_observed: result.stdout.bytes_observed,
        stdout_bytes_retained: result.stdout.bytes_retained,
        stdout_truncated: result.stdout.truncated,
        stderr_bytes_observed: result.stderr.bytes_observed,
        stderr_bytes_retained: result.stderr.bytes_retained,
        stderr_truncated: result.stderr.truncated,
        cancellation_requested: result.cancellation.requested,
    }
}

fn result_reason(result: &ExecutionResult) -> Option<String> {
    result
        .failure
        .as_ref()
        .map(|failure| truncate(&failure.message, MAX_SUMMARY_MESSAGE_BYTES))
        .or_else(|| Some(termination_reason_name(result.termination_reason).into()))
}

fn termination_reason_name(reason: TerminationReason) -> &'static str {
    match reason {
        TerminationReason::ExitZero => "exit_zero",
        TerminationReason::ExitNonzero => "exit_nonzero",
        TerminationReason::StartError => "start_error",
        TerminationReason::TimeoutTerminated => "timeout_terminated",
        TerminationReason::CancelledTerminated => "cancelled_terminated",
        TerminationReason::TimeoutKilled => "timeout_killed",
        TerminationReason::CancelledKilled => "cancelled_killed",
        TerminationReason::DescendantsTerminated => "descendants_terminated",
        TerminationReason::DescendantsKilled => "descendants_killed",
        TerminationReason::ProtocolFailure => "protocol_failure",
    }
}

fn truncate(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn render(path: &Path, state: RunState) -> RunResult {
    let mut human = format!(
        "Run {}: {}\nState: {}\nTasks:",
        state.run_id,
        state.status.as_str(),
        path.display()
    );
    for task in &state.tasks {
        human.push_str(&format!("\n  {}: {}", task.id, task.status.as_str()));
        if let Some(reason) = &task.reason {
            human.push_str(&format!(" ({reason})"));
        }
        if let Some(supervisor_path) = &task.supervisor_state_path {
            human.push_str(&format!(" [{}]", supervisor_path));
        }
    }
    let status = state.status;
    RunResult {
        status,
        output: RunOutput {
            human,
            data: json!({
                "state_path": path.to_string_lossy(),
                "run": state,
            }),
        },
    }
}

fn path_string(path: &Path) -> Result<String, RunError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| RunError::State(format!("path is not valid UTF-8: {}", path.display())))
}

fn now() -> String {
    Utc::now().to_rfc3339()
}
