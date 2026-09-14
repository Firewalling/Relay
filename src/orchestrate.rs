use crate::{
    app::RunOutput,
    cli::{RunArgs, SuperviseArgs},
    supervise::{self, ExecutionResult, Lifecycle, SupervisorError, TerminationReason},
};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
#[cfg(all(unix, not(target_os = "linux")))]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
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

pub(crate) const PLAN_SCHEMA_VERSION: u32 = 3;
const PREVIOUS_PLAN_SCHEMA_VERSION: u32 = 2;
const LEGACY_PLAN_SCHEMA_VERSION: u32 = 1;
const RUN_STATE_SCHEMA_VERSION: u32 = 5;
const PREVIOUS_RUN_STATE_SCHEMA_VERSION: u32 = 4;
const PREVIOUS_PREVIOUS_RUN_STATE_SCHEMA_VERSION: u32 = 3;
const RUN_STATE_V2_SCHEMA_VERSION: u32 = 2;
const LEGACY_RUN_STATE_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_PLAN_BYTES: u64 = 1 << 20;
const MAX_STATE_BYTES: u64 = 4 << 20;
pub(crate) const MAX_TASKS: usize = 64;
const MAX_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_GRACE_MS: i64 = 60_000;
const MAX_OUTPUT_BYTES: i64 = 8 * 1024 * 1024;
const MAX_LOG_BYTES: i64 = 4 * 1024 * 1024;
const MAX_REDACTIONS: usize = 32;
const MAX_REDACTED_BYTES: usize = 64 * 1024;
const MAX_FAILURE_CODE_BYTES: usize = 64;
const MAX_SUMMARY_MESSAGE_BYTES: usize = 256;
const MAX_RETRY_ATTEMPTS: u32 = 10;
const MAX_RETRY_DELAY_MS: u64 = 24 * 60 * 60 * 1000;
const RETRY_POLL_INTERVAL_MS: u64 = 10;
const MAX_OUTPUTS_PER_TASK: usize = 4;
const MAX_OUTPUT_NAME_BYTES: usize = 64;
const MAX_STRUCTURED_OUTPUT_BYTES: usize = 2 * 1024;
const MAX_ARTIFACT_PATH_BYTES: usize = 512;
const MAX_ARTIFACT_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARTIFACT_METADATA_BYTES: usize = 2 * 1024;
const MAX_OUTPUT_MANIFEST_BYTES: usize = 4 * 1024;
const MAX_WORKFLOW_METADATA_BYTES: usize = 16 * 1024;
const MAX_WORKFLOW_NAME_BYTES: usize = 48;
const MAX_WORKFLOW_INPUTS: usize = 16;
const MAX_WORKFLOW_INPUT_NAME_BYTES: usize = 48;
const MAX_WORKFLOW_INPUT_STRING_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationClassification {
    Completed,
    Failed,
    Cancelled,
    StillRunning,
    Indeterminate,
    Refused,
}

impl ReconciliationClassification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::StillRunning => "still_running",
            Self::Indeterminate => "indeterminate",
            Self::Refused => "refused",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ReconciliationReport {
    pub task_id: String,
    pub attempt_run_id: String,
    pub classification: ReconciliationClassification,
    pub reason: String,
}

