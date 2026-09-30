//! PostgreSQL job store: `jobs`, `job_results` and `lab_events` in the
//! engine's own database, in both modes (never the Lab's database).

use scrapix_core::JobState;
use tracing::{debug, warn};

use super::{status_to_str, str_to_status, JobStore, StoreError};

/// Postgres SQLSTATEs meaning the schema is missing (42703
/// undefined_column, 42P01 undefined_table).
pub(crate) fn is_schema_missing_sqlstate(code: Option<&str>) -> bool {
    matches!(code, Some("42703") | Some("42P01"))
}

fn store_err(e: sqlx::Error) -> StoreError {
    match &e {
        sqlx::Error::Database(db) if is_schema_missing_sqlstate(db.code().as_deref()) => {
            StoreError::SchemaMissing(e.to_string())
        }
        _ => StoreError::Other(e.to_string()),
    }
}

/// [`JobStore`] over a Postgres pool.
pub struct PgJobStore {
    pool: sqlx::PgPool,
}

impl PgJobStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

/// Migrations of the engine's own database, in both modes (`jobs`,
/// `job_results`, `lab_events`), tracked in `_sqlx_migrations`.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

impl PgJobStore {
    /// Apply the schema of the engine's own database, in both modes
    /// (tracked in `_sqlx_migrations`).
    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR
            .run(&self.pool)
            .await
            .map_err(|e| StoreError::Other(format!("Postgres job store migration failed: {e}")))
    }

    /// A Rails database has `schema_migrations` in the current search path.
    pub async fn is_rails_database(&self) -> Result<bool, StoreError> {
        sqlx::query_scalar::<_, bool>("SELECT to_regclass('schema_migrations') IS NOT NULL")
            .fetch_one(&self.pool)
            .await
            .map_err(store_err)
    }
}

// ============================================================================
// Row → JobState conversion
// ============================================================================

fn row_to_job_state(row: &sqlx::postgres::PgRow) -> JobState {
    use sqlx::Row;
    let status_str: String = row.get("status");
    let start_urls: serde_json::Value = row.get("start_urls");
    let account_id: Option<uuid::Uuid> = row.get("account_id");

    JobState {
        job_id: row.get("job_id"),
        status: str_to_status(&status_str),
        index_uid: row.get("index_uid"),
        account_id: account_id.map(|u| u.to_string()),
        api_key_id: row.try_get::<Option<uuid::Uuid>, _>("api_key_id").ok().flatten().map(|u| u.to_string()),
        pages_crawled: row.get::<i64, _>("pages_crawled") as u64,
        pages_indexed: row.get::<i64, _>("pages_indexed") as u64,
        documents_sent: row.get::<i64, _>("documents_sent") as u64,
        errors: row.get::<i64, _>("errors") as u64,
        bytes_downloaded: row.get::<i64, _>("bytes_downloaded") as u64,
        started_at: row.get("started_at"),
        completed_at: row.get("completed_at"),
        error_message: row.get("error_message"),
        crawl_rate: row.get("crawl_rate"),
        eta_seconds: row.get::<Option<i64>, _>("eta_seconds").map(|v| v as u64),
        start_urls: serde_json::from_value(start_urls).unwrap_or_else(|e| {
            warn!(job_id = %row.get::<String, _>("job_id"), error = %e, "Failed to deserialize start_urls from database");
            Vec::new()
        }),
        max_pages: row.get::<Option<i64>, _>("max_pages").map(|v| v as u64),
        config: row.get("config"),
        swap_temp_index: row.get("swap_temp_index"),
        swap_meilisearch_url: row.get("swap_meilisearch_url"),
        swap_meilisearch_api_key: row.get("swap_meilisearch_api_key"),
        warnings: Vec::new(),
        // Webhook auth secrets are never persisted (redacted before the
        // `config` blob is stored); a job recovered from Postgres after a
        // restart has no real webhooks to deliver to.
        webhooks: Vec::new(),
    }
}

