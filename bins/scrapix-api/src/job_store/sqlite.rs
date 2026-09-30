//! SQLite job store: the standalone default (`sqlite://./data/scrapix.db`).

use std::str::FromStr;

use scrapix_core::JobState;
use serde_json::Value;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow,
};
use sqlx::Row;
use tracing::{debug, warn};

use super::{status_to_str, str_to_status, JobStore, StoreError};

/// Standalone-only migrations: the engine's own `jobs`/`job_results` schema,
/// tracked in `_sqlx_migrations`.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");

/// [`JobStore`] over a SQLite pool.
pub struct SqliteJobStore {
    pool: SqlitePool,
}

fn other(e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(e.to_string())
}

fn ts(v: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
    v.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn parse_ts(v: Option<String>) -> Option<chrono::DateTime<chrono::Utc>> {
    v.and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
}

fn json_text(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

fn row_to_job(row: &SqliteRow) -> JobState {
    let job_id: String = row.get("job_id");
    let start_urls: String = row.get("start_urls");
    let config: Option<String> = row.get("config");
    JobState {
        status: str_to_status(&row.get::<String, _>("status")),
        index_uid: row.get("index_uid"),
        account_id: row.get("account_id"),
        api_key_id: row.get("api_key_id"),
        pages_crawled: row.get::<i64, _>("pages_crawled") as u64,
        pages_indexed: row.get::<i64, _>("pages_indexed") as u64,
        documents_sent: row.get::<i64, _>("documents_sent") as u64,
        errors: row.get::<i64, _>("errors") as u64,
        bytes_downloaded: row.get::<i64, _>("bytes_downloaded") as u64,
        started_at: parse_ts(row.get("started_at")),
        completed_at: parse_ts(row.get("completed_at")),
        error_message: row.get("error_message"),
        crawl_rate: row.get("crawl_rate"),
        eta_seconds: row.get::<Option<i64>, _>("eta_seconds").map(|v| v as u64),
        start_urls: serde_json::from_str(&start_urls).unwrap_or_else(|e| {
            warn!(job_id = %job_id, error = %e, "Failed to deserialize start_urls from SQLite");
            Vec::new()
        }),
        max_pages: row.get::<Option<i64>, _>("max_pages").map(|v| v as u64),
        config: config.and_then(|c| serde_json::from_str(&c).ok()),
        swap_temp_index: row.get("swap_temp_index"),
        swap_meilisearch_url: row.get("swap_meilisearch_url"),
        swap_meilisearch_api_key: row.get("swap_meilisearch_api_key"),
        warnings: Vec::new(),
        // Webhook auth secrets are never persisted; a job recovered from
        // SQLite after a restart has no real webhooks to deliver to.
        webhooks: Vec::new(),
        job_id,
    }
}

impl SqliteJobStore {
    /// Opens (creating the file and its parent directory) and migrates.
    pub async fn open(url: &str) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::from_str(url)
            .map_err(|e| other(format!("invalid DATABASE_URL `{url}`: {e}")))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        let path = opts.get_filename().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| {
                other(format!(
                    "cannot create directory {} for the job store: {e}",
                    parent.display()
                ))
            })?;
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .map_err(|e| {
                other(format!(
                    "cannot open SQLite job store {}: {e}",
                    path.display()
                ))
            })?;
        MIGRATOR.run(&pool).await.map_err(|e| {
            other(format!(
                "SQLite job store migration failed ({}): {e}",
                path.display()
            ))
        })?;
        Ok(Self { pool })
    }
}

#[async_trait::async_trait]
impl JobStore for SqliteJobStore {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    // ========================================================================
    // Insert / Update
    // ========================================================================