#[derive(Debug, Error)]
pub enum RunError {
    #[error("invalid orchestration plan: {0}")]
    Plan(String),
    #[error("invalid orchestration state: {0}")]
    State(String),
    #[error("orchestration state is busy: {0}")]
    Busy(String),
    #[error("resume reconciliation refused: {message}")]
    Reconciliation {
        message: String,
        state: Box<RunState>,
        reports: Vec<ReconciliationReport>,
    },
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
            Self::Reconciliation { .. } => "run_state_invalid",
            Self::Supervisor { .. } => "run_supervisor",
        }
    }

    pub fn state(&self) -> Option<&RunState> {
        match self {
            Self::Reconciliation { state, .. } | Self::Supervisor { state, .. } => Some(state),
            _ => None,
        }
    }

    pub fn reconciliation(&self) -> &[ReconciliationReport] {
        match self {
            Self::Reconciliation { reports, .. } => reports,
            _ => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputType {
    Json,
    File,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct OutputDeclaration {
    name: String,
    #[serde(rename = "type")]
    output_type: OutputType,
    path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct OutputInput {
    from_task: String,
    output: String,
    #[serde(rename = "as")]
    argument: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
    #[serde(
        default,
        deserialize_with = "deserialize_retry_policy",
        skip_serializing_if = "Option::is_none"
    )]
    retry: Option<RetryPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    outputs: Vec<OutputDeclaration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<OutputInput>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct PlanV1 {
    schema_version: u32,
    tasks: Vec<PlanTaskV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct PlanTaskV1 {
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
    #[serde(default, deserialize_with = "deserialize_present_value")]
    retry: Option<Value>,
}

fn deserialize_present_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct PlanTaskV2 {
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
    #[serde(
        default,
        deserialize_with = "deserialize_retry_policy",
        skip_serializing_if = "Option::is_none"
    )]
    retry: Option<RetryPolicy>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct PlanV2 {
    schema_version: u32,
    tasks: Vec<PlanTaskV2>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct PlanV3 {
    schema_version: u32,
    tasks: Vec<PlanTask>,
}

impl From<PlanTaskV1> for PlanTask {
    fn from(task: PlanTaskV1) -> Self {
        Self {
            id: task.id,
            depends_on: task.depends_on,
            program: task.program,
            arguments: task.arguments,
            working_directory: task.working_directory,
            timeout_ms: task.timeout_ms,
            termination_grace_ms: task.termination_grace_ms,
            stdout_limit_bytes: task.stdout_limit_bytes,
            stderr_limit_bytes: task.stderr_limit_bytes,
            log_limit_bytes: task.log_limit_bytes,
            redact_argument_indexes: task.redact_argument_indexes,
            retry: None,
            outputs: Vec::new(),
            inputs: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for Plan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        decode_plan_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl From<PlanTaskV2> for PlanTask {
    fn from(task: PlanTaskV2) -> Self {
        Self {
            id: task.id,
            depends_on: task.depends_on,
            program: task.program,
            arguments: task.arguments,
            working_directory: task.working_directory,
            timeout_ms: task.timeout_ms,
            termination_grace_ms: task.termination_grace_ms,
            stdout_limit_bytes: task.stdout_limit_bytes,
            stderr_limit_bytes: task.stderr_limit_bytes,
            log_limit_bytes: task.log_limit_bytes,
            redact_argument_indexes: task.redact_argument_indexes,
            retry: task.retry,
            outputs: Vec::new(),
            inputs: Vec::new(),
        }
    }
}

fn decode_plan_value(value: Value) -> Result<Plan, String> {
    let schema_version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "schema_version must be an integer".to_owned())?;
    match schema_version {
        version if version == u64::from(LEGACY_PLAN_SCHEMA_VERSION) => {
            let plan: PlanV1 = serde_json::from_value(value).map_err(|error| {
                format!("invalid plan schema_version {LEGACY_PLAN_SCHEMA_VERSION}: {error}")
            })?;
            if plan.tasks.iter().any(|task| task.retry.is_some()) {
                return Err(format!(
                    "invalid plan schema_version {LEGACY_PLAN_SCHEMA_VERSION}: retry field is not allowed"
                ));
            }
            Ok(Plan {
                schema_version: plan.schema_version,
                tasks: plan.tasks.into_iter().map(Into::into).collect(),
            })
        }
        version if version == u64::from(PREVIOUS_PLAN_SCHEMA_VERSION) => {
            let plan: PlanV2 = serde_json::from_value(value)
                .map_err(|error| format!("invalid plan schema_version {version}: {error}"))?;
            Ok(Plan {
                schema_version: plan.schema_version,
                tasks: plan.tasks.into_iter().map(Into::into).collect(),
            })
        }
        version if version == u64::from(PLAN_SCHEMA_VERSION) => {
            let plan: PlanV3 = serde_json::from_value(value)
                .map_err(|error| format!("invalid plan schema_version {version}: {error}"))?;
            Ok(Plan {
                schema_version: plan.schema_version,
                tasks: plan.tasks,
            })
        }
        schema_version => Err(format!("unsupported plan schema_version {schema_version}")),
    }
}
pub(crate) fn resolve_plan_value(
    value: Value,
    invocation_directory: &Path,
) -> Result<Value, RunError> {
    let mut plan = serde_json::from_value::<Plan>(value).map_err(|error| {
        RunError::Plan(format!("could not decode resolved workflow plan: {error}"))
    })?;
    validate_plan(&plan, invocation_directory)?;
    resolve_working_directories(&mut plan, invocation_directory, None)?;
    validate_encoded_plan_size(&plan)?;
    serde_json::to_value(plan)
        .map_err(|error| RunError::Plan(format!("plan cannot be encoded: {error}")))
}
fn validate_encoded_plan_size(plan: &Plan) -> Result<(), RunError> {
    let bytes = serde_json::to_vec(plan)
        .map_err(|error| RunError::Plan(format!("plan cannot be encoded: {error}")))?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err(RunError::Plan(format!(
            "encoded plan exceeds {MAX_PLAN_BYTES} bytes"
        )));
    }
    Ok(())
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetryPolicy {
    max_attempts: u32,
    delay_ms: u64,
}
fn deserialize_retry_policy<'de, D>(deserializer: D) -> Result<Option<RetryPolicy>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(serde::de::Error::custom("retry must be an object"));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

pub(crate) const fn default_grace_ms() -> i64 {
    2_000
}
pub(crate) const fn default_output_limit() -> i64 {
    1 << 20
}
pub(crate) const fn default_log_limit() -> i64 {
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
    pub fn as_str(self) -> &'static str {
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
    pub fn as_str(self) -> &'static str {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<OutputRecord>>,
    pub attempts: Vec<AttemptRecord>,
    pub retry_ready_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Indeterminate,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptRecord {
    pub attempt: u32,
    pub run_id: String,
    pub supervisor_state_path: String,
    pub status: AttemptStatus,
    pub reason: Option<String>,
    pub summary: Option<ExecutionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<OutputRecord>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRecord {
    pub name: String,
    #[serde(rename = "type")]
    pub output_type: OutputType,
    pub path: String,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub value: Value,
    pub run_id: String,
    pub producer_task: String,
    pub attempt: u32,
    pub attempt_run_id: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCompositionMetadata {
    pub root_workflow: String,
    pub root_definition_sha256: String,
    pub instances: Vec<WorkflowCompositionInstance>,
    pub resolved_plan_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCompositionInstance {
    pub instance: String,
    pub workflow: String,
    pub schema_version: u32,
    pub definition_sha256: String,
    pub resolved_inputs: BTreeMap<String, Value>,
}

pub(crate) fn composition_tree(metadata: &WorkflowRunMetadata) -> Value {
    let root = composition_tree_node(metadata, "root", None);
    json!({"root": root})
}

fn composition_tree_node(
    metadata: &WorkflowRunMetadata,
    path: &str,
    instance: Option<&WorkflowCompositionInstance>,
) -> Value {
    let (workflow, schema_version, definition_sha256, resolved_inputs) = match instance {
        Some(instance) => (
            instance.workflow.as_str(),
            instance.schema_version,
            instance.definition_sha256.as_str(),
            &instance.resolved_inputs,
        ),
        None => (
            metadata.name.as_str(),
            metadata.schema_version,
            metadata
                .composition
                .as_ref()
                .map_or(metadata.definition_sha256.as_str(), |composition| {
                    composition.root_definition_sha256.as_str()
                }),
            &metadata.inputs,
        ),
    };
    let source_graph_sha256 = (instance.is_none() && metadata.composition.is_some())
        .then_some(metadata.definition_sha256.as_str());
    let children = metadata
        .composition
        .as_ref()
        .into_iter()
        .flat_map(|composition| composition.instances.iter())
        .filter(|candidate| {
            candidate
                .instance
                .rsplit_once('.')
                .map_or("root", |(parent, _)| parent)
                == path
        })
        .map(|candidate| composition_tree_node(metadata, &candidate.instance, Some(candidate)))
        .collect::<Vec<_>>();
    json!({
        "instance": path,
        "workflow": workflow,
        "schema_version": schema_version,
        "definition_sha256": definition_sha256,
        "source_graph_sha256": source_graph_sha256,
        "resolved_inputs": resolved_inputs,
        "children": children,
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunMetadata {
    pub name: String,
    pub schema_version: u32,
    pub definition_sha256: String,
    pub inputs: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<WorkflowCompositionMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_plan_sha256: Option<String>,
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
    pub jobs: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowRunMetadata>,
    plan: Plan,
    pub tasks: Vec<TaskRecord>,
}

#[derive(Debug, Default)]
enum JobsField {
    #[default]
    Missing,
    Null,
    Value(usize),
}

fn deserialize_jobs_field<'de, D>(deserializer: D) -> Result<JobsField, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<usize>::deserialize(deserializer)? {
        Some(jobs) => JobsField::Value(jobs),
        None => JobsField::Null,
    })
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunStateWire {
    schema_version: u32,
    run_id: String,
    state_path: String,
    created_at: String,
    updated_at: String,
    status: RunStatus,
    cancellation_requested: bool,
    #[serde(default, deserialize_with = "deserialize_jobs_field")]
    jobs: JobsField,
    #[serde(default)]
    workflow: Option<WorkflowRunMetadata>,
    plan: Plan,
    tasks: Vec<Value>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyTaskRecord {
    id: String,
    status: TaskStatus,
    reason: Option<String>,
    attempt_run_id: Option<String>,
    supervisor_state_path: Option<String>,
    summary: Option<ExecutionSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptRecordV3 {
    attempt: u32,
    run_id: String,
    supervisor_state_path: String,
    status: AttemptStatus,
    reason: Option<String>,
    summary: Option<ExecutionSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRecordV3 {
    id: String,
    status: TaskStatus,
    reason: Option<String>,
    attempt_run_id: Option<String>,
    supervisor_state_path: Option<String>,
    summary: Option<ExecutionSummary>,
    attempts: Vec<AttemptRecordV3>,
    retry_ready_at: Option<String>,
}

impl From<AttemptRecordV3> for AttemptRecord {
    fn from(attempt: AttemptRecordV3) -> Self {
        Self {
            attempt: attempt.attempt,
            run_id: attempt.run_id,
            supervisor_state_path: attempt.supervisor_state_path,
            status: attempt.status,
            reason: attempt.reason,
            summary: attempt.summary,
            outputs: None,
        }
    }
}

impl From<TaskRecordV3> for TaskRecord {
    fn from(task: TaskRecordV3) -> Self {
        Self {
            id: task.id,
            status: task.status,
            reason: task.reason,
            attempt_run_id: task.attempt_run_id,
            supervisor_state_path: task.supervisor_state_path,
            summary: task.summary,
            outputs: None,
            attempts: task.attempts.into_iter().map(Into::into).collect(),
            retry_ready_at: task.retry_ready_at,
        }
    }
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

struct TaskCompletion {
    index: usize,
    result: Result<supervise::SupervisedExecution, SupervisorError>,
}
fn receive_completion(
    receiver: &mpsc::Receiver<TaskCompletion>,
    wait: Option<std::time::Duration>,
) -> Result<Option<TaskCompletion>, ()> {
    match wait {
        Some(wait) => match receiver.recv_timeout(wait) {
            Ok(completion) => Ok(Some(completion)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
        },
        None => receiver.recv().map(Some).map_err(|_| ()),
    }
}

fn supervisor_error(state: &RunState, source: SupervisorError) -> RunError {
    RunError::Supervisor {
        run_id: state.run_id.clone(),
        state_path: state.state_path.clone(),
        state: Box::new(state.clone()),
        source,
    }
}

fn reset_task_to_pending(task: &mut TaskRecord) {
    task.status = TaskStatus::Pending;
    task.reason = None;
    task.attempt_run_id = None;
    task.supervisor_state_path = None;
    task.summary = None;
    task.outputs = None;
    task.retry_ready_at = None;
}

fn rollback_unlaunched_attempt(task: &mut TaskRecord) {
    if task
        .attempts
        .last()
        .is_some_and(|attempt| attempt.status == AttemptStatus::Running)
    {
        task.attempts.pop();
    }
    reset_task_to_pending(task);
    if !task.attempts.is_empty() {
        task.retry_ready_at = Some(now());
    }
}

fn mark_task_indeterminate(task: &mut TaskRecord, reason: String) {
    task.status = TaskStatus::Indeterminate;
    task.reason = Some(reason.clone());
    task.summary = None;
    task.outputs = None;
    task.retry_ready_at = None;
    if let Some(attempt) = task.attempts.last_mut()
        && attempt.status == AttemptStatus::Running
    {
        attempt.status = AttemptStatus::Indeterminate;
        attempt.reason = Some(reason);
    }
}

fn complete_attempt(
    task: &mut TaskRecord,
    status: AttemptStatus,
    reason: Option<String>,
    summary: Option<ExecutionSummary>,
    outputs: Option<Vec<OutputRecord>>,
) -> bool {
    let Some(attempt) = task.attempts.last_mut() else {
        return false;
    };
    if attempt.status != AttemptStatus::Running {
        return false;
    }
    attempt.status = status;
    attempt.reason = reason;
    attempt.summary = summary;
    attempt.outputs = outputs;
    true
}

fn execution_was_cancelled(execution: &ExecutionResult) -> bool {
    execution.lifecycle == Lifecycle::Cancelled
        || matches!(
            execution.termination_reason,
            TerminationReason::CancelledTerminated | TerminationReason::CancelledKilled
        )
        || execution.cancellation.requested
}

fn execution_summary_is_retryable_failure(summary: &ExecutionSummary) -> bool {
    !summary.cancellation_requested
        && matches!(
            (summary.lifecycle, summary.termination_reason),
            (
                Lifecycle::Completed,
                TerminationReason::ExitNonzero | TerminationReason::DescendantsTerminated
            ) | (Lifecycle::FailedToStart, TerminationReason::StartError)
                | (Lifecycle::TimedOut, TerminationReason::TimeoutTerminated)
                | (
                    Lifecycle::Killed,
                    TerminationReason::TimeoutKilled | TerminationReason::DescendantsKilled
                )
        )
}

fn execution_is_retryable_failure(execution: &ExecutionResult, summary: &ExecutionSummary) -> bool {
    !execution.task_succeeded()
        && !execution_was_cancelled(execution)
        && execution_summary_is_retryable_failure(summary)
}

fn retry_ready_at(delay_ms: u64) -> String {
    (Utc::now() + chrono::Duration::milliseconds(delay_ms as i64)).to_rfc3339()
}

fn record_shutdown_failure(
    cleanup_source: &mut Option<SupervisorError>,
    uncertain_tasks: &mut HashSet<usize>,
    state: &mut RunState,
    index: Option<usize>,
    source: SupervisorError,
) {
    if let Some(index) = index.filter(|&index| index < state.tasks.len()) {
        uncertain_tasks.insert(index);
        mark_task_indeterminate(
            &mut state.tasks[index],
            truncate(&source.to_string(), MAX_SUMMARY_MESSAGE_BYTES),
        );
    }
    if cleanup_source.is_none() {
        *cleanup_source = Some(source);
    }
}

fn queue_shutdown_completion(
    completion: TaskCompletion,
    active: &HashMap<usize, thread::JoinHandle<()>>,
    pending: &mut HashMap<usize, TaskCompletion>,
    cleanup_source: &mut Option<SupervisorError>,
    uncertain_tasks: &mut HashSet<usize>,
    state: &mut RunState,
) {
    let index = completion.index;
    if !active.contains_key(&index) {
        record_shutdown_failure(
            cleanup_source,
            uncertain_tasks,
            state,
            Some(index),
            SupervisorError::Transport(io::Error::other(
                "task completion did not match an active worker during shutdown",
            )),
        );
    }
    if let Entry::Vacant(entry) = pending.entry(index) {
        entry.insert(completion);
    } else {
        record_shutdown_failure(
            cleanup_source,
            uncertain_tasks,
            state,
            Some(index),
            SupervisorError::Transport(io::Error::other(
                "duplicate task completion during shutdown",
            )),
        );
    }
}

fn apply_completion(
    plan: &Plan,
    state: &mut RunState,
    completion: TaskCompletion,
    coordinator_cancelled: bool,
    project_root: &Path,
) -> Option<SupervisorError> {
    if completion.index >= state.tasks.len() {
        return Some(SupervisorError::Transport(io::Error::other(
            "task completion index is out of range",
        )));
    }
    if coordinator_cancelled {
        state.cancellation_requested = true;
    }
    match completion.result {
        Ok(result) => {
            let execution = result.execution;
            if !state.tasks[completion.index]
                .attempts
                .last()
                .is_some_and(|attempt| {
                    attempt.status == AttemptStatus::Running && attempt.run_id == execution.run_id
                })
            {
                let source = SupervisorError::Transport(io::Error::other(
                    "task completion did not match its attempt identity",
                ));
                mark_task_indeterminate(
                    &mut state.tasks[completion.index],
                    truncate(&source.to_string(), MAX_SUMMARY_MESSAGE_BYTES),
                );
                return Some(source);
            }
            let execution_cancelled = execution_was_cancelled(&execution);
            let execution_succeeded = execution.task_succeeded();
            let (outputs, output_error) =
                if execution_succeeded && !plan.tasks[completion.index].outputs.is_empty() {
                    match state.tasks[completion.index].attempts.last() {
                        Some(attempt) => {
                            match finalize_outputs(
                                project_root,
                                &state.run_id,
                                &plan.tasks[completion.index],
                                attempt,
                            ) {
                                Ok(outputs) => (Some(outputs), None),
                                Err(error) => (None, Some(error)),
                            }
                        }
                        None => (
                            None,
                            Some("successful completion has no attempt record".into()),
                        ),
                    }
                } else {
                    (None, None)
                };
            let succeeded = execution_succeeded && output_error.is_none();
            let summary = summarize(&execution);
            let reason = if let Some(error) = output_error.as_deref() {
                Some(truncate(
                    &format!("output finalization refused: {error}"),
                    MAX_SUMMARY_MESSAGE_BYTES,
                ))
            } else if !execution_succeeded {
                result_reason(&execution)
            } else {
                None
            };
            let attempt_status = if output_error.is_some() {
                AttemptStatus::Indeterminate
            } else if succeeded {
                AttemptStatus::Succeeded
            } else if execution_cancelled {
                AttemptStatus::Cancelled
            } else {
                AttemptStatus::Failed
            };
            if !complete_attempt(
                &mut state.tasks[completion.index],
                attempt_status,
                reason.clone(),
                (attempt_status != AttemptStatus::Indeterminate).then(|| summary.clone()),
                outputs.clone(),
            ) {
                let source = SupervisorError::Transport(io::Error::other(
                    "task completion did not match a running attempt",
                ));
                mark_task_indeterminate(
                    &mut state.tasks[completion.index],
                    truncate(&source.to_string(), MAX_SUMMARY_MESSAGE_BYTES),
                );
                return Some(source);
            }
            let retry_delay_ms = if !succeeded && !execution_cancelled {
                plan.tasks[completion.index]
                    .retry
                    .as_ref()
                    .and_then(|policy| {
                        (state.tasks[completion.index].attempts.len()
                            < policy.max_attempts as usize)
                            .then_some(policy.delay_ms)
                    })
            } else {
                None
            };
            let retryable_failure =
                retry_delay_ms.filter(|_| execution_is_retryable_failure(&execution, &summary));
            let task = &mut state.tasks[completion.index];
            task.retry_ready_at = None;
            task.summary = Some(summary);
            task.outputs = outputs.clone();
            if succeeded {
                task.status = TaskStatus::Succeeded;
                task.reason = None;
            } else if output_error.is_some() {
                task.status = TaskStatus::Indeterminate;
                task.reason = reason.clone();
                task.summary = None;
                task.outputs = None;
            } else if execution_cancelled {
                task.status = TaskStatus::Cancelled;
                task.reason = reason;
            } else if let Some(delay_ms) = retryable_failure {
                task.status = TaskStatus::Pending;
                task.reason = None;
                task.attempt_run_id = None;
                task.supervisor_state_path = None;
                task.summary = None;
                task.outputs = None;
                task.retry_ready_at = Some(retry_ready_at(delay_ms));
            } else {
                task.status = TaskStatus::Failed;
                task.reason = reason;
            }
            mark_blocked(plan, state);
            if execution_cancelled {
                state.cancellation_requested = true;
            }
            None
        }
        Err(SupervisorError::CancelledBeforeLaunch) => {
            rollback_unlaunched_attempt(&mut state.tasks[completion.index]);
            state.cancellation_requested = true;
            None
        }
        Err(source) => {
            mark_task_indeterminate(
                &mut state.tasks[completion.index],
                truncate(&source.to_string(), MAX_SUMMARY_MESSAGE_BYTES),
            );
            mark_blocked(plan, state);
            state.status = RunStatus::Indeterminate;
            Some(source)
        }
    }
}

struct ShutdownContext<'a> {
    coordinator: &'a supervise::CoordinatorSignals,
    active: &'a mut HashMap<usize, thread::JoinHandle<()>>,
    completed_receiver: &'a mpsc::Receiver<TaskCompletion>,
    plan: &'a Plan,
    state: &'a mut RunState,
    store: &'a mut StateStore,
}

fn shutdown_and_drain(
    original_error: RunError,
    context: ShutdownContext<'_>,
    unlaunched_index: Option<usize>,
) -> RunError {
    let ShutdownContext {
        coordinator,
        active,
        completed_receiver,
        plan,
        state,
        store,
    } = context;
    if let Some(index) = unlaunched_index {
        rollback_unlaunched_attempt(&mut state.tasks[index]);
    }
    coordinator.request_cancellation();
    state.cancellation_requested = true;
    let mut cleanup_source = None;
    let mut pending = HashMap::<usize, TaskCompletion>::new();
    let mut uncertain_tasks = HashSet::new();
    let mut channel_closed = false;

    while !active.is_empty() {
        while !channel_closed {
            match completed_receiver.try_recv() {
                Ok(completion) => queue_shutdown_completion(
                    completion,
                    active,
                    &mut pending,
                    &mut cleanup_source,
                    &mut uncertain_tasks,
                    state,
                ),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    channel_closed = true;
                    record_shutdown_failure(
                        &mut cleanup_source,
                        &mut uncertain_tasks,
                        state,
                        None,
                        SupervisorError::Transport(io::Error::other(
                            "task completion channel closed during shutdown",
                        )),
                    );
                }
            }
        }

        let index = active
            .keys()
            .find(|index| pending.contains_key(index))
            .copied()
            .or_else(|| {
                active
                    .iter()
                    .find(|(_, worker)| worker.is_finished())
                    .map(|(index, _)| *index)
            });
        let Some(index) = index else {
            if channel_closed {
                break;
            }
            match completed_receiver.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(completion) => queue_shutdown_completion(
                    completion,
                    active,
                    &mut pending,
                    &mut cleanup_source,
                    &mut uncertain_tasks,
                    state,
                ),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    channel_closed = true;
                    record_shutdown_failure(
                        &mut cleanup_source,
                        &mut uncertain_tasks,
                        state,
                        None,
                        SupervisorError::Transport(io::Error::other(
                            "task completion channel closed during shutdown",
                        )),
                    );
                }
            }
            continue;
        };

        let Some(worker) = active.remove(&index) else {
            record_shutdown_failure(
                &mut cleanup_source,
                &mut uncertain_tasks,
                state,
                Some(index),
                SupervisorError::Transport(io::Error::other(
                    "active worker disappeared during shutdown",
                )),
            );
            break;
        };
        let mut completion = pending.remove(&index);
        let worker_panicked = worker.join().is_err();
        loop {
            match completed_receiver.try_recv() {
                Ok(candidate) if candidate.index == index => {
                    if completion.is_none() {
                        completion = Some(candidate);
                    } else {
                        record_shutdown_failure(
                            &mut cleanup_source,
                            &mut uncertain_tasks,
                            state,
                            Some(index),
                            SupervisorError::Transport(io::Error::other(
                                "duplicate task completion during shutdown",
                            )),
                        );
                    }
                }
                Ok(candidate) => queue_shutdown_completion(
                    candidate,
                    active,
                    &mut pending,
                    &mut cleanup_source,
                    &mut uncertain_tasks,
                    state,
                ),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    channel_closed = true;
                    if !worker_panicked && completion.is_none() {
                        record_shutdown_failure(
                            &mut cleanup_source,
                            &mut uncertain_tasks,
                            state,
                            None,
                            SupervisorError::Transport(io::Error::other(
                                "task completion channel closed during shutdown",
                            )),
                        );
                    }
                    break;
                }
            }
        }
        match completion {
            Some(completion) if !uncertain_tasks.contains(&index) => {
                if let Some(source) = apply_completion(
                    plan,
                    state,
                    completion,
                    true,
                    &project_root_for_state_path(&state.state_path),
                ) {
                    record_shutdown_failure(
                        &mut cleanup_source,
                        &mut uncertain_tasks,
                        state,
                        Some(index),
                        source,
                    );
                }
                if worker_panicked {
                    record_shutdown_failure(
                        &mut cleanup_source,
                        &mut uncertain_tasks,
                        state,
                        Some(index),
                        SupervisorError::Transport(io::Error::other(
                            "supervisor worker panicked while shutting down",
                        )),
                    );
                }
            }
            Some(_) => {}
            None => {
                let source = SupervisorError::Transport(io::Error::other(if worker_panicked {
                    "supervisor worker panicked while shutting down"
                } else {
                    "task worker finished without a completion"
                }));
                record_shutdown_failure(
                    &mut cleanup_source,
                    &mut uncertain_tasks,
                    state,
                    Some(index),
                    source,
                );
            }
        }
    }

    let remaining = active.drain().collect::<Vec<_>>();
    for (index, worker) in remaining {
        let worker_panicked = worker.join().is_err();
        if worker_panicked || state.tasks[index].status == TaskStatus::Running {
            let source = SupervisorError::Transport(io::Error::other(if worker_panicked {
                "supervisor worker panicked while shutting down"
            } else {
                "task worker finished without a completion"
            }));
            record_shutdown_failure(
                &mut cleanup_source,
                &mut uncertain_tasks,
                state,
                Some(index),
                source,
            );
        }
    }

    if cleanup_source.is_some() {
        for task in &mut state.tasks {
            if task.status == TaskStatus::Running {
                mark_task_indeterminate(
                    task,
                    "coordinator shutdown could not establish task completion".into(),
                );
            }
        }
    }
    for index in uncertain_tasks {
        if let Some(task) = state.tasks.get_mut(index)
            && task.status != TaskStatus::Indeterminate
        {
            mark_task_indeterminate(
                task,
                "coordinator shutdown could not establish task completion".into(),
            );
        }
    }
    mark_blocked(plan, state);
    state.status = derive_status(&state.tasks, true);
    let persist_error = persist_at(store, state, "shutdown").err();
    match (original_error, persist_error) {
        (RunError::Supervisor { source, .. }, None) => supervisor_error(state, source),
        (error, None) => error,
        (error, Some(persist_error)) => RunError::State(format!(
            "{error}; could not persist shutdown state: {persist_error}"
        )),
    }
}

fn reconcile_running_attempts(
    args: &RunArgs,
    plan: &Plan,
    state: &mut RunState,
    store: &mut StateStore,
    invocation_directory: &Path,
    project_root: &Path,
    coordinator_cancelled: bool,
) -> Result<Vec<ReconciliationReport>, RunError> {
    let indexes: Vec<usize> = state
        .tasks
        .iter()
        .enumerate()
        .filter_map(|(index, task)| {
            matches!(task.status, TaskStatus::Running | TaskStatus::Indeterminate).then_some(index)
        })
        .collect();
    if indexes.is_empty() {
        return Ok(Vec::new());
    }

    let mut reports = Vec::with_capacity(indexes.len());
    let mut terminal = Vec::new();
    let mut uncertain = Vec::new();
    let mut blocked = false;
    for index in indexes {
        let task = &state.tasks[index];
        let attempt = task
            .attempts
            .last()
            .expect("validated unsafe task has an attempt");
        let attempt_run_id = attempt.run_id.clone();
        let attempt_state_path = PathBuf::from(&attempt.supervisor_state_path);
        if task.status == TaskStatus::Indeterminate {
            blocked = true;
            reports.push(ReconciliationReport {
                task_id: task.id.clone(),
                attempt_run_id,
                classification: ReconciliationClassification::Refused,
                reason: "attempt is already indeterminate; no automatic retry is allowed".into(),
            });
            continue;
        }
        let arguments = resolve_task_arguments(plan, state, index, project_root)?;
        let supervise_args = task_supervise_args(
            &args.supervisor,
            &plan.tasks[index],
            attempt_run_id.clone(),
            attempt_state_path.clone(),
            &arguments,
            project_root,
        )?;
        match supervise::inspect_attempt(&supervise_args, invocation_directory) {
            supervise::AttemptInspection::Completed(execution) => {
                let execution = *execution;
                let classification = if execution.task_succeeded() {
                    ReconciliationClassification::Completed
                } else if execution_was_cancelled(&execution) {
                    ReconciliationClassification::Cancelled
                } else {
                    ReconciliationClassification::Failed
                };
                let reason = format!(
                    "trusted supervisor result: {}",
                    termination_reason_name(execution.termination_reason)
                );
                terminal.push((
                    index,
                    attempt_run_id,
                    attempt_state_path,
                    execution,
                    classification,
                    reason,
                ));
            }
            supervise::AttemptInspection::StillRunning => {
                blocked = true;
                reports.push(ReconciliationReport {
                    task_id: task.id.clone(),
                    attempt_run_id,
                    classification: ReconciliationClassification::StillRunning,
                    reason: "attempt still active; resume cannot continue this branch yet".into(),
                });
            }
            supervise::AttemptInspection::Indeterminate(reason) => {
                blocked = true;
                uncertain.push((index, truncate(&reason, MAX_SUMMARY_MESSAGE_BYTES)));
                reports.push(ReconciliationReport {
                    task_id: task.id.clone(),
                    attempt_run_id,
                    classification: ReconciliationClassification::Indeterminate,
                    reason: truncate(&reason, MAX_SUMMARY_MESSAGE_BYTES),
                });
            }
        }
    }

    for (index, attempt_run_id, attempt_state_path, execution, classification, reason) in terminal {
        let state_path = path_string(&attempt_state_path)?;
        let completion = TaskCompletion {
            index,
            result: Ok(supervise::SupervisedExecution {
                state_path,
                execution,
            }),
        };
        if let Some(source) =
            apply_completion(plan, state, completion, coordinator_cancelled, project_root)
        {
            blocked = true;
            let reason = truncate(&source.to_string(), MAX_SUMMARY_MESSAGE_BYTES);
            mark_task_indeterminate(&mut state.tasks[index], reason.clone());
            reports.push(ReconciliationReport {
                task_id: state.tasks[index].id.clone(),
                attempt_run_id,
                classification: ReconciliationClassification::Indeterminate,
                reason,
            });
        } else {
            reports.push(ReconciliationReport {
                task_id: state.tasks[index].id.clone(),
                attempt_run_id,
                classification,
                reason,
            });
        }
    }
    for (index, reason) in uncertain {
        mark_task_indeterminate(&mut state.tasks[index], reason);
    }
    if coordinator_cancelled {
        state.cancellation_requested = true;
    }
    mark_blocked(plan, state);
    state.status = derive_status(&state.tasks, state.cancellation_requested);
    persist_at(store, state, "reconciliation")?;
    if blocked {
        let message = if reports
            .iter()
            .any(|report| report.classification == ReconciliationClassification::StillRunning)
        {
            "attempt still active; resume cannot continue this branch yet"
        } else {
            "unresolved attempt evidence remains indeterminate; resume is blocked"
        };
        return Err(RunError::Reconciliation {
            message: message.into(),
            state: Box::new(state.clone()),
            reports,
        });
    }
    Ok(reports)
}

fn explain_task_source(workflow: Option<&WorkflowRunMetadata>, task_id: &str) -> Value {
    let Some(workflow) = workflow else {
        return Value::Null;
    };
    let instance = workflow
        .composition
        .as_ref()
        .into_iter()
        .flat_map(|composition| composition.instances.iter())
        .filter(|candidate| {
            let prefix = format!("{}.", candidate.instance);
            task_id.starts_with(&prefix)
        })
        .max_by_key(|candidate| candidate.instance.len());
    let (instance_path, source_task, definition_sha256) = match instance {
        Some(instance) => {
            let prefix = format!("{}.", instance.instance);
            (
                instance.instance.as_str(),
                task_id.strip_prefix(&prefix).unwrap_or(task_id),
                instance.definition_sha256.as_str(),
            )
        }
        None => (
            "root",
            task_id,
            workflow
                .composition
                .as_ref()
                .map_or(workflow.definition_sha256.as_str(), |composition| {
                    composition.root_definition_sha256.as_str()
                }),
        ),
    };
    json!({
        "workflow": instance.map_or(workflow.name.as_str(), |value| value.workflow.as_str()),
        "instance": instance_path,
        "source_task": source_task,
        "definition_sha256": definition_sha256,
    })
}
fn redact_explanation_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            if secrets
                .iter()
                .any(|secret| !secret.is_empty() && text.contains(secret))
            {
                *text = "<redacted>".into();
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_explanation_value(value, secrets);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_explanation_value(value, secrets);
            }
        }
        Value::Number(_) | Value::Bool(_) if secrets.contains(&value.to_string()) => {
            *value = json!("<redacted>");
        }
        Value::Null | Value::Number(_) | Value::Bool(_) => {}
    }
}

fn redact_explanation_text(text: String, secrets: &[String]) -> String {
    if secrets
        .iter()
        .any(|secret| !secret.is_empty() && text.contains(secret))
    {
        "<redacted>".into()
    } else {
        text
    }
}

fn resolve_run_selector(selector: &Path, project_root: &Path) -> Result<PathBuf, RunError> {
    let Some(id) = selector
        .to_str()
        .filter(|value| value.parse::<Uuid>().is_ok())
        .map(str::to_owned)
    else {
        return Ok(if selector.is_absolute() {
            selector.to_owned()
        } else {
            project_root.join(selector)
        });
    };
    let runs = project_root.join(".relay/runs");
    let default = runs.join(format!("{id}.json"));
    match fs::symlink_metadata(&default) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            return Ok(default);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RunError::State(format!(
                "could not inspect {}: {error}",
                default.display()
            )));
        }
    }
    if fs::symlink_metadata(&runs)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Ok(default);
    }
    let entries = match fs::read_dir(&runs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(default),
        Err(error) => {
            return Err(RunError::State(format!(
                "could not inspect {}: {error}",
                runs.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| RunError::State(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| RunError::State(error.to_string()))?
            .is_file()
        {
            continue;
        }
        let candidate = entry.path();
        let Ok((canonical, bytes)) = read_handoff_state(&candidate) else {
            continue;
        };
        let Ok(state) = decode_run_state(&bytes, &canonical) else {
            continue;
        };
        if state.run_id == id {
            return Ok(candidate);
        }
    }
    Ok(default)
}

pub fn explain(selector: &Path, project_root: &Path) -> Result<RunOutput, RunError> {
    let path = resolve_run_selector(selector, project_root)?;
    validate_state_path(&path, project_root)?;
    let project =
        fs::canonicalize(project_root).map_err(|error| RunError::State(error.to_string()))?;
    let (canonical_path, bytes) = read_handoff_state(&path)?;
    if !canonical_path.starts_with(&project) {
        return Err(RunError::State(
            "run explanation requires a project-local state path".into(),
        ));
    }
    let state = decode_run_state(&bytes, &canonical_path)?;
    validate_encoded_plan_size(&state.plan)?;
    validate_plan(&state.plan, project_root)?;
    validate_historical_state(&state, &state.plan, &canonical_path, project_root)?;
    let redacted = state
        .plan
        .tasks
        .iter()
        .any(|task| !task.redact_argument_indexes.is_empty());
    let mut redaction_secrets = Vec::new();
    if redacted && let Some(workflow) = state.workflow.as_ref() {
        redaction_secrets.extend(
            workflow
                .inputs
                .values()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        );
        if let Some(composition) = workflow.composition.as_ref() {
            redaction_secrets.extend(
                composition
                    .instances
                    .iter()
                    .flat_map(|instance| instance.resolved_inputs.values())
                    .filter_map(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    let mut workflow = json!(state.workflow);
    if redacted {
        if let Some(inputs) = workflow.get_mut("inputs").and_then(Value::as_object_mut) {
            inputs
                .values_mut()
                .for_each(|value| *value = json!("<redacted>"));
        }
        if let Some(instances) = workflow
            .get_mut("composition")
            .and_then(|c| c.get_mut("instances"))
            .and_then(Value::as_array_mut)
        {
            for instance in instances {
                if let Some(inputs) = instance["resolved_inputs"].as_object_mut() {
                    inputs
                        .values_mut()
                        .for_each(|value| *value = json!("<redacted>"));
                }
            }
        }
    }
    let reconciliation_status = match state.status {
        RunStatus::Indeterminate => "indeterminate",
        RunStatus::Running => "requires_live_reconciliation",
        _ => "no_unresolved_attempts",
    };
    let workflow_name = workflow
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("direct plan");
    let definition_sha256 = workflow.get("definition_sha256").and_then(Value::as_str);
    let resolved_plan_sha256 = workflow.get("resolved_plan_sha256").and_then(Value::as_str);
    let composition_instances = workflow
        .get("composition")
        .and_then(|value| value.get("instances"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut human = format!(
        "Run: {}\nStatus: {}\nWorkflow: {}\nReconciliation: {} (persisted evidence only; reconciliation history is not recorded)",
        state.run_id,
        state.status.as_str(),
        workflow_name,
        reconciliation_status
    );
    human.push_str(&format!(
        "\nDefinition: {}\nResolved plan: {}\nComposition instances: {}",
        definition_sha256.unwrap_or("unavailable"),
        resolved_plan_sha256.unwrap_or("unavailable"),
        composition_instances
    ));
    let mut tasks = Vec::with_capacity(state.tasks.len());
    for (task, plan_task) in state.tasks.iter().zip(&state.plan.tasks) {
        let maximum = plan_task
            .retry
            .as_ref()
            .map_or(1, |retry| retry.max_attempts);
        let retryable = task
            .summary
            .as_ref()
            .is_some_and(execution_summary_is_retryable_failure);
        let retry = if task.status == TaskStatus::Indeterminate {
            "refused_indeterminate"
        } else if task.status == TaskStatus::Succeeded {
            if task.attempts.len() > 1 {
                "succeeded_after_retry"
            } else {
                "not_needed"
            }
        } else if task.retry_ready_at.is_some() {
            "scheduled"
        } else if plan_task.retry.is_none() {
            "disabled"
        } else if task.status == TaskStatus::Failed
            && retryable
            && task.attempts.len() >= maximum as usize
        {
            "exhausted"
        } else if task.status == TaskStatus::Failed || task.status == TaskStatus::Cancelled {
            "not_retryable"
        } else {
            "available"
        };
        let latest_reason = task
            .summary
            .as_ref()
            .and_then(|summary| {
                summary
                    .exit_code
                    .filter(|code| *code != 0)
                    .map(|code| format!("exit code {code}"))
            })
            .or_else(|| {
                task.summary
                    .as_ref()
                    .and_then(|summary| summary.failure_message.clone())
            })
            .or_else(|| task.reason.clone())
            .map(|reason| redact_explanation_text(reason, &redaction_secrets));
        let mut dependencies: Vec<_> = plan_task
            .depends_on
            .iter()
            .map(|id| {
                let dependency = state
                    .tasks
                    .iter()
                    .find(|t| &t.id == id)
                    .expect("validated dependency");
                json!({"task": id, "status": dependency.status, "reason": dependency.reason})
            })
            .collect();
        for dependency in &mut dependencies {
            redact_explanation_value(dependency, &redaction_secrets);
        }
        let blocking: Vec<_> = dependencies
            .iter()
            .filter(|dependency| dependency["status"] != "succeeded")
            .cloned()
            .collect();
        let source = explain_task_source(state.workflow.as_ref(), &task.id);
        let mut outputs = json!(task.outputs.as_deref().unwrap_or_default());
        let mut attempts = json!(task.attempts);
        for output in outputs.as_array_mut().expect("validated output array") {
            output["aggregate_run_id"] = json!(state.run_id);
        }
        for attempt in attempts.as_array_mut().expect("validated attempt array") {
            attempt["attempt_number"] = attempt["attempt"].clone();
            attempt["attempt_run_id"] = attempt["run_id"].clone();
            attempt["aggregate_run_id"] = json!(state.run_id);
        }
        redact_explanation_value(&mut outputs, &redaction_secrets);
        redact_explanation_value(&mut attempts, &redaction_secrets);
        if redacted {
            // Persisted argv cannot identify the original secret. Withhold payload
            // values, not producer identity, rather than guess which are safe.
            for output in outputs.as_array_mut().unwrap() {
                output["value"] = json!("<redacted>");
            }
            for attempt in attempts.as_array_mut().unwrap() {
                if let Some(outputs) = attempt.get_mut("outputs").and_then(Value::as_array_mut) {
                    for output in outputs {
                        output["value"] = json!("<redacted>");
                    }
                }
            }
        }
        let reconciliation = match task.status {
            TaskStatus::Indeterminate => "indeterminate_no_automatic_retry",
            TaskStatus::Running => "requires_live_reconciliation",
            _ => "no_unresolved_attempt",
        };
        let mut reason = json!(task.reason);
        redact_explanation_value(&mut reason, &redaction_secrets);
        human.push_str(&format!("\n\n{}\nStatus: {}\nDependencies: {}\nAttempts: {} / {}\nRetry: {}\nReconciliation: {}", task.id, task.status.as_str(), json!(plan_task.depends_on), task.attempts.len(), maximum, retry, reconciliation));
        if let Some(reason) = &latest_reason {
            human.push_str(&format!("\nLatest reason: {}", json!(reason)));
        }
        if task.status == TaskStatus::Blocked {
            for dependency in &blocking {
                human.push_str(&format!(
                    "\nReason: dependency \x60{}\x60 {}",
                    dependency["task"].as_str().unwrap(),
                    dependency["status"].as_str().unwrap()
                ));
            }
        }
        for attempt in attempts.as_array().unwrap() {
            human.push_str(&format!(
                "\n  Attempt {}: {} (attempt run {}, aggregate run {})",
                attempt["attempt_number"],
                attempt["status"],
                attempt["attempt_run_id"],
                attempt["aggregate_run_id"]
            ));
        }
        human.push_str(&format!(
            "\nTrusted outputs (persisted provenance; files not revalidated): {}",
            outputs
        ));
        tasks.push(json!({
            "id": task.id,
            "status": task.status,
            "source": source,
            "aggregate_run_id": state.run_id,
            "depends_on": plan_task.depends_on,
            "dependencies": dependencies,
            "blocking_dependencies": blocking,
            "reason": reason,
            "latest_reason": latest_reason,
            "attempts": attempts,
            "attempts_used": task.attempts.len(),
            "max_attempts": maximum,
            "retry": {
                "status": retry,
                "delay_ms": plan_task.retry.as_ref().map_or(0, |r| r.delay_ms),
                "ready_at": task.retry_ready_at,
            },
            "reconciliation": reconciliation,
            "trusted_outputs": outputs,
            "output_inputs": plan_task.inputs,
        }));
    }
    Ok(RunOutput {
        human,
        data: json!({
            "run": {
                "run_id": state.run_id,
                "aggregate_run_id": state.run_id,
                "status": state.status,
                "state_path": state.state_path,
                "workflow": workflow.clone(),
                "resolved_plan_sha256": workflow
                    .get("resolved_plan_sha256")
                    .cloned()
                    .unwrap_or(Value::Null),
                "jobs": state.jobs,
                "created_at": state.created_at,
                "updated_at": state.updated_at,
                "cancellation_requested": state.cancellation_requested,
            },
            "tasks": tasks,
            "reconciliation": {
                "status": reconciliation_status,
                "mode": "persisted_only",
                "history": "not_recorded",
            },
            "output_verification": "persisted_provenance_only",
        }),
    })
}

pub fn run(
    args: RunArgs,
    invocation_directory: &Path,
    project_root: &Path,
) -> Result<RunResult, RunError> {
    let plan_path = args
        .plan
        .as_deref()
        .ok_or_else(|| RunError::Plan("a plan is required".into()))?;
    let raw_plan = read_bounded(plan_path, MAX_PLAN_BYTES).map_err(|error| {
        RunError::Plan(format!("could not read {}: {error}", plan_path.display()))
    })?;
    let plan: Plan = serde_json::from_slice(&raw_plan).map_err(|error| {
        RunError::Plan(format!("could not decode {}: {error}", plan_path.display()))
    })?;
    run_plan(
        args,
        invocation_directory,
        project_root,
        Some(plan),
        None,
        None,
    )
}

pub fn run_resolved(
    args: RunArgs,
    invocation_directory: &Path,
    project_root: &Path,
    value: Value,
    workflow: WorkflowRunMetadata,
) -> Result<RunResult, RunError> {
    let plan: Plan = serde_json::from_value(value).map_err(|error| {
        RunError::Plan(format!("could not decode resolved workflow plan: {error}"))
    })?;
    run_plan(
        args,
        invocation_directory,
        project_root,
        Some(plan),
        Some(workflow),
        None,
    )
}

pub fn run_workflow_resume(
    args: RunArgs,
    invocation_directory: &Path,
    project_root: &Path,
    workflow_name: String,
    current_definition_sha256: Option<String>,
) -> Result<RunResult, RunError> {
    if args.resume.is_none() {
        return Err(RunError::State(
            "workflow resume requires an aggregate state path".into(),
        ));
    }
    run_plan(
        args,
        invocation_directory,
        project_root,
        None,
        None,
        Some(WorkflowResumeRequest {
            name: workflow_name,
            current_definition_sha256,
        }),
    )
}
pub fn read_workflow_resume_metadata(
    path: &Path,
    project_root: &Path,
) -> Result<WorkflowRunMetadata, RunError> {
    validate_state_path(path, project_root)?;
    let mut store = StateStore::open(path, true)?;
    let state = store.read()?;
    let metadata = state
        .workflow
        .ok_or_else(|| RunError::State("aggregate state is not a workflow run".into()))?;
    validate_workflow_metadata(&metadata)?;
    Ok(metadata)
}

pub fn read_handoff_run(path: &Path, project_root: &Path) -> Result<RunState, RunError> {
    validate_handoff_state_path(path, project_root)?;
    let (canonical_path, bytes) = read_handoff_state(path)?;
    let state = decode_run_state(&bytes, &canonical_path)?;
    let plan = state.plan.clone();
    validate_encoded_plan_size(&plan)?;
    validate_plan(&plan, project_root)?;
    validate_resume_state(&state, &plan, &canonical_path, project_root, false)?;
    Ok(state)
}

struct WorkflowResumeRequest {
    name: String,
    current_definition_sha256: Option<String>,
}

fn run_plan(
    args: RunArgs,
    invocation_directory: &Path,
    project_root: &Path,
    mut supplied_plan: Option<Plan>,
    workflow_metadata: Option<WorkflowRunMetadata>,
    workflow_resume: Option<WorkflowResumeRequest>,
) -> Result<RunResult, RunError> {
    let resuming = args.resume.is_some();
    if !resuming && supplied_plan.is_none() {
        return Err(RunError::Plan(
            "a resolved plan is required for a new run".into(),
        ));
    }
    if resuming && supplied_plan.is_none() && workflow_resume.is_none() {
        return Err(RunError::State(
            "a plan is required when resuming directly".into(),
        ));
    }
    if resuming && workflow_metadata.is_some() {
        return Err(RunError::State(
            "workflow metadata cannot be supplied while resuming".into(),
        ));
    }
    if args.jobs == 0 {
        return Err(RunError::Plan("jobs must be a positive integer".into()));
    }
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
    if resuming
        && workflow_resume.is_none()
        && let Some(plan) = supplied_plan.as_ref()
    {
        reject_redacted_resume(plan)?;
    }
    if !resuming && let Some(workflow) = workflow_metadata.as_ref() {
        validate_workflow_metadata(workflow)?;
    }
    if let Some(plan) = supplied_plan.as_ref()
        && (!resuming || workflow_resume.is_none())
    {
        validate_plan(plan, invocation_directory)?;
    }

    let requested_state_path = match args.resume.clone().or(args.state_path.clone()) {
        Some(path) => path,
        None => project_root
            .join(".relay")
            .join("runs")
            .join(format!("{}.json", Uuid::new_v4())),
    };
    validate_state_path(&requested_state_path, project_root)?;

    let mut plan: Plan;
    let mut store: StateStore;
    let mut state: RunState;
    let mut reconciliation = Vec::new();
    let mut workflow_notice = None;
    let attempts: Vec<(String, PathBuf)>;
    if resuming {
        store = StateStore::open(&requested_state_path, true)?;
        state = store.read()?;
        reject_redacted_resume(&state.plan)?;
        if let Some(request) = workflow_resume.as_ref() {
            let metadata = state.workflow.as_ref().ok_or_else(|| {
                RunError::State(format!(
                    "aggregate state is not a workflow run for {:?}",
                    request.name
                ))
            })?;
            if metadata.name != request.name {
                return Err(RunError::State(format!(
                    "aggregate state belongs to workflow {:?}, not {:?}",
                    metadata.name, request.name
                )));
            }
            plan = state.plan.clone();
        } else {
            plan = supplied_plan
                .take()
                .expect("direct resume plan checked before state open");
        }
        resolve_working_directories(&mut plan, invocation_directory, Some(&state.plan))?;
        validate_encoded_plan_size(&plan)?;
        validate_plan(&plan, invocation_directory)?;
        attempts = plan
            .tasks
            .iter()
            .map(|_| {
                let run_id = Uuid::new_v4().to_string();
                let path = supervisor_state_path(&run_id)?;
                Ok((run_id, path))
            })
            .collect::<Result<_, RunError>>()?;
        preflight_tasks(
            &args.supervisor,
            &plan,
            &attempts,
            invocation_directory,
            project_root,
        )?;
        let sanitized_plan = sanitize_plan(&plan);
        validate_resume_state(&state, &sanitized_plan, store.path(), project_root, true)?;
        if let Some(request) = workflow_resume.as_ref()
            && let Some(current) = request.current_definition_sha256.as_deref()
            && state
                .workflow
                .as_ref()
                .is_some_and(|workflow| workflow.definition_sha256 != current)
        {
            workflow_notice = Some("workflow definition changed since run started".into());
        }
        let coordinator_cancelled = state.cancellation_requested;
        state.cancellation_requested = false;
        state.jobs = args.jobs;
        reconciliation = reconcile_running_attempts(
            &args,
            &plan,
            &mut state,
            &mut store,
            invocation_directory,
            project_root,
            coordinator_cancelled,
        )?;
    } else {
        plan = supplied_plan.expect("new run plan checked before state open");
        resolve_working_directories(&mut plan, invocation_directory, None)?;
        validate_encoded_plan_size(&plan)?;
        validate_plan(&plan, invocation_directory)?;
        attempts = plan
            .tasks
            .iter()
            .map(|_| {
                let run_id = Uuid::new_v4().to_string();
                let path = supervisor_state_path(&run_id)?;
                Ok((run_id, path))
            })
            .collect::<Result<_, RunError>>()?;
        preflight_tasks(
            &args.supervisor,
            &plan,
            &attempts,
            invocation_directory,
            project_root,
        )?;
        ensure_state_parent(&requested_state_path)?;
        validate_state_path(&requested_state_path, project_root)?;
        store = StateStore::open(&requested_state_path, false)?;
        let now = now();
        state = RunState {
            schema_version: RUN_STATE_SCHEMA_VERSION,
            state_path: path_string(store.path())?,
            run_id: Uuid::new_v4().to_string(),
            created_at: now.clone(),
            updated_at: now,
            status: RunStatus::Running,
            cancellation_requested: false,
            jobs: args.jobs,
            workflow: workflow_metadata,
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
                    outputs: None,
                    attempts: Vec::new(),
                    retry_ready_at: None,
                })
                .collect(),
        };
        test_pause("before_initial_install")?;
        store.write_initial(&state)?;
    }

    mark_blocked(&plan, &mut state);
    state.status = derive_status(&state.tasks, state.cancellation_requested);
    persist_at(&mut store, &mut state, "initial")?;
    let coordinator = Arc::new(
        supervise::CoordinatorSignals::install()
            .map_err(|source| supervisor_error(&state, source))?,
    );
    let (completed_sender, completed_receiver) = mpsc::channel::<TaskCompletion>();
    let mut active = HashMap::<usize, thread::JoinHandle<()>>::new();
    let mut boundary_error = None;

    loop {
        if coordinator.cancellation_requested() && !state.cancellation_requested {
            state.cancellation_requested = true;
            state.status = derive_status(&state.tasks, true);
            if let Err(error) = persist_at(&mut store, &mut state, "cancellation") {
                return Err(shutdown_and_drain(
                    error,
                    ShutdownContext {
                        coordinator: coordinator.as_ref(),
                        active: &mut active,
                        completed_receiver: &completed_receiver,
                        plan: &plan,
                        state: &mut state,
                        store: &mut store,
                    },
                    None,
                ));
            }
        }

        while !state.cancellation_requested && active.len() < args.jobs {
            mark_blocked(&plan, &mut state);
            if coordinator.cancellation_requested() {
                state.cancellation_requested = true;
                state.status = derive_status(&state.tasks, true);
                if let Err(error) = persist_at(&mut store, &mut state, "cancellation") {
                    return Err(shutdown_and_drain(
                        error,
                        ShutdownContext {
                            coordinator: coordinator.as_ref(),
                            active: &mut active,
                            completed_receiver: &completed_receiver,
                            plan: &plan,
                            state: &mut state,
                            store: &mut store,
                        },
                        None,
                    ));
                }
                break;
            }
            let Some(index) = next_ready(&plan, &state.tasks, Utc::now()) else {
                break;
            };

            let task = &plan.tasks[index];
            let (attempt_run_id, supervisor_state_path) = if state.tasks[index].attempts.is_empty()
            {
                attempts[index].clone()
            } else {
                let run_id = Uuid::new_v4().to_string();
                let path = match supervisor_state_path(&run_id) {
                    Ok(path) => path,
                    Err(error) => {
                        return Err(shutdown_and_drain(
                            error,
                            ShutdownContext {
                                coordinator: coordinator.as_ref(),
                                active: &mut active,
                                completed_receiver: &completed_receiver,
                                plan: &plan,
                                state: &mut state,
                                store: &mut store,
                            },
                            None,
                        ));
                    }
                };
                (run_id, path)
            };
            let supervisor_state_path_string = match path_string(&supervisor_state_path) {
                Ok(path) => path,
                Err(error) => {
                    return Err(shutdown_and_drain(
                        error,
                        ShutdownContext {
                            coordinator: coordinator.as_ref(),
                            active: &mut active,
                            completed_receiver: &completed_receiver,
                            plan: &plan,
                            state: &mut state,
                            store: &mut store,
                        },
                        None,
                    ));
                }
            };
            let attempt_number = state.tasks[index].attempts.len() as u32 + 1;
            state.tasks[index].attempts.push(AttemptRecord {
                attempt: attempt_number,
                run_id: attempt_run_id.clone(),
                supervisor_state_path: supervisor_state_path_string.clone(),
                status: AttemptStatus::Running,
                reason: None,
                summary: None,
                outputs: None,
            });
            state.tasks[index].status = TaskStatus::Running;
            state.tasks[index].reason = None;
            state.tasks[index].summary = None;
            state.tasks[index].retry_ready_at = None;
            state.tasks[index].attempt_run_id = Some(attempt_run_id.clone());
            state.tasks[index].supervisor_state_path = Some(supervisor_state_path_string);
            state.status = RunStatus::Running;
            if let Err(error) = persist_at(&mut store, &mut state, "running") {
                return Err(shutdown_and_drain(
                    error,
                    ShutdownContext {
                        coordinator: coordinator.as_ref(),
                        active: &mut active,
                        completed_receiver: &completed_receiver,
                        plan: &plan,
                        state: &mut state,
                        store: &mut store,
                    },
                    Some(index),
                ));
            }

            if let Err(error) = test_pause("before_supervisor_launch") {
                return Err(shutdown_and_drain(
                    error,
                    ShutdownContext {
                        coordinator: coordinator.as_ref(),
                        active: &mut active,
                        completed_receiver: &completed_receiver,
                        plan: &plan,
                        state: &mut state,
                        store: &mut store,
                    },
                    Some(index),
                ));
            }
            if coordinator.cancellation_requested() {
                rollback_unlaunched_attempt(&mut state.tasks[index]);
                state.cancellation_requested = true;
                state.status = derive_status(&state.tasks, true);
                if let Err(error) = persist_at(&mut store, &mut state, "cancellation") {
                    return Err(shutdown_and_drain(
                        error,
                        ShutdownContext {
                            coordinator: coordinator.as_ref(),
                            active: &mut active,
                            completed_receiver: &completed_receiver,
                            plan: &plan,
                            state: &mut state,
                            store: &mut store,
                        },
                        None,
                    ));
                }
                break;
            }

            let arguments = match resolve_task_arguments(&plan, &state, index, project_root) {
                Ok(arguments) => arguments,
                Err(error) => {
                    mark_task_indeterminate(
                        &mut state.tasks[index],
                        truncate(&error.to_string(), MAX_SUMMARY_MESSAGE_BYTES),
                    );
                    continue;
                }
            };
            let supervise_args = match task_supervise_args(
                &args.supervisor,
                task,
                attempt_run_id.clone(),
                supervisor_state_path.clone(),
                &arguments,
                project_root,
            ) {
                Ok(arguments) => arguments,
                Err(error) => {
                    mark_task_indeterminate(
                        &mut state.tasks[index],
                        truncate(&error.to_string(), MAX_SUMMARY_MESSAGE_BYTES),
                    );
                    continue;
                }
            };
            let sender = completed_sender.clone();
            let coordinator_for_task = Arc::clone(&coordinator);
            let output_task = task.clone();
            let output_project_root = project_root.to_owned();
            let output_attempt_run_id = attempt_run_id.clone();
            let invocation_directory = invocation_directory.to_owned();
            let task_id = task.id.clone();
            let worker = thread::Builder::new()
                .name(format!("relay-task-{task_id}"))
                .spawn(move || {
                    let result = if let Err(error) = prepare_output_paths(
                        &output_project_root,
                        &output_attempt_run_id,
                        &output_task,
                    ) {
                        Err(SupervisorError::Transport(io::Error::other(
                            error.to_string(),
                        )))
                    } else {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            supervise::execute_coordinated(
                                supervise_args,
                                &invocation_directory,
                                &coordinator_for_task,
                            )
                        }))
                        .unwrap_or_else(|_| {
                            Err(SupervisorError::Transport(io::Error::other(
                                "supervisor worker panicked",
                            )))
                        })
                    };
                    let _ = sender.send(TaskCompletion { index, result });
                });
            let worker = match worker {
                Ok(worker) => worker,
                Err(source) => {
                    let source = SupervisorError::Transport(source);
                    mark_task_indeterminate(
                        &mut state.tasks[index],
                        truncate(&source.to_string(), MAX_SUMMARY_MESSAGE_BYTES),
                    );
                    mark_blocked(&plan, &mut state);
                    state.status = RunStatus::Indeterminate;
                    return Err(shutdown_and_drain(
                        supervisor_error(&state, source),
                        ShutdownContext {
                            coordinator: coordinator.as_ref(),
                            active: &mut active,
                            completed_receiver: &completed_receiver,
                            plan: &plan,
                            state: &mut state,
                            store: &mut store,
                        },
                        None,
                    ));
                }
            };
            active.insert(index, worker);
        }

        if active.is_empty() {
            mark_blocked(&plan, &mut state);
            if state.cancellation_requested {
                break;
            }
            if next_ready(&plan, &state.tasks, Utc::now()).is_some() {
                continue;
            }
            if next_retry_wait(&plan, &state.tasks).is_none() {
                break;
            }
        }

        let wait = if active.len() < args.jobs {
            next_retry_wait(&plan, &state.tasks)
        } else {
            None
        };
        let completion = match receive_completion(&completed_receiver, wait) {
            Ok(Some(completion)) => completion,
            Ok(None) => continue,
            Err(()) => {
                let source =
                    SupervisorError::Transport(io::Error::other("task completion channel closed"));
                return Err(shutdown_and_drain(
                    supervisor_error(&state, source),
                    ShutdownContext {
                        coordinator: coordinator.as_ref(),
                        active: &mut active,
                        completed_receiver: &completed_receiver,
                        plan: &plan,
                        state: &mut state,
                        store: &mut store,
                    },
                    None,
                ));
            }
        };
        let Some(worker) = active.remove(&completion.index) else {
            let source = SupervisorError::Transport(io::Error::other(
                "task completion did not match an active worker",
            ));
            return Err(shutdown_and_drain(
                supervisor_error(&state, source),
                ShutdownContext {
                    coordinator: coordinator.as_ref(),
                    active: &mut active,
                    completed_receiver: &completed_receiver,
                    plan: &plan,
                    state: &mut state,
                    store: &mut store,
                },
                None,
            ));
        };
        if worker.join().is_err() {
            let source = SupervisorError::Transport(io::Error::other(
                "supervisor worker panicked while joining",
            ));
            mark_task_indeterminate(
                &mut state.tasks[completion.index],
                truncate(&source.to_string(), MAX_SUMMARY_MESSAGE_BYTES),
            );
            mark_blocked(&plan, &mut state);
            state.status = RunStatus::Indeterminate;
            return Err(shutdown_and_drain(
                supervisor_error(&state, source),
                ShutdownContext {
                    coordinator: coordinator.as_ref(),
                    active: &mut active,
                    completed_receiver: &completed_receiver,
                    plan: &plan,
                    state: &mut state,
                    store: &mut store,
                },
                None,
            ));
        }

        let coordinator_cancelled = coordinator.cancellation_requested();
        if let Some(source) = apply_completion(
            &plan,
            &mut state,
            completion,
            coordinator_cancelled,
            project_root,
        ) && boundary_error.is_none()
        {
            boundary_error = Some(source);
        }
        if let Err(error) = test_pause("after_supervisor_result") {
            return Err(shutdown_and_drain(
                error,
                ShutdownContext {
                    coordinator: coordinator.as_ref(),
                    active: &mut active,
                    completed_receiver: &completed_receiver,
                    plan: &plan,
                    state: &mut state,
                    store: &mut store,
                },
                None,
            ));
        }
        state.status = derive_status(&state.tasks, state.cancellation_requested);
        let persistence_point = if state.cancellation_requested {
            "cancellation"
        } else {
            "completion"
        };
        if let Err(error) = persist_at(&mut store, &mut state, persistence_point) {
            return Err(shutdown_and_drain(
                error,
                ShutdownContext {
                    coordinator: coordinator.as_ref(),
                    active: &mut active,
                    completed_receiver: &completed_receiver,
                    plan: &plan,
                    state: &mut state,
                    store: &mut store,
                },
                None,
            ));
        }
    }

    state.status = derive_status(&state.tasks, state.cancellation_requested);
    persist_at(&mut store, &mut state, "final")?;
    if let Some(source) = boundary_error {
        return Err(supervisor_error(&state, source));
    }
    Ok(render(store.path(), state, reconciliation, workflow_notice))
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
fn attempt_output_root(project_root: &Path, attempt_run_id: &str) -> PathBuf {
    project_root
        .join(".relay")
        .join("runs")
        .join("outputs")
        .join(attempt_run_id)
}

fn declared_output_path(
    project_root: &Path,
    attempt_run_id: &str,
    declaration: &OutputDeclaration,
) -> PathBuf {
    attempt_output_root(project_root, attempt_run_id).join(&declaration.path)
}

fn output_environment(
    project_root: &Path,
    attempt_run_id: &str,
    task: &PlanTask,
) -> Result<Vec<(String, String)>, RunError> {
    if task.outputs.is_empty() {
        return Ok(Vec::new());
    }
    let mut manifest = serde_json::Map::new();
    for declaration in &task.outputs {
        let path = declared_output_path(project_root, attempt_run_id, declaration);
        let path = path.to_str().ok_or_else(|| {
            RunError::Plan(format!(
                "task {:?} output path must be valid UTF-8",
                task.id
            ))
        })?;
        manifest.insert(declaration.name.clone(), Value::String(path.to_owned()));
    }
    let value = serde_json::to_string(&Value::Object(manifest))
        .map_err(|error| RunError::Plan(format!("output manifest cannot be encoded: {error}")))?;
    if value.len() > MAX_OUTPUT_MANIFEST_BYTES {
        return Err(RunError::Plan(format!(
            "task {:?} output manifest exceeds {MAX_OUTPUT_MANIFEST_BYTES} bytes",
            task.id
        )));
    }
    Ok(vec![("RELAY_OUTPUT_MANIFEST".into(), value)])
}

fn prepare_output_paths(
    project_root: &Path,
    attempt_run_id: &str,
    task: &PlanTask,
) -> Result<(), RunError> {
    let root = attempt_output_root(project_root, attempt_run_id);
    let output_parent = project_root.join(".relay").join("runs").join("outputs");
    reject_symlinked_components(&output_parent)
        .map_err(|error| RunError::Plan(error.to_string()))?;
    fs::create_dir_all(&root).map_err(|error| {
        RunError::Plan(format!(
            "could not create task output directory {}: {error}",
            root.display()
        ))
    })?;
    reject_symlinked_components(&root).map_err(|error| RunError::Plan(error.to_string()))?;
    for declaration in &task.outputs {
        let path = declared_output_path(project_root, attempt_run_id, declaration);
        let parent = path.parent().expect("declared output path has a parent");
        reject_symlinked_components(parent).map_err(|error| RunError::Plan(error.to_string()))?;
        fs::create_dir_all(parent).map_err(|error| {
            RunError::Plan(format!(
                "could not create task output directory {}: {error}",
                parent.display()
            ))
        })?;
        reject_symlinked_components(parent).map_err(|error| RunError::Plan(error.to_string()))?;
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(RunError::Plan(format!(
                    "task {:?} output path already exists: {}",
                    task.id, declaration.path
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(RunError::Plan(format!(
                    "could not inspect task output path {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}
fn project_root_for_state_path(state_path: &str) -> PathBuf {
    let path = Path::new(state_path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if parent.file_name() == Some(OsStr::new("runs"))
        && parent.parent().and_then(Path::file_name) == Some(OsStr::new(".relay"))
    {
        parent
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent.to_path_buf())
    } else {
        parent.to_path_buf()
    }
}

#[cfg(target_os = "linux")]
fn open_output_file(path: &Path) -> io::Result<File> {
    let parent_path = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output path has no parent"))?;
    let parent =
        open_parent_nofollow(parent_path).map_err(|error| io::Error::other(error.to_string()))?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "output path has no filename")
    })?;
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "output filename contains NUL"))?;
    openat(
        &parent,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        0,
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_output_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_output_file(path: &Path) -> io::Result<File> {
    File::open(path)
}

pub(crate) fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = digest.as_ref();
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[usize::from(*byte >> 4)] as char);
        result.push(HEX[usize::from(*byte & 0x0f)] as char);
    }
    result
}

fn read_hashed_output(
    path: &Path,
    maximum: u64,
    capture: bool,
) -> Result<(Option<Vec<u8>>, u64, String), String> {
    reject_symlinked_components(path).map_err(|error| error.to_string())?;
    let mut file = open_output_file(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("output is not a regular file: {}", path.display()));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(format!(
            "output has unsafe hardlink count: {}",
            path.display()
        ));
    }
    if metadata.len() > maximum {
        return Err(format!(
            "output exceeds {maximum} bytes: {}",
            path.display()
        ));
    }
    let mut bytes = capture.then(|| Vec::with_capacity(metadata.len() as usize));
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| format!("output size overflow: {}", path.display()))?;
        if total > maximum {
            return Err(format!(
                "output exceeds {maximum} bytes: {}",
                path.display()
            ));
        }
        hasher.update(&buffer[..count]);
        if let Some(bytes) = &mut bytes {
            bytes.extend_from_slice(&buffer[..count]);
        }
    }
    let after = file
        .metadata()
        .map_err(|error| format!("could not restat {}: {error}", path.display()))?;
    #[cfg(unix)]
    if after.nlink() != 1 {
        return Err(format!(
            "output link count changed while it was read: {}",
            path.display()
        ));
    }
    if after.len() != total {
        return Err(format!(
            "output changed while it was read: {}",
            path.display()
        ));
    }
    Ok((bytes, total, hex_digest(hasher.finalize())))
}

fn finalize_outputs(
    project_root: &Path,
    aggregate_run_id: &str,
    task: &PlanTask,
    attempt: &AttemptRecord,
) -> Result<Vec<OutputRecord>, String> {
    let mut records = Vec::with_capacity(task.outputs.len());
    for declaration in &task.outputs {
        let path = declared_output_path(project_root, &attempt.run_id, declaration);
        let (bytes, size_bytes, sha256) = match declaration.output_type {
            OutputType::Json => {
                read_hashed_output(&path, MAX_STRUCTURED_OUTPUT_BYTES as u64, true)?
            }
            OutputType::File => read_hashed_output(&path, MAX_ARTIFACT_FILE_BYTES, false)?,
        };
        let value = match declaration.output_type {
            OutputType::Json => {
                let bytes = bytes.ok_or_else(|| "structured output was not captured".to_owned())?;
                let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
                    format!(
                        "structured output {:?} is invalid JSON: {error}",
                        declaration.name
                    )
                })?;
                if serde_json::to_vec(&value)
                    .map_err(|error| error.to_string())?
                    .len()
                    > MAX_STRUCTURED_OUTPUT_BYTES
                {
                    return Err(format!(
                        "structured output {:?} exceeds {MAX_STRUCTURED_OUTPUT_BYTES} bytes",
                        declaration.name
                    ));
                }
                value
            }
            OutputType::File => Value::Null,
        };
        let path = path
            .to_str()
            .ok_or_else(|| "output path is not valid UTF-8".to_owned())?
            .to_owned();
        let record = OutputRecord {
            name: declaration.name.clone(),
            output_type: declaration.output_type,
            path,
            size_bytes,
            sha256: Some(sha256),
            value,
            run_id: aggregate_run_id.to_owned(),
            producer_task: task.id.clone(),
            attempt: attempt.attempt,
            attempt_run_id: attempt.run_id.clone(),
        };
        if declaration.output_type == OutputType::File
            && serde_json::to_vec(&record)
                .map_err(|error| error.to_string())?
                .len()
                > MAX_ARTIFACT_METADATA_BYTES
        {
            return Err(format!(
                "artifact metadata {:?} exceeds {MAX_ARTIFACT_METADATA_BYTES} bytes",
                declaration.name
            ));
        }
        records.push(record);
    }
    Ok(records)
}

fn validate_output_record(
    project_root: &Path,
    aggregate_run_id: &str,
    producer: &PlanTask,
    declaration: &OutputDeclaration,
    record: &OutputRecord,
) -> Result<(), String> {
    let expected_path = declared_output_path(project_root, &record.attempt_run_id, declaration);
    if record.name != declaration.name
        || record.run_id != aggregate_run_id
        || record.output_type != declaration.output_type
        || record.producer_task != producer.id
        || record.path != expected_path.to_str().unwrap_or_default()
        || record.sha256.as_deref().is_none()
    {
        return Err(format!(
            "output {:?} has mismatched provenance",
            declaration.name
        ));
    }
    Ok(())
}

fn resolve_task_arguments(
    plan: &Plan,
    state: &RunState,
    index: usize,
    project_root: &Path,
) -> Result<Vec<String>, RunError> {
    let task = &plan.tasks[index];
    let mut arguments = task.arguments.clone();
    for input in &task.inputs {
        let producer_index = plan
            .tasks
            .iter()
            .position(|candidate| candidate.id == input.from_task)
            .ok_or_else(|| {
                RunError::State(format!("input producer {:?} is missing", input.from_task))
            })?;
        let producer = &plan.tasks[producer_index];
        let declaration = producer
            .outputs
            .iter()
            .find(|output| output.name == input.output)
            .ok_or_else(|| {
                RunError::State(format!("input output {:?} is missing", input.output))
            })?;
        let producer_record = &state.tasks[producer_index];
        if producer_record.status != TaskStatus::Succeeded {
            return Err(RunError::State(format!(
                "input producer {:?} did not succeed",
                input.from_task
            )));
        }
        let latest = producer_record
            .attempts
            .last()
            .filter(|attempt| attempt.status == AttemptStatus::Succeeded)
            .ok_or_else(|| {
                RunError::State(format!(
                    "input producer {:?} has no successful attempt",
                    input.from_task
                ))
            })?;
        let output = producer_record
            .outputs
            .as_ref()
            .and_then(|outputs| outputs.iter().find(|output| output.name == input.output))
            .ok_or_else(|| {
                RunError::State(format!("input output {:?} is not trusted", input.output))
            })?;
        if latest
            .outputs
            .as_ref()
            .and_then(|outputs| {
                outputs
                    .iter()
                    .find(|candidate| candidate.name == output.name)
            })
            .is_none_or(|candidate| {
                candidate.attempt != output.attempt
                    || candidate.attempt_run_id != output.attempt_run_id
                    || candidate.path != output.path
                    || candidate.sha256 != output.sha256
            })
        {
            return Err(RunError::State(format!(
                "input output {:?} has mismatched attempt provenance",
                input.output
            )));
        }
        validate_output_record(project_root, &state.run_id, producer, declaration, output)
            .map_err(RunError::State)?;
        let path = Path::new(&output.path);
        let value = match output.output_type {
            OutputType::Json => {
                let (bytes, size, sha256) =
                    read_hashed_output(path, MAX_STRUCTURED_OUTPUT_BYTES as u64, true)
                        .map_err(RunError::State)?;
                if size != output.size_bytes || output.sha256.as_deref() != Some(sha256.as_str()) {
                    return Err(RunError::State(format!(
                        "input output {:?} changed after production",
                        input.output
                    )));
                }
                let bytes = bytes
                    .ok_or_else(|| RunError::State("structured input was not captured".into()))?;
                let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
                    RunError::State(format!(
                        "input output {:?} is invalid JSON: {error}",
                        input.output
                    ))
                })?;
                if output.value != value {
                    return Err(RunError::State(format!(
                        "input output {:?} value changed after production",
                        input.output
                    )));
                }
                serde_json::to_string(&value).map_err(|error| {
                    RunError::State(format!(
                        "input output {:?} cannot be encoded: {error}",
                        input.output
                    ))
                })?
            }
            OutputType::File => {
                let (_, size, sha256) = read_hashed_output(path, MAX_ARTIFACT_FILE_BYTES, false)
                    .map_err(RunError::State)?;
                if size != output.size_bytes || output.sha256.as_deref() != Some(sha256.as_str()) {
                    return Err(RunError::State(format!(
                        "input artifact {:?} changed after production",
                        input.output
                    )));
                }
                output.path.clone()
            }
        };
        arguments.push(input.argument.clone());
        arguments.push(value);
    }
    Ok(arguments)
}

fn task_supervise_args(
    supervisor: &Option<PathBuf>,
    task: &PlanTask,
    run_id: String,
    state_path: PathBuf,
    arguments: &[String],
    project_root: &Path,
) -> Result<SuperviseArgs, RunError> {
    let relay_environment = output_environment(project_root, &run_id, task)?;
    Ok(SuperviseArgs {
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
        relay_environment,
        program_and_args: std::iter::once(task.program.clone())
            .chain(arguments.iter().cloned())
            .collect(),
    })
}

fn preflight_tasks(
    supervisor: &Option<PathBuf>,
    plan: &Plan,
    attempts: &[(String, PathBuf)],
    invocation_directory: &Path,
    project_root: &Path,
) -> Result<(), RunError> {
    for (task, (run_id, state_path)) in plan.tasks.iter().zip(attempts) {
        if !task.outputs.is_empty() {
            prepare_output_paths(project_root, run_id, task)?;
        }
        let request = task_supervise_args(
            supervisor,
            task,
            run_id.clone(),
            state_path.clone(),
            &task.arguments,
            project_root,
        )?;
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
        .create(true)
        .append(true)
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
    if !matches!(
        plan.schema_version,
        LEGACY_PLAN_SCHEMA_VERSION | PREVIOUS_PLAN_SCHEMA_VERSION | PLAN_SCHEMA_VERSION
    ) {
        return Err(RunError::Plan(format!(
            "schema_version must be {LEGACY_PLAN_SCHEMA_VERSION}, {PREVIOUS_PLAN_SCHEMA_VERSION}, or {PLAN_SCHEMA_VERSION}"
        )));
    }
    if plan.schema_version == LEGACY_PLAN_SCHEMA_VERSION
        && plan.tasks.iter().any(|task| task.retry.is_some())
    {
        return Err(RunError::Plan(
            "retry requires plan schema_version 2".into(),
        ));
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
    validate_output_declarations(plan)?;
    validate_acyclic(plan)
}

fn validate_output_declarations(plan: &Plan) -> Result<(), RunError> {
    let ids: HashSet<&str> = plan.tasks.iter().map(|task| task.id.as_str()).collect();
    for task in &plan.tasks {
        if plan.schema_version != PLAN_SCHEMA_VERSION
            && (!task.outputs.is_empty() || !task.inputs.is_empty())
        {
            return Err(RunError::Plan(format!(
                "task {:?} outputs and inputs require plan schema_version {PLAN_SCHEMA_VERSION}",
                task.id
            )));
        }
        if task.outputs.len() > MAX_OUTPUTS_PER_TASK {
            return Err(RunError::Plan(format!(
                "task {:?} declares more than {MAX_OUTPUTS_PER_TASK} outputs",
                task.id
            )));
        }
        let mut output_names = HashSet::with_capacity(task.outputs.len());
        for output in &task.outputs {
            if !valid_output_name(&output.name) {
                return Err(RunError::Plan(format!(
                    "task {:?} output name {:?} must match [a-z][a-z0-9_]{{0,63}}",
                    task.id, output.name
                )));
            }
            if !output_names.insert(output.name.as_str()) {
                return Err(RunError::Plan(format!(
                    "task {:?} has duplicate output name {:?}",
                    task.id, output.name
                )));
            }
            validate_text(&format!("task {:?} output path", task.id), &output.path)?;
            validate_output_path(&task.id, &output.path)?;
        }
        if task.inputs.len() > MAX_OUTPUTS_PER_TASK {
            return Err(RunError::Plan(format!(
                "task {:?} declares more than {MAX_OUTPUTS_PER_TASK} inputs",
                task.id
            )));
        }
        for input in &task.inputs {
            validate_text(
                &format!("task {:?} input producer", task.id),
                &input.from_task,
            )?;
            validate_text(&format!("task {:?} input output", task.id), &input.output)?;
            validate_text(
                &format!("task {:?} input argument", task.id),
                &input.argument,
            )?;
            if input.argument.is_empty() {
                return Err(RunError::Plan(format!(
                    "task {:?} input argument is empty",
                    task.id
                )));
            }
            if !ids.contains(input.from_task.as_str()) {
                return Err(RunError::Plan(format!(
                    "task {:?} input references unknown task {:?}",
                    task.id, input.from_task
                )));
            }
            let producer = plan
                .tasks
                .iter()
                .find(|candidate| candidate.id == input.from_task)
                .expect("validated producer task id");
            if !producer
                .outputs
                .iter()
                .any(|output| output.name == input.output)
            {
                return Err(RunError::Plan(format!(
                    "task {:?} input references unknown output {:?} from {:?}",
                    task.id, input.output, input.from_task
                )));
            }
            if !is_transitive_dependency(plan, &task.id, &input.from_task) {
                return Err(RunError::Plan(format!(
                    "task {:?} input producer {:?} is not a dependency",
                    task.id, input.from_task
                )));
            }
        }
    }
    Ok(())
}

fn valid_output_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_OUTPUT_NAME_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn validate_output_path(task_id: &str, value: &str) -> Result<(), RunError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > MAX_ARTIFACT_PATH_BYTES
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RunError::Plan(format!(
            "task {:?} output path must be a relative path of normal components",
            task_id
        )));
    }
    Ok(())
}

fn is_transitive_dependency(plan: &Plan, task_id: &str, producer_id: &str) -> bool {
    let mut pending = vec![task_id];
    let mut visited = HashSet::new();
    while let Some(current) = pending.pop() {
        if !visited.insert(current) {
            continue;
        }
        let Some(task) = plan.tasks.iter().find(|task| task.id == current) else {
            continue;
        };
        if task
            .depends_on
            .iter()
            .any(|dependency| dependency == producer_id)
        {
            return true;
        }
        pending.extend(task.depends_on.iter().map(String::as_str));
    }
    false
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
    if let Some(retry) = &task.retry {
        if !(1..=MAX_RETRY_ATTEMPTS).contains(&retry.max_attempts) {
            return Err(RunError::Plan(format!(
                "task {:?} retry.max_attempts must be between 1 and {MAX_RETRY_ATTEMPTS}",
                task.id
            )));
        }
        if retry.delay_ms > MAX_RETRY_DELAY_MS {
            return Err(RunError::Plan(format!(
                "task {:?} retry.delay_ms must be at most {MAX_RETRY_DELAY_MS}",
                task.id
            )));
        }
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

fn reject_redacted_resume(plan: &Plan) -> Result<(), RunError> {
    if plan
        .tasks
        .iter()
        .any(|task| !task.redact_argument_indexes.is_empty())
    {
        return Err(RunError::State(
            "resume is unavailable for plans containing redacted_argument_indexes".into(),
        ));
    }
    Ok(())
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

pub(crate) fn validate_state_path(path: &Path, project_root: &Path) -> Result<(), RunError> {
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

fn validate_handoff_state_path(path: &Path, project_root: &Path) -> Result<(), RunError> {
    validate_state_path(path, project_root)?;
    let normalized_path = normalize_absolute(path)?;
    let runs = project_root.join(".relay").join("runs");
    let normalized_runs = normalize_absolute(&runs)?;
    if normalized_path == normalized_runs || !normalized_path.starts_with(&normalized_runs) {
        return Err(RunError::State(format!(
            "handoff run path must be under {}: {}",
            runs.display(),
            path.display()
        )));
    }
    let canonical_path = fs::canonicalize(path).map_err(|error| {
        RunError::State(format!(
            "could not resolve run path {}: {error}",
            path.display()
        ))
    })?;
    let canonical_runs = fs::canonicalize(&runs).map_err(|error| {
        RunError::State(format!(
            "could not resolve run directory {}: {error}",
            runs.display()
        ))
    })?;
    if canonical_path == canonical_runs || !canonical_path.starts_with(&canonical_runs) {
        return Err(RunError::State(format!(
            "handoff run path resolves outside {}: {}",
            runs.display(),
            path.display()
        )));
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

fn encode_state(state: &RunState) -> Result<Vec<u8>, RunError> {
    let mut bytes = serde_json::to_vec(state)
        .map_err(|error| RunError::State(format!("could not encode state: {error}")))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(RunError::State(format!(
            "encoded state exceeds {MAX_STATE_BYTES} bytes"
        )));
    }
    Ok(bytes)
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
        decode_run_state(&bytes, &self.path)
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
        let bytes = encode_state(state)?;
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
        let bytes = encode_state(state)?;
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

fn persist_at(store: &mut StateStore, state: &mut RunState, point: &str) -> Result<(), RunError> {
    state.updated_at = now();
    test_fail_persist(point)?;
    store.write_update(state)
}

#[cfg(debug_assertions)]
fn test_fail_persist(point: &str) -> Result<(), RunError> {
    static FAILURE_INJECTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if env::var_os("RELAY_TEST_RUN_FAIL_PERSIST_AT").as_deref() == Some(OsStr::new(point))
        && !FAILURE_INJECTED.swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return Err(RunError::State(format!(
            "injected aggregate persistence failure at {point}"
        )));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn test_fail_persist(_point: &str) -> Result<(), RunError> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn write_state(path: &Path, state: &RunState) -> Result<(), RunError> {
    let bytes = encode_state(state)?;
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
    decode_run_state(&bytes, path)
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
#[cfg(target_os = "linux")]
fn read_handoff_state(path: &Path) -> Result<(PathBuf, Vec<u8>), RunError> {
    let parent_path = path
        .parent()
        .ok_or_else(|| RunError::State("handoff run path has no parent".into()))?;
    let parent = open_parent_nofollow(parent_path)?;
    let canonical_parent = fs::canonicalize(parent_path).map_err(|error| {
        RunError::State(format!(
            "could not canonicalize {}: {error}",
            parent_path.display()
        ))
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| RunError::State("handoff run path must name a file".into()))?;
    let name = c_name(file_name, "handoff run filename")?;
    let file = openat(
        &parent,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .map_err(|error| {
        RunError::State(format!(
            "could not securely open {}: {error}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(|error| {
        RunError::State(format!("could not inspect {}: {error}", path.display()))
    })?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(RunError::State(format!(
            "handoff run state must be a singly-linked regular file: {}",
            path.display()
        )));
    }
    let canonical_path = canonical_parent.join(file_name);
    let bytes = read_bounded_file(file, MAX_STATE_BYTES)
        .map_err(|error| RunError::State(format!("could not read {}: {error}", path.display())))?;
    Ok((canonical_path, bytes))
}

#[cfg(not(target_os = "linux"))]
fn read_handoff_state(path: &Path) -> Result<(PathBuf, Vec<u8>), RunError> {
    let canonical_path = fs::canonicalize(path).map_err(|error| {
        RunError::State(format!(
            "could not resolve run path {}: {error}",
            path.display()
        ))
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        RunError::State(format!("could not inspect {}: {error}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(RunError::State(format!(
            "handoff run state must be a regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(RunError::State(format!(
            "handoff run state must be singly linked: {}",
            path.display()
        )));
    }
    let file = File::open(&canonical_path)
        .map_err(|error| RunError::State(format!("could not open {}: {error}", path.display())))?;
    let bytes = read_bounded_file(file, MAX_STATE_BYTES)
        .map_err(|error| RunError::State(format!("could not read {}: {error}", path.display())))?;
    Ok((canonical_path, bytes))
}

fn decode_run_state(bytes: &[u8], path: &Path) -> Result<RunState, RunError> {
    let wire: RunStateWire = serde_json::from_slice(bytes).map_err(|error| {
        RunError::State(format!("could not decode {}: {error}", path.display()))
    })?;
    let RunStateWire {
        schema_version,
        run_id,
        state_path,
        created_at,
        updated_at,
        status,
        cancellation_requested,
        jobs,
        workflow,
        plan,
        tasks,
    } = wire;
    if schema_version != RUN_STATE_SCHEMA_VERSION && workflow.is_some() {
        return Err(RunError::State(format!(
            "workflow metadata requires aggregate state schema_version {RUN_STATE_SCHEMA_VERSION}"
        )));
    }
    let jobs = match schema_version {
        LEGACY_RUN_STATE_SCHEMA_VERSION => match jobs {
            JobsField::Missing => 1,
            JobsField::Value(_) | JobsField::Null => {
                return Err(RunError::State(
                    "schema_version 1 aggregate state cannot contain jobs".into(),
                ));
            }
        },
        RUN_STATE_V2_SCHEMA_VERSION
        | PREVIOUS_PREVIOUS_RUN_STATE_SCHEMA_VERSION
        | PREVIOUS_RUN_STATE_SCHEMA_VERSION
        | RUN_STATE_SCHEMA_VERSION => match jobs {
            JobsField::Value(jobs) if jobs > 0 => jobs,
            JobsField::Value(_) => {
                return Err(RunError::State("jobs must be a positive integer".into()));
            }
            JobsField::Null | JobsField::Missing => {
                return Err(RunError::State(format!(
                    "schema_version {schema_version} aggregate state requires jobs"
                )));
            }
        },
        schema_version => {
            return Err(RunError::State(format!(
                "unsupported aggregate state schema_version {schema_version}"
            )));
        }
    };
    let tasks = match schema_version {
        LEGACY_RUN_STATE_SCHEMA_VERSION | RUN_STATE_V2_SCHEMA_VERSION => tasks
            .into_iter()
            .map(|value| decode_legacy_task(value, path))
            .collect::<Result<Vec<_>, _>>()?,
        PREVIOUS_PREVIOUS_RUN_STATE_SCHEMA_VERSION => tasks
            .into_iter()
            .map(|value| {
                serde_json::from_value::<TaskRecordV3>(value)
                    .map(Into::into)
                    .map_err(|error| {
                        RunError::State(format!(
                            "could not decode v3 task in {}: {error}",
                            path.display()
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?,
        PREVIOUS_RUN_STATE_SCHEMA_VERSION | RUN_STATE_SCHEMA_VERSION => tasks
            .into_iter()
            .map(|value| {
                serde_json::from_value(value).map_err(|error| {
                    RunError::State(format!(
                        "could not decode task in {}: {error}",
                        path.display()
                    ))
                })
            })
            .collect::<Result<Vec<TaskRecord>, _>>()?,
        _ => unreachable!("unsupported state schema rejected above"),
    };
    Ok(RunState {
        schema_version: RUN_STATE_SCHEMA_VERSION,
        run_id,
        state_path,
        created_at,
        updated_at,
        status,
        cancellation_requested,
        jobs,
        workflow,
        plan,
        tasks,
    })
}

fn decode_legacy_task(value: Value, path: &Path) -> Result<TaskRecord, RunError> {
    let legacy: LegacyTaskRecord = serde_json::from_value(value).map_err(|error| {
        RunError::State(format!(
            "could not decode legacy task in {}: {error}",
            path.display()
        ))
    })?;
    let LegacyTaskRecord {
        id,
        status,
        reason,
        attempt_run_id,
        supervisor_state_path,
        summary,
    } = legacy;
    let attempts = match (
        attempt_run_id.as_deref(),
        supervisor_state_path.as_deref(),
        attempt_status_from_task_status(status),
    ) {
        (Some(run_id), Some(supervisor_state_path), Some(status)) => vec![AttemptRecord {
            attempt: 1,
            run_id: run_id.into(),
            supervisor_state_path: supervisor_state_path.into(),
            status,
            reason: reason.clone(),
            summary: summary.clone(),
            outputs: None,
        }],
        _ => Vec::new(),
    };
    Ok(TaskRecord {
        id,
        status,
        reason,
        attempt_run_id,
        supervisor_state_path,
        summary,
        outputs: None,
        attempts,
        retry_ready_at: None,
    })
}

fn attempt_status_from_task_status(status: TaskStatus) -> Option<AttemptStatus> {
    match status {
        TaskStatus::Running => Some(AttemptStatus::Running),
        TaskStatus::Succeeded => Some(AttemptStatus::Succeeded),
        TaskStatus::Failed => Some(AttemptStatus::Failed),
        TaskStatus::Cancelled => Some(AttemptStatus::Cancelled),
        TaskStatus::Indeterminate => Some(AttemptStatus::Indeterminate),
        TaskStatus::Pending | TaskStatus::Blocked => None,
    }
}
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
pub(crate) fn validate_workflow_metadata(workflow: &WorkflowRunMetadata) -> Result<(), RunError> {
    if !valid_workflow_name(&workflow.name) {
        return Err(RunError::State(format!(
            "persisted workflow name {:?} is invalid",
            workflow.name
        )));
    }
    if !matches!(workflow.schema_version, 1 | 2) {
        return Err(RunError::State(format!(
            "unsupported persisted workflow schema_version {}",
            workflow.schema_version
        )));
    }
    if !valid_sha256(&workflow.definition_sha256) {
        return Err(RunError::State(
            "persisted workflow definition hash is invalid".into(),
        ));
    }
    if workflow.inputs.len() > MAX_WORKFLOW_INPUTS {
        return Err(RunError::State(format!(
            "persisted workflow contains more than {MAX_WORKFLOW_INPUTS} inputs"
        )));
    }
    for (name, value) in &workflow.inputs {
        if !valid_workflow_input_name(name) {
            return Err(RunError::State(format!(
                "persisted workflow input name {:?} is invalid",
                name
            )));
        }
        match value {
            Value::String(value) if value.len() <= MAX_WORKFLOW_INPUT_STRING_BYTES => {}
            Value::Bool(_) => {}
            Value::Number(value) if value.as_i64().is_some() => {}
            _ => {
                return Err(RunError::State(format!(
                    "persisted workflow input {:?} is not a bounded scalar",
                    name
                )));
            }
        }
    }
    if let Some(composition) = workflow.composition.as_ref() {
        if workflow.schema_version != 2 {
            return Err(RunError::State(
                "composition metadata requires workflow schema_version 2".into(),
            ));
        }
        if composition.root_workflow != workflow.name {
            return Err(RunError::State(
                "composition root does not match workflow name".into(),
            ));
        }
        if !valid_workflow_name(&composition.root_workflow)
            || !valid_sha256(&composition.root_definition_sha256)
            || !valid_sha256(&composition.resolved_plan_sha256)
        {
            return Err(RunError::State(
                "composition metadata identity is invalid".into(),
            ));
        }
        if composition.instances.len() > 64 {
            return Err(RunError::State(
                "composition metadata exceeds instance bound".into(),
            ));
        }
        let mut instance_paths = HashSet::new();
        for instance in &composition.instances {
            if !instance_paths.insert(instance.instance.as_str())
                || !valid_task_id(&instance.instance)
                || !valid_workflow_name(&instance.workflow)
                || !matches!(instance.schema_version, 1 | 2)
                || !valid_sha256(&instance.definition_sha256)
                || instance.resolved_inputs.len() > MAX_WORKFLOW_INPUTS
            {
                return Err(RunError::State(
                    "composition instance metadata is invalid".into(),
                ));
            }
            for (name, value) in &instance.resolved_inputs {
                if !valid_workflow_input_name(name) || !valid_workflow_scalar(value) {
                    return Err(RunError::State(
                        "composition input metadata is invalid".into(),
                    ));
                }
            }
        }
        if workflow.resolved_plan_sha256.as_deref()
            != Some(composition.resolved_plan_sha256.as_str())
        {
            return Err(RunError::State(
                "resolved plan identity does not match composition metadata".into(),
            ));
        }
    } else if workflow.schema_version == 2 {
        return Err(RunError::State(
            "schema_version 2 workflow metadata lacks composition".into(),
        ));
    }
    if let Some(hash) = workflow.resolved_plan_sha256.as_deref()
        && !valid_sha256(hash)
    {
        return Err(RunError::State(
            "persisted resolved plan hash is invalid".into(),
        ));
    }
    if serde_json::to_vec(workflow)
        .map_err(|error| RunError::State(format!("workflow metadata cannot be encoded: {error}")))?
        .len()
        > MAX_WORKFLOW_METADATA_BYTES
    {
        return Err(RunError::State(format!(
            "persisted workflow metadata exceeds {MAX_WORKFLOW_METADATA_BYTES} bytes"
        )));
    }
    Ok(())
}
fn valid_workflow_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_WORKFLOW_NAME_BYTES
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_workflow_input_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_WORKFLOW_INPUT_NAME_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}
fn valid_workflow_scalar(value: &Value) -> bool {
    match value {
        Value::String(value) => value.len() <= MAX_WORKFLOW_INPUT_STRING_BYTES,
        Value::Bool(_) => true,
        Value::Number(value) => value.as_i64().is_some(),
        Value::Null | Value::Array(_) | Value::Object(_) => false,
    }
}

fn validate_attempt_outputs(
    project_root: &Path,
    aggregate_run_id: &str,
    plan_task: &PlanTask,
    attempt: &AttemptRecord,
    verify_artifacts: bool,
) -> Result<(), RunError> {
    let Some(outputs) = &attempt.outputs else {
        if attempt.status == AttemptStatus::Succeeded && !plan_task.outputs.is_empty() {
            return Err(RunError::State(format!(
                "successful attempt {:?} has no output records",
                attempt.run_id
            )));
        }
        return Ok(());
    };
    if attempt.status != AttemptStatus::Succeeded {
        return Err(RunError::State(format!(
            "non-successful attempt {:?} contains trusted outputs",
            attempt.run_id
        )));
    }
    if outputs.len() != plan_task.outputs.len() {
        return Err(RunError::State(format!(
            "attempt {:?} output count does not match declaration",
            attempt.run_id
        )));
    }
    let mut names = HashSet::with_capacity(outputs.len());
    for record in outputs {
        let declaration = plan_task
            .outputs
            .iter()
            .find(|declaration| declaration.name == record.name)
            .ok_or_else(|| {
                RunError::State(format!(
                    "attempt {:?} contains undeclared output {:?}",
                    attempt.run_id, record.name
                ))
            })?;
        if !names.insert(record.name.as_str())
            || record.attempt != attempt.attempt
            || record.attempt_run_id != attempt.run_id
            || !valid_sha256(record.sha256.as_deref().unwrap_or_default())
        {
            return Err(RunError::State(format!(
                "attempt {:?} contains invalid output provenance",
                attempt.run_id
            )));
        }
        validate_output_record(
            project_root,
            aggregate_run_id,
            plan_task,
            declaration,
            record,
        )
        .map_err(RunError::State)?;
        let path = Path::new(&record.path);
        match record.output_type {
            OutputType::Json => {
                if record.size_bytes > MAX_STRUCTURED_OUTPUT_BYTES as u64
                    || serde_json::to_vec(&record.value)
                        .map_err(|error| RunError::State(error.to_string()))?
                        .len()
                        > MAX_STRUCTURED_OUTPUT_BYTES
                {
                    return Err(RunError::State(format!(
                        "structured output {:?} exceeds its bound",
                        record.name
                    )));
                }
                if !verify_artifacts {
                    continue;
                }
                let (bytes, size, sha256) =
                    read_hashed_output(path, MAX_STRUCTURED_OUTPUT_BYTES as u64, true)
                        .map_err(RunError::State)?;
                if size != record.size_bytes || record.sha256.as_deref() != Some(sha256.as_str()) {
                    return Err(RunError::State(format!(
                        "structured output {:?} changed after persistence",
                        record.name
                    )));
                }
                let bytes = bytes
                    .ok_or_else(|| RunError::State("structured output bytes missing".into()))?;
                let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
                    RunError::State(format!(
                        "structured output {:?} is invalid JSON: {error}",
                        record.name
                    ))
                })?;
                if record.value != value {
                    return Err(RunError::State(format!(
                        "structured output {:?} value mismatch",
                        record.name
                    )));
                }
            }
            OutputType::File => {
                if record.value != Value::Null {
                    return Err(RunError::State(format!(
                        "file output {:?} contains structured data",
                        record.name
                    )));
                }
                if serde_json::to_vec(record)
                    .map_err(|error| RunError::State(error.to_string()))?
                    .len()
                    > MAX_ARTIFACT_METADATA_BYTES
                {
                    return Err(RunError::State(format!(
                        "artifact metadata {:?} exceeds its bound",
                        record.name
                    )));
                }
                if record.size_bytes > MAX_ARTIFACT_FILE_BYTES {
                    return Err(RunError::State("artifact size exceeds its bound".into()));
                }
                if !verify_artifacts {
                    continue;
                }
                let (bytes, size, sha256) =
                    read_hashed_output(path, MAX_ARTIFACT_FILE_BYTES, false)
                        .map_err(RunError::State)?;
                drop(bytes);
                if size != record.size_bytes || record.sha256.as_deref() != Some(sha256.as_str()) {
                    return Err(RunError::State(format!(
                        "artifact {:?} changed after persistence",
                        record.name
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_task_outputs(
    project_root: &Path,
    aggregate_run_id: &str,
    plan_task: &PlanTask,
    task: &TaskRecord,
    verify_artifacts: bool,
) -> Result<(), RunError> {
    for attempt in &task.attempts {
        validate_attempt_outputs(
            project_root,
            aggregate_run_id,
            plan_task,
            attempt,
            verify_artifacts,
        )?;
    }
    let should_have_outputs = task.status == TaskStatus::Succeeded && !plan_task.outputs.is_empty();
    if task.outputs.is_some() != should_have_outputs {
        return Err(RunError::State(format!(
            "task {:?} has inconsistent output records",
            task.id
        )));
    }
    if let Some(outputs) = &task.outputs {
        let latest = task.attempts.last().ok_or_else(|| {
            RunError::State(format!("task {:?} has outputs without an attempt", task.id))
        })?;
        if latest.status != AttemptStatus::Succeeded || latest.outputs.as_ref() != Some(outputs) {
            return Err(RunError::State(format!(
                "task {:?} outputs do not match latest successful attempt",
                task.id
            )));
        }
    }
    Ok(())
}

fn validate_resume_state(
    state: &RunState,
    plan: &Plan,
    state_path: &Path,
    project_root: &Path,
    reject_redacted: bool,
) -> Result<(), RunError> {
    validate_resume_state_with_artifacts(
        state,
        plan,
        state_path,
        project_root,
        reject_redacted,
        true,
    )
}

fn validate_historical_state(
    state: &RunState,
    plan: &Plan,
    state_path: &Path,
    project_root: &Path,
) -> Result<(), RunError> {
    validate_resume_state_with_artifacts(state, plan, state_path, project_root, false, false)
}

fn validate_resume_state_with_artifacts(
    state: &RunState,
    plan: &Plan,
    state_path: &Path,
    project_root: &Path,
    reject_redacted: bool,
    verify_artifacts: bool,
) -> Result<(), RunError> {
    if state.schema_version != RUN_STATE_SCHEMA_VERSION {
        return Err(RunError::State(format!(
            "schema_version must be {RUN_STATE_SCHEMA_VERSION}"
        )));
    }
    if let Some(workflow) = state.workflow.as_ref() {
        validate_workflow_metadata(workflow)?;
    }
    if state.jobs == 0 {
        return Err(RunError::State("jobs must be a positive integer".into()));
    }
    if state.state_path != path_string(state_path)? {
        return Err(RunError::State(format!(
            "resume must use the persisted canonical state_path {}",
            state.state_path
        )));
    }
    if reject_redacted {
        reject_redacted_resume(&state.plan)?;
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

    validate_resume_dependencies(plan, &state.tasks)?;

    let mut attempt_ids = HashSet::new();
    let mut supervisor_paths = HashSet::new();
    for (index, task) in state.tasks.iter().enumerate() {
        let plan_task = &plan.tasks[index];
        validate_attempt_history(
            task,
            plan_task,
            &mut attempt_ids,
            &mut supervisor_paths,
            verify_artifacts,
        )?;
        validate_task_outputs(
            project_root,
            &state.run_id,
            plan_task,
            task,
            verify_artifacts,
        )?;
        let has_current = task.attempt_run_id.is_some() && task.supervisor_state_path.is_some();
        if task.attempt_run_id.is_some() != task.supervisor_state_path.is_some() {
            return Err(RunError::State(format!(
                "task {:?} has an incomplete current attempt identity",
                task.id
            )));
        }
        match task.status {
            TaskStatus::Pending => {
                if has_current || task.summary.is_some() || task.reason.is_some() {
                    return Err(RunError::State(format!(
                        "pending task {:?} has current attempt data",
                        task.id
                    )));
                }
                if let Some(ready_at) = &task.retry_ready_at {
                    if parse_retry_deadline(ready_at).is_none() {
                        return Err(RunError::State(format!(
                            "task {:?} retry_ready_at is invalid",
                            task.id
                        )));
                    }
                    let retryable = plan_task.retry.as_ref().is_some_and(|policy| {
                        task.attempts.last().is_some_and(|attempt| {
                            attempt.status == AttemptStatus::Failed
                                && attempt
                                    .summary
                                    .as_ref()
                                    .is_some_and(execution_summary_is_retryable_failure)
                                && task.attempts.len() < policy.max_attempts as usize
                        })
                    });
                    if !retryable {
                        return Err(RunError::State(format!(
                            "task {:?} has an invalid scheduled retry",
                            task.id
                        )));
                    }
                } else if !task.attempts.is_empty() {
                    return Err(RunError::State(format!(
                        "pending task {:?} has attempt history without a retry deadline",
                        task.id
                    )));
                }
            }
            TaskStatus::Blocked => {
                if has_current
                    || task.summary.is_some()
                    || !task.attempts.is_empty()
                    || task.retry_ready_at.is_some()
                {
                    return Err(RunError::State(format!(
                        "blocked task {:?} has execution data",
                        task.id
                    )));
                }
            }
            TaskStatus::Running => {
                if !has_current || task.summary.is_some() || task.retry_ready_at.is_some() {
                    return Err(RunError::State(format!(
                        "running task {:?} has inconsistent current attempt data",
                        task.id
                    )));
                }
                if task.attempts.last().map(|attempt| attempt.status)
                    != Some(AttemptStatus::Running)
                {
                    return Err(RunError::State(format!(
                        "running task {:?} has no running attempt",
                        task.id
                    )));
                }
            }
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled => {
                if !has_current || task.summary.is_none() || task.retry_ready_at.is_some() {
                    return Err(RunError::State(format!(
                        "terminal task {:?} has inconsistent current attempt data",
                        task.id
                    )));
                }
                let Some(summary) = &task.summary else {
                    unreachable!("terminal summary checked above");
                };
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
                if task.status == TaskStatus::Failed
                    && plan_task.retry.as_ref().is_some_and(|policy| {
                        task.attempts.last().is_some_and(|attempt| {
                            attempt.status == AttemptStatus::Failed
                                && attempt
                                    .summary
                                    .as_ref()
                                    .is_some_and(execution_summary_is_retryable_failure)
                                && task.attempts.len() < policy.max_attempts as usize
                        })
                    })
                {
                    return Err(RunError::State(format!(
                        "failed task {:?} has retryable work without retry deadline",
                        task.id
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
                let expected = match task.status {
                    TaskStatus::Succeeded => AttemptStatus::Succeeded,
                    TaskStatus::Failed => AttemptStatus::Failed,
                    TaskStatus::Cancelled => AttemptStatus::Cancelled,
                    _ => unreachable!(),
                };
                if task.attempts.last().map(|attempt| attempt.status) != Some(expected) {
                    return Err(RunError::State(format!(
                        "terminal task {:?} does not match its latest attempt",
                        task.id
                    )));
                }
                let Some(latest) = task.attempts.last() else {
                    return Err(RunError::State(format!(
                        "terminal task {:?} has no latest attempt",
                        task.id
                    )));
                };
                if task.reason != latest.reason || task.summary != latest.summary {
                    return Err(RunError::State(format!(
                        "terminal task {:?} does not match its latest attempt details",
                        task.id
                    )));
                }
            }
            TaskStatus::Indeterminate => {
                if !has_current || task.summary.is_some() || task.retry_ready_at.is_some() {
                    return Err(RunError::State(format!(
                        "indeterminate task {:?} has inconsistent current attempt data",
                        task.id
                    )));
                }
                if task.attempts.last().map(|attempt| attempt.status)
                    != Some(AttemptStatus::Indeterminate)
                {
                    return Err(RunError::State(format!(
                        "indeterminate task {:?} has no indeterminate attempt",
                        task.id
                    )));
                }
            }
        }
        if has_current {
            let Some(latest) = task.attempts.last() else {
                return Err(RunError::State(format!(
                    "task {:?} has current attempt without history",
                    task.id
                )));
            };
            if task.attempt_run_id.as_deref() != Some(latest.run_id.as_str())
                || task.supervisor_state_path.as_deref()
                    != Some(latest.supervisor_state_path.as_str())
            {
                return Err(RunError::State(format!(
                    "task {:?} current attempt does not match its history",
                    task.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_attempt_history<'a>(
    task: &'a TaskRecord,
    plan_task: &PlanTask,
    attempt_ids: &mut HashSet<&'a str>,
    supervisor_paths: &mut HashSet<&'a Path>,
    verify_live_paths: bool,
) -> Result<(), RunError> {
    if task.attempts.len() > MAX_RETRY_ATTEMPTS as usize {
        return Err(RunError::State(format!(
            "task {:?} has too many retained attempts",
            task.id
        )));
    }
    if plan_task.retry.is_none() && task.attempts.len() > 1 {
        return Err(RunError::State(format!(
            "task {:?} has retry history without retry policy",
            task.id
        )));
    }
    for (index, attempt) in task.attempts.iter().enumerate() {
        if index + 1 < task.attempts.len()
            && (attempt.status != AttemptStatus::Failed
                || !attempt
                    .summary
                    .as_ref()
                    .is_some_and(execution_summary_is_retryable_failure))
        {
            return Err(RunError::State(format!(
                "task {:?} has a non-retryable non-final attempt",
                task.id
            )));
        }
        let expected = u32::try_from(index + 1).map_err(|_| {
            RunError::State(format!("task {:?} attempt number overflowed", task.id))
        })?;
        if attempt.attempt != expected {
            return Err(RunError::State(format!(
                "task {:?} attempt numbers are not sequential",
                task.id
            )));
        }
        if attempt.run_id.parse::<Uuid>().is_err() {
            return Err(RunError::State(format!(
                "task {:?} has an invalid historical attempt_run_id",
                task.id
            )));
        }
        let supervisor_path = Path::new(&attempt.supervisor_state_path);
        validate_supervisor_state_path_shape(supervisor_path)?;
        if verify_live_paths {
            reject_symlinked_components(supervisor_path)?;
        }
        if supervisor_path.file_stem() != Some(OsStr::new(&attempt.run_id))
            || supervisor_path.extension() != Some(OsStr::new("json"))
        {
            return Err(RunError::State(format!(
                "task {:?} historical supervisor state path does not match its attempt identity",
                task.id
            )));
        }
        if !attempt_ids.insert(attempt.run_id.as_str()) || !supervisor_paths.insert(supervisor_path)
        {
            return Err(RunError::State(format!(
                "task {:?} reuses a historical attempt identity or supervisor state path",
                task.id
            )));
        }
        match attempt.status {
            AttemptStatus::Running => {
                if index + 1 != task.attempts.len()
                    || attempt.reason.is_some()
                    || attempt.summary.is_some()
                {
                    return Err(RunError::State(format!(
                        "task {:?} has an invalid running attempt history",
                        task.id
                    )));
                }
            }
            AttemptStatus::Indeterminate => {
                if index + 1 != task.attempts.len()
                    || attempt.reason.is_none()
                    || attempt.summary.is_some()
                {
                    return Err(RunError::State(format!(
                        "task {:?} has an invalid indeterminate attempt history",
                        task.id
                    )));
                }
            }
            AttemptStatus::Succeeded | AttemptStatus::Failed | AttemptStatus::Cancelled => {
                let Some(summary) = &attempt.summary else {
                    return Err(RunError::State(format!(
                        "task {:?} terminal attempt has no execution summary",
                        task.id
                    )));
                };
                let expected_status = match task_status_from_summary(summary) {
                    Some(TaskStatus::Succeeded) => AttemptStatus::Succeeded,
                    Some(TaskStatus::Failed) => AttemptStatus::Failed,
                    Some(TaskStatus::Cancelled) => AttemptStatus::Cancelled,
                    _ => {
                        return Err(RunError::State(format!(
                            "task {:?} has an invalid historical execution summary",
                            task.id
                        )));
                    }
                };
                if attempt.status != expected_status
                    || (attempt.status == AttemptStatus::Succeeded && attempt.reason.is_some())
                    || (attempt.status != AttemptStatus::Succeeded && attempt.reason.is_none())
                {
                    return Err(RunError::State(format!(
                        "task {:?} has inconsistent historical attempt data",
                        task.id
                    )));
                }
            }
        }
    }
    if task.attempts.len()
        > plan_task
            .retry
            .as_ref()
            .map_or(1, |policy| policy.max_attempts as usize)
    {
        return Err(RunError::State(format!(
            "task {:?} exceeds its retry policy",
            task.id
        )));
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
        }
    }
    Ok(())
}

fn parse_retry_deadline(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn next_ready(plan: &Plan, records: &[TaskRecord], now: DateTime<Utc>) -> Option<usize> {
    let statuses: HashMap<&str, TaskStatus> = records
        .iter()
        .map(|record| (record.id.as_str(), record.status))
        .collect();
    plan.tasks.iter().enumerate().find_map(|(index, task)| {
        let retry_ready = records[index]
            .retry_ready_at
            .as_deref()
            .is_none_or(|ready| {
                parse_retry_deadline(ready).is_some_and(|deadline| deadline <= now)
            });
        (records[index].status == TaskStatus::Pending
            && retry_ready
            && task
                .depends_on
                .iter()
                .all(|dependency| statuses[dependency.as_str()] == TaskStatus::Succeeded))
        .then_some(index)
    })
}

fn next_retry_wait(plan: &Plan, records: &[TaskRecord]) -> Option<std::time::Duration> {
    let now = Utc::now();
    let statuses: HashMap<&str, TaskStatus> = records
        .iter()
        .map(|record| (record.id.as_str(), record.status))
        .collect();
    let deadline = plan
        .tasks
        .iter()
        .enumerate()
        .filter(|(index, task)| {
            records[*index].status == TaskStatus::Pending
                && records[*index].retry_ready_at.is_some()
                && task
                    .depends_on
                    .iter()
                    .all(|dependency| statuses[dependency.as_str()] == TaskStatus::Succeeded)
        })
        .filter_map(|(index, _)| {
            records[index]
                .retry_ready_at
                .as_deref()
                .and_then(parse_retry_deadline)
        })
        .min();
    let remaining = deadline?
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_default();
    Some(remaining.min(std::time::Duration::from_millis(RETRY_POLL_INTERVAL_MS)))
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
    validate_supervisor_state_path_shape(path)?;
    reject_symlinked_components(path)
}

fn validate_supervisor_state_path_shape(path: &Path) -> Result<(), RunError> {
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
    Ok(())
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
        failure_code: result
            .failure
            .as_ref()
            .map(|failure| truncate(&failure.code, MAX_FAILURE_CODE_BYTES)),
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
    Some(termination_reason_name(result.termination_reason).into())
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

fn render(
    path: &Path,
    state: RunState,
    reconciliation: Vec<ReconciliationReport>,
    workflow_notice: Option<String>,
) -> RunResult {
    let mut human = format!(
        "Run {}: {}\nState: {}\nTasks:",
        state.run_id,
        state.status.as_str(),
        path.display()
    );
    if let Some(workflow) = state.workflow.as_ref() {
        human.push_str(&format!(
            "\nWorkflow: {}\nDefinition: {}",
            workflow.name,
            short_hash(&workflow.definition_sha256)
        ));
    }
    if let Some(notice) = workflow_notice.as_deref() {
        human.push_str(&format!("\nNotice: {notice}"));
    }
    for task in &state.tasks {
        human.push_str(&format!("\n  {}: {}", task.id, task.status.as_str()));
        if let Some(reason) = &task.reason {
            human.push_str(&format!(" ({reason})"));
        }
        if let Some(supervisor_path) = &task.supervisor_state_path {
            human.push_str(&format!(" [{}]", supervisor_path));
        }
        if let Some(outputs) = &task.outputs
            && !outputs.is_empty()
        {
            human.push_str("\n    outputs:");
            for output in outputs {
                let description = match output.output_type {
                    OutputType::Json => "json",
                    OutputType::File => output.path.as_str(),
                };
                human.push_str(&format!("\n      {} -> {}", output.name, description));
            }
        }
    }
    if !reconciliation.is_empty() {
        human.push_str("\nReconciliation:");
        for report in &reconciliation {
            human.push_str(&format!(
                "\n  {}: {} ({})",
                report.task_id,
                report.classification.as_str(),
                report.reason
            ));
        }
    }
    let status = state.status;
    let mut data = json!({
        "state_path": path.to_string_lossy(),
        "run": state,
        "reconciliation": reconciliation,
    });
    if let Some(notice) = workflow_notice {
        data["workflow_notice"] = Value::String(notice);
    }
    RunResult {
        status,
        output: RunOutput { human, data },
    }
}
fn short_hash(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

fn path_string(path: &Path) -> Result<String, RunError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| RunError::State(format!("path is not valid UTF-8: {}", path.display())))
}

fn now() -> String {
    Utc::now().to_rfc3339()
}
#[cfg(test)]
mod tests {
    use super::*;

    fn maximum_summary() -> ExecutionSummary {
        ExecutionSummary {
            lifecycle: Lifecycle::FailedToStart,
            termination_reason: TerminationReason::StartError,
            exit_code: None,
            failure_code: Some("\0".repeat(MAX_FAILURE_CODE_BYTES)),
            failure_message: Some("\0".repeat(MAX_SUMMARY_MESSAGE_BYTES)),
            accepted_at: "2026-01-01T00:00:00+00:00".into(),
            started_at: "2026-01-01T00:00:00+00:00".into(),
            completed_at: "2026-01-01T00:00:00+00:00".into(),
            duration_ms: 1,
            stdout_bytes_observed: 0,
            stdout_bytes_retained: 0,
            stdout_truncated: false,
            stderr_bytes_observed: 0,
            stderr_bytes_retained: 0,
            stderr_truncated: false,
            cancellation_requested: false,
        }
    }

    fn maximum_state() -> RunState {
        let plan = Plan {
            schema_version: PLAN_SCHEMA_VERSION,
            tasks: (0..MAX_TASKS)
                .map(|index| PlanTask {
                    id: format!("t{index:0>47}"),
                    depends_on: Vec::new(),
                    program: "/bin/true".into(),
                    arguments: if index == 0 {
                        vec![String::new(); 342_000]
                    } else {
                        Vec::new()
                    },
                    working_directory: Some("/tmp".into()),
                    timeout_ms: 1_000,
                    termination_grace_ms: default_grace_ms(),
                    stdout_limit_bytes: default_output_limit(),
                    stderr_limit_bytes: default_output_limit(),
                    log_limit_bytes: default_log_limit(),
                    redact_argument_indexes: Vec::new(),
                    retry: Some(RetryPolicy {
                        max_attempts: MAX_RETRY_ATTEMPTS,
                        delay_ms: 0,
                    }),
                    outputs: Vec::new(),
                    inputs: Vec::new(),
                })
                .collect(),
        };
        let tasks = (0..MAX_TASKS)
            .map(|index| {
                let attempts: Vec<_> = (1..=MAX_RETRY_ATTEMPTS)
                    .map(|attempt| {
                        let run_id = Uuid::from_u128(
                            0x0000_0000_0000_4000_8000_0000_0000_0000u128
                                + ((index as u128) * u128::from(MAX_RETRY_ATTEMPTS)
                                    + u128::from(attempt)),
                        )
                        .to_string();
                        let supervisor_state_path = format!("/tmp/{run_id}.json");
                        AttemptRecord {
                            attempt,
                            run_id,
                            supervisor_state_path,
                            status: AttemptStatus::Failed,
                            reason: Some("start_error".into()),
                            summary: Some(maximum_summary()),
                            outputs: None,
                        }
                    })
                    .collect();
                let latest = attempts.last().expect("maximum history is non-empty");
                TaskRecord {
                    id: format!("t{index:0>47}"),
                    status: TaskStatus::Failed,
                    reason: latest.reason.clone(),
                    attempt_run_id: Some(latest.run_id.clone()),
                    supervisor_state_path: Some(latest.supervisor_state_path.clone()),
                    summary: latest.summary.clone(),
                    outputs: None,
                    attempts,
                    retry_ready_at: None,
                }
            })
            .collect();
        RunState {
            schema_version: RUN_STATE_SCHEMA_VERSION,
            run_id: "00000000-0000-4000-8000-000000000001".into(),
            state_path: "/tmp/relay-state.json".into(),
            created_at: "2026-01-01T00:00:00+00:00".into(),
            updated_at: "2026-01-01T00:00:00+00:00".into(),
            status: RunStatus::Failed,
            cancellation_requested: false,
            jobs: 1,
            workflow: None,
            plan,
            tasks,
        }
    }

    #[test]
    fn maximum_retry_history_fits_aggregate_state_limit() {
        let state = maximum_state();
        let plan_bytes = serde_json::to_vec(&state.plan).unwrap();
        println!("maximum retry plan: {} bytes", plan_bytes.len());
        assert!(plan_bytes.len() < MAX_PLAN_BYTES as usize);
        assert!(plan_bytes.len() >= MAX_PLAN_BYTES as usize - 16_384);
        let mut state_without_plan = state.clone();
        state_without_plan.plan = Plan {
            schema_version: PLAN_SCHEMA_VERSION,
            tasks: Vec::new(),
        };
        let empty_plan_bytes = serde_json::to_vec(&state_without_plan.plan).unwrap().len();
        let fixed_per_task_attempt_overhead =
            encode_state(&state_without_plan).unwrap().len() - empty_plan_bytes;
        let worst_case_bound = MAX_PLAN_BYTES as usize + fixed_per_task_attempt_overhead;
        println!(
            "fixed per-task/attempt overhead: {} bytes; bound: {} bytes",
            fixed_per_task_attempt_overhead, worst_case_bound
        );
        assert!(worst_case_bound < MAX_STATE_BYTES as usize);
        validate_plan(&state.plan, Path::new("/tmp")).unwrap();
        validate_resume_state(
            &state,
            &state.plan,
            Path::new(&state.state_path),
            Path::new("/tmp"),
            true,
        )
        .unwrap();
        let encoded = encode_state(&state).unwrap();
        println!("maximum retry aggregate: {} bytes", encoded.len());
        assert_eq!(
            encoded.len(),
            plan_bytes.len() + fixed_per_task_attempt_overhead
        );
        assert!(encoded.len() <= worst_case_bound);
    }

    #[test]
    fn maximum_failure_diagnostics_are_bounded_once_per_attempt() {
        let state = maximum_state();
        for task in &state.tasks {
            for attempt in &task.attempts {
                assert_eq!(attempt.reason.as_deref(), Some("start_error"));
                assert_eq!(
                    attempt
                        .summary
                        .as_ref()
                        .and_then(|summary| summary.failure_code.as_deref())
                        .unwrap()
                        .len(),
                    MAX_FAILURE_CODE_BYTES
                );
                assert_eq!(
                    attempt
                        .summary
                        .as_ref()
                        .and_then(|summary| summary.failure_message.as_deref())
                        .unwrap()
                        .len(),
                    MAX_SUMMARY_MESSAGE_BYTES
                );
            }
        }
        let encoded = encode_state(&state).unwrap();
        println!("maximum diagnostic aggregate: {} bytes", encoded.len());
        assert!(encoded.len() < MAX_STATE_BYTES as usize);
    }

    #[test]
    fn maximum_structured_output_metadata_fits_aggregate_state_limit() {
        let mut state = maximum_state();
        let value = Value::String("x".repeat(MAX_STRUCTURED_OUTPUT_BYTES - 128));
        for (index, task) in state.tasks.iter_mut().enumerate() {
            let outputs = (0..MAX_OUTPUTS_PER_TASK)
                .map(|output_index| OutputRecord {
                    name: format!("out{index}_{output_index}"),
                    output_type: OutputType::Json,
                    path: format!("/tmp/output-{index}-{output_index}.json"),
                    size_bytes: MAX_STRUCTURED_OUTPUT_BYTES as u64,
                    sha256: Some("0".repeat(64)),
                    value: value.clone(),
                    run_id: state.run_id.clone(),
                    producer_task: task.id.clone(),
                    attempt: 1,
                    attempt_run_id: "00000000-0000-4000-8000-000000000001".into(),
                })
                .collect::<Vec<_>>();
            task.outputs = Some(outputs.clone());
            if let Some(attempt) = task.attempts.last_mut() {
                attempt.outputs = Some(outputs);
            }
        }
        let encoded = encode_state(&state).unwrap();
        println!(
            "maximum structured output aggregate: {} bytes",
            encoded.len()
        );
        assert!(encoded.len() < MAX_STATE_BYTES as usize);
    }

    #[test]
    fn oversized_artifact_is_rejected_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("artifact.bin");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_ARTIFACT_FILE_BYTES + 1).unwrap();
        let error = read_hashed_output(&path, MAX_ARTIFACT_FILE_BYTES, false).unwrap_err();
        assert!(error.contains("exceeds"));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_artifact_is_rejected_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("artifact.fifo");
        let path_string = CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path_string.as_ptr(), 0o600) }, 0);
        let error = read_hashed_output(&path, MAX_ARTIFACT_FILE_BYTES, false).unwrap_err();
        assert!(error.contains("regular file"));
    }
    #[test]
    fn encoded_plan_size_is_bounded_before_persistence() {
        let plan = Plan {
            schema_version: PLAN_SCHEMA_VERSION,
            tasks: vec![PlanTask {
                id: "oversized".into(),
                depends_on: Vec::new(),
                program: "/bin/true".into(),
                arguments: vec!["x".repeat(MAX_PLAN_BYTES as usize)],
                working_directory: Some("/tmp".into()),
                timeout_ms: 1,
                termination_grace_ms: default_grace_ms(),
                stdout_limit_bytes: default_output_limit(),
                stderr_limit_bytes: default_output_limit(),
                log_limit_bytes: default_log_limit(),
                redact_argument_indexes: Vec::new(),
                retry: None,
                outputs: Vec::new(),
                inputs: Vec::new(),
            }],
        };
        let error = validate_encoded_plan_size(&plan).unwrap_err();
        assert!(error.to_string().contains("encoded plan exceeds"));
    }
}
