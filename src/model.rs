use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;

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