    async fn insert_job(&self, job: &JobState) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO jobs (job_id, status, index_uid, account_id, api_key_id,
                pages_crawled, pages_indexed, documents_sent, errors, bytes_downloaded,
                started_at, completed_at, crawl_rate, eta_seconds, error_message,
                start_urls, max_pages, config, swap_temp_index, swap_meilisearch_url, swap_meilisearch_api_key)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT (job_id) DO NOTHING",
        )
        .bind(&job.job_id)
        .bind(status_to_str(&job.status))
        .bind(&job.index_uid)
        .bind(&job.account_id)
        .bind(&job.api_key_id)
        .bind(job.pages_crawled as i64)
        .bind(job.pages_indexed as i64)
        .bind(job.documents_sent as i64)
        .bind(job.errors as i64)
        .bind(job.bytes_downloaded as i64)
        .bind(ts(job.started_at))
        .bind(ts(job.completed_at))
        .bind(job.crawl_rate)
        .bind(job.eta_seconds.map(|v| v as i64))
        .bind(&job.error_message)
        .bind(serde_json::to_string(&job.start_urls).unwrap_or_else(|_| "[]".into()))
        .bind(job.max_pages.map(|v| v as i64))
        .bind(job.config.as_ref().map(json_text))
        .bind(&job.swap_temp_index)
        .bind(&job.swap_meilisearch_url)
        .bind(&job.swap_meilisearch_api_key)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|e| {
            warn!(job_id = %job.job_id, error = %e, "Failed to insert job into SQLite");
            other(e)
        })
    }

    async fn update_job_full(&self, job: &JobState) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE jobs SET status = ?, pages_crawled = ?, pages_indexed = ?, documents_sent = ?,
                errors = ?, bytes_downloaded = ?, started_at = ?, completed_at = ?,
                crawl_rate = ?, eta_seconds = ?, error_message = ?
             WHERE job_id = ?",
        )
        .bind(status_to_str(&job.status))
        .bind(job.pages_crawled as i64)
        .bind(job.pages_indexed as i64)
        .bind(job.documents_sent as i64)
        .bind(job.errors as i64)
        .bind(job.bytes_downloaded as i64)
        .bind(ts(job.started_at))
        .bind(ts(job.completed_at))
        .bind(job.crawl_rate)
        .bind(job.eta_seconds.map(|v| v as i64))
        .bind(&job.error_message)
        .bind(&job.job_id)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|e| {
            warn!(job_id = %job.job_id, error = %e, "Failed to update job in SQLite");
            other(e)
        })
    }

    async fn flush_job_counters(&self, snapshots: &[JobState]) -> Result<(), StoreError> {
        if snapshots.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(other)?;
        let mut rows_affected = 0u64;
        for j in snapshots {
            let result = sqlx::query(
                "UPDATE jobs SET status = ?, pages_crawled = ?, pages_indexed = ?, documents_sent = ?,
                    errors = ?, bytes_downloaded = ?, crawl_rate = ?, eta_seconds = ?
                 WHERE job_id = ? AND status NOT IN ('completed','failed','cancelled')
                   AND ? NOT IN ('completed','failed','cancelled')",
            )
            .bind(status_to_str(&j.status))
            .bind(j.pages_crawled as i64)
            .bind(j.pages_indexed as i64)
            .bind(j.documents_sent as i64)
            .bind(j.errors as i64)
            .bind(j.bytes_downloaded as i64)
            .bind(j.crawl_rate)
            .bind(j.eta_seconds.map(|v| v as i64))
            .bind(&j.job_id)
            .bind(status_to_str(&j.status))
            .execute(&mut *tx)
            .await;
            match result {
                Ok(r) => rows_affected += r.rows_affected(),
                Err(e) => {
                    warn!(error = %e, "Failed to flush job counters to SQLite");
                    return Err(other(e));
                }
            }
        }
        tx.commit().await.map_err(|e| {
            warn!(error = %e, "Failed to flush job counters to SQLite");
            other(e)
        })?;
        debug!(rows = rows_affected, "Flushed job counters to SQLite");
        Ok(())
    }

    async fn flush_job_accounting(&self, entries: &[(String, Value)]) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(other)?;
        let mut rows_affected = 0u64;
        for (id, acc) in entries {
            let result = sqlx::query("UPDATE jobs SET accounting = ? WHERE job_id = ?")
                .bind(json_text(acc))
                .bind(id)
                .execute(&mut *tx)
                .await;
            match result {
                Ok(r) => rows_affected += r.rows_affected(),
                Err(e) => {
                    warn!(error = %e, "Failed to flush job accounting to SQLite");
                    return Err(other(e));
                }
            }
        }
        tx.commit().await.map_err(|e| {
            warn!(error = %e, "Failed to flush job accounting to SQLite");
            other(e)
        })?;
        debug!(rows = rows_affected, "Flushed job accounting to SQLite");
        Ok(())
    }

    // ========================================================================
    // Reads
    // ========================================================================

    async fn load_active_jobs(&self) -> Vec<JobState> {
        sqlx::query("SELECT * FROM jobs WHERE status IN ('pending','running','paused') ORDER BY created_at, rowid")
            .fetch_all(&self.pool)
            .await
            .map(|rows| rows.iter().map(row_to_job).collect())
            .unwrap_or_else(|e| {
                warn!(error = %e, "Failed to load active jobs from SQLite");
                Vec::new()
            })
    }

    async fn load_active_job_accounting(&self) -> Vec<(String, Value)> {
        sqlx::query("SELECT job_id, accounting FROM jobs WHERE status IN ('running','paused')")
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        let id: String = r.try_get("job_id").ok()?;
                        let acc: String = r.try_get("accounting").ok()?;
                        Some((id, serde_json::from_str(&acc).ok()?))
                    })
                    .collect()
            })
            .unwrap_or_else(|e| {
                warn!(error = %e, "Failed to load job accounting from SQLite");
                Vec::new()
            })
    }

    async fn get_job(&self, job_id: &str, account_id: Option<&str>) -> Option<JobState> {
        let q = match account_id {
            None => sqlx::query("SELECT * FROM jobs WHERE job_id = ?").bind(job_id),
            Some(a) => sqlx::query("SELECT * FROM jobs WHERE job_id = ? AND account_id = ?")
                .bind(job_id)
                .bind(a),
        };
        q.fetch_optional(&self.pool)
            .await
            .ok()
            .flatten()
            .map(|r| row_to_job(&r))
    }

    async fn list_jobs(&self, account_id: Option<&str>, limit: i64, offset: i64) -> Vec<JobState> {
        let q = match account_id {
            None => sqlx::query("SELECT * FROM jobs ORDER BY created_at DESC, rowid DESC LIMIT ? OFFSET ?")
                .bind(limit)
                .bind(offset),
            Some(a) => sqlx::query(
                "SELECT * FROM jobs WHERE account_id = ? ORDER BY created_at DESC, rowid DESC LIMIT ? OFFSET ?",
            )
            .bind(a)
            .bind(limit)
            .bind(offset),
        };
        q.fetch_all(&self.pool)
            .await
            .map(|rows| rows.iter().map(row_to_job).collect())
            .unwrap_or_else(|e| {
                warn!(error = %e, "Failed to list jobs from SQLite");
                Vec::new()
            })
    }

    async fn active_job_ids(&self, account_id: &str) -> Result<Vec<String>, StoreError> {
        sqlx::query_scalar(
            "SELECT job_id FROM jobs WHERE account_id = ? AND status IN ('pending','running')",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(other)
    }

    // ========================================================================
    // Engine-job results (`job_results`)
    // ========================================================================

    async fn store_result_page(
        &self,
        job_id: &str,
        seq: u64,
        url: &str,
        success: bool,
        payload: &Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO job_results (job_id, seq, kind, url, success, payload)
             VALUES (?, ?, 'page', ?, ?, ?) ON CONFLICT (job_id, seq) DO NOTHING",
        )
        .bind(job_id)
        .bind(seq as i64)
        .bind(url)
        .bind(success)
        .bind(json_text(payload))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(other)
    }

    async fn store_result_summary(&self, job_id: &str, payload: &Value) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO job_results (job_id, seq, kind, url, success, payload)
             VALUES (?, 0, 'extract', NULL, 1, ?)
             ON CONFLICT (job_id, seq) DO UPDATE SET payload = excluded.payload",
        )
        .bind(job_id)
        .bind(json_text(payload))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(other)
    }

    async fn load_result_summary(&self, job_id: &str) -> Result<Option<Value>, StoreError> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT payload FROM job_results WHERE job_id = ? AND kind = 'extract' LIMIT 1",
        )
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(other)?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    async fn result_pages(
        &self,
        job_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<(Vec<(u64, Value)>, u64), StoreError> {
        let rows = sqlx::query(
            "SELECT seq, payload FROM job_results WHERE job_id = ? AND kind = 'page' AND seq > ? ORDER BY seq LIMIT ?",
        )
        .bind(job_id)
        .bind(after as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(other)?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM job_results WHERE job_id = ? AND kind = 'page'",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await
        .map_err(other)?;
        let data = rows
            .iter()
            .map(|r| {
                let payload: String = r.get("payload");
                (
                    r.get::<i64, _>("seq") as u64,
                    serde_json::from_str(&payload).unwrap_or(Value::Null),
                )
            })
            .collect();
        Ok((data, total as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/scrapix.db");
        SqliteJobStore::open(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn unwritable_path_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, b"x").unwrap(); // a file where a directory is needed
        let url = format!("sqlite://{}", blocker.join("scrapix.db").display());
        let err = SqliteJobStore::open(&url).await.err().expect("must fail");
        assert!(
            err.to_string().contains(&blocker.display().to_string()),
            "{err}"
        );
    }

    #[tokio::test]
    async fn reopen_keeps_rows() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("r.db").display());
        let s = SqliteJobStore::open(&url).await.unwrap();
        s.insert_job(&crate::job_store::conformance::job("keep"))
            .await
            .unwrap();
        drop(s);
        let s = SqliteJobStore::open(&url).await.unwrap();
        assert!(s.get_job("keep", None).await.is_some());
    }
}
