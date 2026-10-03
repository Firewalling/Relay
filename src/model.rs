use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;

pub const OUTPUT_SCHEMA_VERSION: u32 = 1;
pub const ORCHESTRATION_PROTOCOL_VERSION: u32 = 1;
pub const HANDOFF_SCHEMA_VERSION: u32 = 2;
pub const WORKTREE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolManifest {
    pub protocol_version: u32,
    pub relay_version: String,
    pub transport: ProtocolTransport,
    pub state: ProtocolState,
    pub orchestrator: ProtocolComponent,
    pub workflow: ProtocolComponent,
    pub operations: Vec<ProtocolOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolTransport {
    pub kind: String,
    pub invocation: String,
    pub framing: String,
    pub shell: bool,
    pub invocation_prefix: Vec<String>,
    pub success_stream: String,
    pub error_stream: String,
    pub output_schema_version: u32,
    pub one_document: bool,
    pub success_exit_code: u32,
    pub failure_exit_code_nonzero: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolState {
    pub owner: String,
    pub schema_version: u32,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolComponent {
    pub owner: String,
    pub implemented: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolOperation {
    pub name: String,
    pub argv_template: Vec<String>,
    pub mutating: bool,
    pub stdin: ProtocolStdin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolStdin {
    None,
    Utf8Note,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub schema_version: u32,
    pub id: Uuid,
    pub goal: String,
    pub state: SessionState,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub initial_git: GitSnapshot,
    pub final_git: Option<GitSnapshot>,
    pub last_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub id: Uuid,
    pub session_id: Uuid,
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub id: Uuid,
    pub session_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub message: Option<String>,
    pub git: GitSnapshot,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSnapshot {
    pub schema_version: u32,
    pub available: bool,
    pub repository_root: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub clean: Option<bool>,
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_path_encoding: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub changed_files_truncated: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub changed_files_omitted: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeProvenance {
    pub schema_version: u32,
    pub repository_id: String,
    pub repository_root: String,
    pub worktree_id: String,
    pub worktree_path: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub ref_name: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    pub detached: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRecord {
    pub schema_version: u32,
    pub id: Uuid,
    pub artifact_path: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<HandoffSessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_worktree: Option<WorktreeProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
    pub target_agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_worktree: Option<WorktreeProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default)]
    pub completed: Vec<HandoffEntry>,
    #[serde(default)]
    pub decisions: Vec<HandoffEntry>,
    #[serde(default)]
    pub blockers: Vec<HandoffEntry>,
    #[serde(default)]
    pub next_actions: Vec<HandoffEntry>,
    pub references: HandoffReferences,
    pub git: GitSnapshot,
    #[serde(default)]
    pub unavailable: Vec<HandoffUnavailable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffEntry {
    pub text: String,
    pub source: HandoffEntrySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffEntrySource {
    Explicit,
    RecordedNote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffUnavailable {
    SourceSession,
    SourceAgent,
    Goal,
    Completed,
    Decisions,
    Blockers,
    NextActions,
    Git,
    Checkpoints,
    Notes,
    Runs,
    Outputs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffSessionSnapshot {
    pub id: Uuid,
    pub goal: String,
    pub state: SessionState,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeProvenance>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffReferences {
    #[serde(default)]
    pub checkpoints: Vec<HandoffCheckpointReference>,
    #[serde(default)]
    pub notes: Vec<HandoffNoteReference>,
    #[serde(default)]
    pub runs: Vec<HandoffRunReference>,
    #[serde(default)]
    pub outputs: Vec<HandoffOutputReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffCheckpointReference {
    pub id: Uuid,
    pub session_id: Uuid,
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    pub path: String,
    pub git: GitSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffNoteReference {
    pub id: Uuid,
    pub session_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub category: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRunReference {
    pub run_id: String,
    pub state_path: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<HandoffWorkflowSnapshot>,
    #[serde(default)]
    pub tasks: Vec<HandoffTaskSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffWorkflowSnapshot {
    pub name: String,
    pub schema_version: u32,
    pub definition_sha256: String,
    pub inputs: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffTaskSnapshot {
    pub id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_run_id: Option<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffOutputReference {
    pub run_id: String,
    pub task: String,
    pub output: String,
    pub attempt: u32,
    pub attempt_run_id: String,
    #[serde(rename = "type")]
    pub output_type: HandoffOutputType,
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffOutputType {
    Json,
    File,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockOwner {
    pub schema_version: u32,
    pub pid: u32,
    pub command: String,
    pub acquired_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEffect {
    pub kind: String,
    pub path: String,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationMarker {
    pub schema_version: u32,
    pub id: Uuid,
    pub command: String,
    pub session_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub effects: Vec<OperationEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: DiagnosticSeverity,
    pub path: String,
    pub message: String,
    pub guidance: String,
    pub repairable: bool,
}

#[derive(Debug, Serialize)]
pub struct SuccessEnvelope<T: Serialize> {
    pub output_schema_version: u32,
    pub ok: bool,
    pub command: String,
    pub data: T,
}

#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    pub output_schema_version: u32,
    pub ok: bool,
    pub command: String,
    pub error: OutputError,
}

#[derive(Debug, Serialize)]
pub struct OutputError {
    pub code: String,
    pub message: String,
}

impl GitSnapshot {
    pub fn unavailable() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ..Self::default()
        }
    }
}
