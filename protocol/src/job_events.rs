use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const JOB_EVENT_SCHEMA_NAME: &str = "protocol::JobEvent";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobEventKind {
    System,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct JobEvent {
    pub job_id: Uuid,
    pub worker_id: String,
    pub at: DateTime<Utc>,
    pub kind: JobEventKind,
    #[serde(default)]
    pub stream: Option<String>,
    pub message: String,
}