#[async_trait::async_trait]
impl JobStore for PgJobStore {
    fn backend(&self) -> &'static str {
        "postgres"
    }

    fn lab_outbox(&self) -> std::sync::Arc<dyn crate::lab_events::LabOutbox> {
        std::sync::Arc::new(crate::lab_events::PgOutbox::new(self.pool.clone()))
    }

    // ========================================================================
    // Insert / Update
    // ========================================================================

    /// Insert a new job row (ON CONFLICT DO NOTHING), returning the error, if
    /// any, after logging it.
    async fn insert_job(&self, job: &JobState) -> Result<(), StoreError> {
        let account_id: Option<uuid::Uuid> = job.account_id.as_deref().and_then(|s| s.parse().ok());
        let api_key_id: Option<uuid::Uuid> = job.api_key_id.as_deref().and_then(|s| s.parse().ok());

        let start_urls = serde_json::to_value(&job.start_urls).unwrap_or_default();

        let result = sqlx::query(
            "INSERT INTO jobs (
            job_id, status, index_uid, account_id, api_key_id,
            pages_crawled, pages_indexed, documents_sent, errors, bytes_downloaded,
            started_at, completed_at, crawl_rate, eta_seconds,
            error_message, start_urls, max_pages, config,
            swap_temp_index, swap_meilisearch_url, swap_meilisearch_api_key
        ) VALUES (
            $1, $2, $3, $4, $5,
            $6, $7, $8, $9, $10,
            $11, $12, $13, $14,
            $15, $16, $17, $18,
            $19, $20, $21
        ) ON CONFLICT (job_id) DO NOTHING",
        )
        .bind(&job.job_id)
        .bind(status_to_str(&job.status))
        .bind(&job.index_uid)
        .bind(account_id)
        .bind(api_key_id)
        .bind(job.pages_crawled as i64)
        .bind(job.pages_indexed as i64)
        .bind(job.documents_sent as i64)
        .bind(job.errors as i64)
        .bind(job.bytes_downloaded as i64)
        .bind(job.started_at)
        .bind(job.completed_at)
        .bind(job.crawl_rate)
        .bind(job.eta_seconds.map(|v| v as i64))
        .bind(&job.error_message)
        .bind(&start_urls)
        .bind(job.max_pages.map(|v| v as i64))
        .bind(&job.config)
        .bind(&job.swap_temp_index)
        .bind(&job.swap_meilisearch_url)
        .bind(&job.swap_meilisearch_api_key)
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!(job_id = %job.job_id, error = %e, "Failed to insert job into Postgres");
                Err(store_err(e))
            }
        }
    }

    /// Full update of a single job's mutable fields (lifecycle events: complete, fail, cancel).
    ///
    /// Returns `Err` (after logging) so the flush can retry an owed terminal
    /// write before releasing the job's held acks.
    async fn update_job_full(&self, job: &JobState) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE jobs SET
            status = $2,
            pages_crawled = $3, pages_indexed = $4, documents_sent = $5,
            errors = $6, bytes_downloaded = $7,
            started_at = $8, completed_at = $9,
            crawl_rate = $10, eta_seconds = $11,
            error_message = $12
        WHERE job_id = $1",
        )
        .bind(&job.job_id)
        .bind(status_to_str(&job.status))
        .bind(job.pages_crawled as i64)
        .bind(job.pages_indexed as i64)
        .bind(job.documents_sent as i64)
        .bind(job.errors as i64)
        .bind(job.bytes_downloaded as i64)
        .bind(job.started_at)
        .bind(job.completed_at)
        .bind(job.crawl_rate)
        .bind(job.eta_seconds.map(|v| v as i64))
        .bind(&job.error_message)
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!(job_id = %job.job_id, error = %e, "Failed to update job in Postgres");
                Err(store_err(e))
            }
        }
    }

    /// Batch-update counters for dirty jobs in a single round-trip using `unnest` arrays.
    ///
    /// Rows already terminal are skipped: a counter snapshot taken before a
    /// job finished must not overwrite its terminal row, which only
    /// [`update_job_full`](JobStore::update_job_full) writes. Terminal
    /// snapshots are skipped too, for the same reason: the terminal status
    /// is persisted only by `update_job_full` (after the job's Lab events).
    async fn flush_job_counters(&self, snapshots: &[JobState]) -> Result<(), StoreError> {
        if snapshots.is_empty() {
            return Ok(());
        }

        let ids: Vec<&str> = snapshots.iter().map(|j| j.job_id.as_str()).collect();
        let statuses: Vec<&str> = snapshots.iter().map(|j| status_to_str(&j.status)).collect();
        let pages_crawled: Vec<i64> = snapshots.iter().map(|j| j.pages_crawled as i64).collect();
        let pages_indexed: Vec<i64> = snapshots.iter().map(|j| j.pages_indexed as i64).collect();
        let documents_sent: Vec<i64> = snapshots.iter().map(|j| j.documents_sent as i64).collect();
        let errors: Vec<i64> = snapshots.iter().map(|j| j.errors as i64).collect();
        let bytes_downloaded: Vec<i64> = snapshots
            .iter()
            .map(|j| j.bytes_downloaded as i64)
            .collect();
        let crawl_rates: Vec<f64> = snapshots.iter().map(|j| j.crawl_rate).collect();
        let eta_secs: Vec<Option<i64>> = snapshots
            .iter()
            .map(|j| j.eta_seconds.map(|v| v as i64))
            .collect();

        let result = sqlx::query(
            "UPDATE jobs AS j SET
            status = d.status,
            pages_crawled = d.pages_crawled,
            pages_indexed = d.pages_indexed,
            documents_sent = d.documents_sent,
            errors = d.errors,
            bytes_downloaded = d.bytes_downloaded,
            crawl_rate = d.crawl_rate,
            eta_seconds = d.eta_seconds
        FROM (
            SELECT * FROM unnest(
                $1::text[], $2::text[],
                $3::bigint[], $4::bigint[], $5::bigint[],
                $6::bigint[], $7::bigint[],
                $8::double precision[], $9::bigint[]
            ) AS t(
                job_id, status,
                pages_crawled, pages_indexed, documents_sent,
                errors, bytes_downloaded,
                crawl_rate, eta_seconds
            )
        ) AS d
        WHERE j.job_id = d.job_id AND j.status NOT IN ('completed', 'failed', 'cancelled')
          AND d.status NOT IN ('completed', 'failed', 'cancelled')",
        )
        .bind(&ids)
        .bind(&statuses)
        .bind(&pages_crawled)
        .bind(&pages_indexed)
        .bind(&documents_sent)
        .bind(&errors)
        .bind(&bytes_downloaded)
        .bind(&crawl_rates)
        .bind(&eta_secs)
        .execute(&self.pool)
        .await;

        match result {
            Ok(r) => {
                debug!(rows = r.rows_affected(), "Flushed job counters to Postgres");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "Failed to flush job counters to Postgres");
                Err(store_err(e))
            }
        }
    }

    /// Persist per-job work-accounting snapshots (`jobs.accounting` jsonb, R5)
    /// in a single round-trip.
    ///
    /// Deliberately a separate statement from
    /// [`flush_job_counters`](JobStore::flush_job_counters): if the engine
    /// runs against a database where the migration adding the column
    /// is missing, only this statement fails (logged) and the
    /// counter flush keeps working.
    ///
    /// Returns `Err` (after logging) when the statement fails, so the caller can
    /// keep the covered events un-acked.
    async fn flush_job_accounting(
        &self,
        entries: &[(String, serde_json::Value)],
    ) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = entries.iter().map(|(id, _)| id.as_str()).collect();
        let values: Vec<serde_json::Value> = entries.iter().map(|(_, v)| v.clone()).collect();

        let result = sqlx::query(
            "UPDATE jobs AS j SET accounting = d.accounting
        FROM (
            SELECT * FROM unnest($1::text[], $2::jsonb[]) AS t(job_id, accounting)
        ) AS d
        WHERE j.job_id = d.job_id",
        )
        .bind(&ids)
        .bind(&values)
        .execute(&self.pool)
        .await;

        match result {
            Ok(r) => {
                debug!(
                    rows = r.rows_affected(),
                    "Flushed job accounting to Postgres"
                );
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "Failed to flush job accounting to Postgres");
                Err(store_err(e))
            }
        }
    }

    // ========================================================================
    // Reads
    // ========================================================================

    /// Load active (pending/running/paused) jobs for startup recovery.
    async fn load_active_jobs(&self) -> Vec<JobState> {
        let rows = sqlx::query(
            "SELECT * FROM jobs WHERE status IN ('pending', 'running', 'paused') ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await;

        match rows {
            Ok(rows) => rows.iter().map(row_to_job_state).collect(),
            Err(e) => {
                warn!(error = %e, "Failed to load active jobs from Postgres");
                Vec::new()
            }
        }
    }

    /// Load the persisted work-accounting snapshots of running/paused jobs for
    /// startup recovery. Returns an empty list (logged) if the query fails, e.g.
    /// when the `accounting` column does not exist yet.
    async fn load_active_job_accounting(&self) -> Vec<(String, serde_json::Value)> {
        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT job_id, accounting FROM jobs WHERE status IN ('running', 'paused')",
        )
        .fetch_all(&self.pool)
        .await;

        match rows {
            Ok(rows) => rows
                .iter()
                .filter_map(|row| {
                    let id: String = row.try_get("job_id").ok()?;
                    let acc: serde_json::Value = row.try_get("accounting").ok()?;
                    Some((id, acc))
                })
                .collect(),
            Err(e) => {
                warn!(error = %e, "Failed to load job accounting from Postgres");
                Vec::new()
            }
        }
    }

    /// Look up a single job by ID (fallback when not in the in-memory map),
    /// scoped to an account when `account_id` is given.
    async fn get_job(&self, job_id: &str, account_id: Option<&str>) -> Option<JobState> {
        let row = match account_id {
            None => {
                sqlx::query("SELECT * FROM jobs WHERE job_id = $1")
                    .bind(job_id)
                    .fetch_optional(&self.pool)
                    .await
            }
            Some(account_id) => {
                let account_uuid: uuid::Uuid = account_id.parse().ok()?;
                sqlx::query("SELECT * FROM jobs WHERE job_id = $1 AND account_id = $2")
                    .bind(job_id)
                    .bind(account_uuid)
                    .fetch_optional(&self.pool)
                    .await
            }
        };
        row.ok().flatten().map(|row| row_to_job_state(&row))
    }

    /// Paginated list of jobs, newest first: all jobs, or those of one
    /// account.
    async fn list_jobs(&self, account_id: Option<&str>, limit: i64, offset: i64) -> Vec<JobState> {
        match account_id {
            None => {
                let rows =
                    sqlx::query("SELECT * FROM jobs ORDER BY created_at DESC LIMIT $1 OFFSET $2")
                        .bind(limit)
                        .bind(offset)
                        .fetch_all(&self.pool)
                        .await;

                match rows {
                    Ok(rows) => rows.iter().map(row_to_job_state).collect(),
                    Err(e) => {
                        warn!(error = %e, "Failed to list jobs from Postgres");
                        Vec::new()
                    }
                }
            }
            Some(account_id) => {
                let account_uuid: uuid::Uuid = match account_id.parse() {
                    Ok(u) => u,
                    Err(_) => return Vec::new(),
                };
                let rows = sqlx::query(
                    "SELECT * FROM jobs WHERE account_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
                )
                .bind(account_uuid)
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await;

                match rows {
                    Ok(rows) => rows.iter().map(row_to_job_state).collect(),
                    Err(e) => {
                        warn!(error = %e, "Failed to list jobs for account from Postgres");
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Pending/running jobs of an account (per-tier concurrent job limit).
    async fn active_job_ids(&self, account_id: &str) -> Result<Vec<String>, StoreError> {
        sqlx::query_scalar(
            "SELECT job_id FROM jobs WHERE account_id = $1 AND status IN ('pending', 'running')",
        )
        .bind(uuid::Uuid::parse_str(account_id).ok())
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)
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
        payload: &serde_json::Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO job_results (job_id, seq, kind, url, success, payload) \
             VALUES ($1, $2, 'page', $3, $4, $5) ON CONFLICT (job_id, seq) DO NOTHING",
        )
        .bind(job_id)
        .bind(seq as i32)
        .bind(url)
        .bind(success)
        .bind(payload)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(store_err)
    }

    async fn store_result_summary(
        &self,
        job_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO job_results (job_id, seq, kind, url, success, payload) \
             VALUES ($1, 0, 'extract', NULL, true, $2) \
             ON CONFLICT (job_id, seq) DO UPDATE SET payload = EXCLUDED.payload",
        )
        .bind(job_id)
        .bind(payload)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(store_err)
    }

    async fn load_result_summary(
        &self,
        job_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT payload FROM job_results WHERE job_id = $1 AND kind = 'extract' LIMIT 1",
        )
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_err)
    }

    async fn result_pages(
        &self,
        job_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<(Vec<(u64, serde_json::Value)>, u64), StoreError> {
        use sqlx::Row as _;
        let rows = sqlx::query(
            "SELECT seq, payload FROM job_results \
             WHERE job_id = $1 AND kind = 'page' AND seq > $2 ORDER BY seq LIMIT $3",
        )
        .bind(job_id)
        .bind(after.min(i32::MAX as u64) as i32)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM job_results WHERE job_id = $1 AND kind = 'page'",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await
        .map_err(store_err)?;
        let pages = rows
            .into_iter()
            .map(|row| {
                (
                    row.get::<i32, _>("seq") as u64,
                    row.get::<serde_json::Value, _>("payload"),
                )
            })
            .collect();
        Ok((pages, total as u64))
    }
}

