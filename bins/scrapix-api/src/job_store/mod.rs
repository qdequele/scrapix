//! Durable job state (`jobs`) and engine-job results (`job_results`).
//!
//! Write-through cache: the in-memory maps in `AppState` stay the primary
//! read path; a store gives durability across restarts. Hosted mode uses
//! the Rails-owned Postgres schema; standalone uses engine-owned migrations
//! (SQLite by default, or a dedicated Postgres).

#[cfg(test)]
mod conformance;
pub mod postgres;
pub mod sqlite; // added in Task 6

use scrapix_core::{JobState, JobStatus};

pub use postgres::PgJobStore;

#[derive(Debug)]
pub enum StoreError {
    /// The table/column the write needs does not exist (e.g. the Rails
    /// migration has not run). Not retryable.
    SchemaMissing(String),
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SchemaMissing(m) => write!(f, "schema missing: {m}"),
            Self::Other(m) => f.write_str(m),
        }
    }
}

pub(crate) fn status_to_str(status: &JobStatus) -> &'static str {
    match status {
        JobStatus::Pending => "pending",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
        JobStatus::Paused => "paused",
    }
}

pub(crate) fn str_to_status(s: &str) -> JobStatus {
    match s {
        "running" => JobStatus::Running,
        "completed" => JobStatus::Completed,
        "failed" => JobStatus::Failed,
        "cancelled" => JobStatus::Cancelled,
        "paused" => JobStatus::Paused,
        _ => JobStatus::Pending,
    }
}

/// Persistence for crawl/engine job state and engine-job results.
///
/// Failures are logged by the store; methods returning `Result` also hand
/// the error back so the caller can retry or degrade.
#[async_trait::async_trait]
pub trait JobStore: Send + Sync {
    /// `"postgres"` or `"sqlite"` (for logs).
    fn backend(&self) -> &'static str;
    /// Insert a new job row; an existing row with the same id is kept.
    async fn insert_job(&self, job: &JobState) -> Result<(), StoreError>;
    /// Full update of a job's mutable fields (lifecycle events: complete,
    /// fail, cancel).
    async fn update_job_full(&self, job: &JobState) -> Result<(), StoreError>;
    /// Batch-update the counters of dirty jobs. Rows already terminal are
    /// left alone (terminal rows are only written by `update_job_full`).
    async fn flush_job_counters(&self, snapshots: &[JobState]) -> Result<(), StoreError>;
    /// Persist per-job work-accounting snapshots.
    async fn flush_job_accounting(
        &self,
        entries: &[(String, serde_json::Value)],
    ) -> Result<(), StoreError>;
    /// Active (pending/running/paused) jobs, for startup recovery.
    async fn load_active_jobs(&self) -> Vec<JobState>;
    /// Persisted accounting of running/paused jobs, for startup recovery.
    async fn load_active_job_accounting(&self) -> Vec<(String, serde_json::Value)>;
    /// One job, scoped to `account_id` when given.
    async fn get_job(&self, job_id: &str, account_id: Option<&str>) -> Option<JobState>;
    /// Jobs newest first, scoped to `account_id` when given.
    async fn list_jobs(&self, account_id: Option<&str>, limit: i64, offset: i64) -> Vec<JobState>;
    /// Pending/running jobs of an account (concurrent-job quota).
    async fn count_active_jobs(&self, account_id: &str) -> Result<i64, StoreError>;
    /// Store result `seq` of an engine-run job (an existing `seq` is kept).
    async fn store_result_page(
        &self,
        job_id: &str,
        seq: u64,
        url: &str,
        success: bool,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError>;
    /// Store (replace) the summary of an extract job.
    async fn store_result_summary(
        &self,
        job_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError>;
    /// The summary of an extract job, if stored.
    async fn load_result_summary(
        &self,
        job_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError>;
    /// Pages with seq > `after`, ascending, at most `limit`; plus the total page count.
    async fn result_pages(
        &self,
        job_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<(Vec<(u64, serde_json::Value)>, u64), StoreError>;
}
