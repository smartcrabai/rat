use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::schedule::Schedule;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NewJob {
    pub command: String,
    pub cwd: PathBuf,
    pub path: String,
    pub env: BTreeMap<String, String>,
    pub schedule: Schedule,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobState {
    Active,
    Paused,
    NeedsReview,
    Completed,
    Deleted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: i64,
    pub definition: NewJob,
    pub state: JobState,
    pub next_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    Starting,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Uncertain,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub id: i64,
    pub job_id: i64,
    pub definition: NewJob,
    pub scheduled_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub truncated: bool,
    pub output_missing: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RunCompletion {
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub truncated: bool,
    pub output_missing: bool,
    pub message: Option<String>,
}
