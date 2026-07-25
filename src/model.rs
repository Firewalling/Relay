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
}

impl GitSnapshot {
    pub fn unavailable() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ..Self::default()
        }
    }
}