/// Throwaway Postgres pool with a fresh schema and the engine migrations
/// applied; each call gets its own schema so tests don't see each other's rows.
#[cfg(test)]
pub(crate) async fn test_pg_pool() -> Option<sqlx::PgPool> {
    let pool = test_empty_pg_pool().await?;
    PgJobStore::new(pool.clone()).migrate().await.ok()?;
    Some(pool)
}

/// Throwaway Postgres pool on a fresh, empty schema (no migrations).
#[cfg(test)]
pub(crate) async fn test_empty_pg_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("JOBSTORE_TEST_DATABASE_URL").ok()?;
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let admin = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect({
            let schema = schema.clone();
            move |conn, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {schema}"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(&url)
        .await
        .ok()?;
    Some(pool)
}

/// Throwaway Postgres for the conformance suite.
#[cfg(test)]
pub(crate) async fn test_pg_store() -> Option<PgJobStore> {
    Some(PgJobStore::new(test_pg_pool().await?))
}

#[cfg(test)]
pub(crate) async fn test_store() -> Option<std::sync::Arc<dyn JobStore>> {
    Some(std::sync::Arc::new(test_pg_store().await?))
}

#[cfg(test)]
mod tests {
    use super::{test_empty_pg_pool, test_pg_store, PgJobStore};

    #[test]
    fn schema_missing_sqlstates() {
        assert!(super::is_schema_missing_sqlstate(Some("42703")));
        assert!(super::is_schema_missing_sqlstate(Some("42P01")));
        assert!(!super::is_schema_missing_sqlstate(Some("08006")));
        assert!(!super::is_schema_missing_sqlstate(None));
    }

    #[tokio::test]
    async fn detects_rails_database() {
        let Some(pg) = test_pg_store().await else {
            eprintln!("skipped: postgres backend unavailable");
            return;
        };
        assert!(!pg.is_rails_database().await.unwrap());
        sqlx::query("CREATE TABLE schema_migrations (version text)")
            .execute(&pg.pool)
            .await
            .unwrap();
        assert!(pg.is_rails_database().await.unwrap());
    }

    #[tokio::test]
    async fn rails_database_is_refused_for_the_engine_store() {
        let Some(pool) = test_empty_pg_pool().await else {
            eprintln!("skipped");
            return;
        };
        let store = PgJobStore::new(pool.clone());
        assert!(!store.is_rails_database().await.unwrap());
        sqlx::query("CREATE TABLE schema_migrations (version text)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(store.is_rails_database().await.unwrap());
    }
}
