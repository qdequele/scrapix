//! Events the engine reports to the Lab (Rails): usage for billing and job
//! lifecycle for emails. Written to an engine-owned outbox (`lab_events`),
//! delivered by `lab_sink` over a signed webhook. Never used in standalone.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::job_store::StoreError;

pub const LAB_NAMESPACE: Uuid = uuid::uuid!("6f1c3a2e-6b1d-4f6e-9a57-0c3b2a9e5d41");
pub const PRODUCT: &str = "scrapix";

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LabEvent {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub kind: String,
    pub occurred_at: DateTime<Utc>,
    pub account_id: String,
    pub api_key_id: Option<String>,
    pub product: String,
    pub data: Value,
}

fn event(
    id: Uuid,
    kind: &str,
    account_id: &str,
    api_key_id: Option<&str>,
    data: Value,
) -> LabEvent {
    LabEvent {
        id,
        kind: kind.to_string(),
        occurred_at: Utc::now(),
        account_id: account_id.to_string(),
        api_key_id: api_key_id.map(str::to_string),
        product: PRODUCT.to_string(),
        data,
    }
}

/// Descriptions embed caller input (a search `q`, a map URL): drop control
/// characters (NUL included, which Postgres `jsonb` rejects) so the ledger
/// line and the outbox row stay clean.
fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Provider pass-through cost the engine paid for this usage, in micro-USD
/// (spec §4.2). Scrapix does not price its AI/OCR providers yet, so it
/// reports 0; the Lab prices units from its own table.
const PROVIDER_COST_MICRO_USD: i64 = 0;

fn usage_data(operation: &str, units: Value, description: String, job_id: Option<&str>) -> Value {
    let mut d = json!({
        "operation": operation,
        "units": units,
        "provider_cost_micro_usd": PROVIDER_COST_MICRO_USD,
        "description": strip_control_chars(&description),
    });
    if let Some(j) = job_id {
        d["job_id"] = json!(j);
    }
    d
}

impl LabEvent {
    pub fn usage(
        account_id: &str,
        api_key_id: Option<&str>,
        operation: &str,
        units: Value,
        description: String,
        job_id: Option<&str>,
    ) -> Self {
        event(
            Uuid::now_v7(),
            "usage.recorded",
            account_id,
            api_key_id,
            usage_data(operation, units, description, job_id),
        )
    }

    pub fn crawl_final_usage(
        job_id: &str,
        account_id: &str,
        units: Value,
        description: String,
    ) -> Self {
        let id = Uuid::new_v5(&LAB_NAMESPACE, format!("job:{job_id}:final").as_bytes());
        event(
            id,
            "usage.recorded",
            account_id,
            None,
            usage_data("crawl", units, description, Some(job_id)),
        )
    }

    fn lifecycle(kind: &str, job_id: &str, account_id: &str, data: Value) -> Self {
        let id = Uuid::new_v5(&LAB_NAMESPACE, format!("job:{job_id}:lifecycle").as_bytes());
        event(id, kind, account_id, None, data)
    }

    pub fn job_completed(job_id: &str, account_id: &str, data: Value) -> Self {
        Self::lifecycle("job.completed", job_id, account_id, data)
    }

    pub fn job_failed(job_id: &str, account_id: &str, data: Value) -> Self {
        Self::lifecycle("job.failed", job_id, account_id, data)
    }
}

#[async_trait::async_trait]
pub trait LabOutbox: Send + Sync {
    /// Insert events; an already-present id is skipped (idempotent).
    async fn enqueue(&self, events: &[LabEvent]) -> Result<(), StoreError>;
    /// Undelivered events whose retry time has passed, oldest first.
    async fn due(&self, limit: i64) -> Result<Vec<LabEvent>, StoreError>;
    async fn mark_delivered(&self, ids: &[Uuid]) -> Result<(), StoreError>;
    /// Record a failed delivery: attempts + 1 and exponential backoff.
    async fn reschedule(&self, ids: &[Uuid]) -> Result<(), StoreError>;
    /// Undelivered count and the creation time of the oldest one.
    async fn pending_stats(&self) -> Result<(i64, Option<DateTime<Utc>>), StoreError>;
    async fn purge_delivered(&self, older_than_secs: i64) -> Result<u64, StoreError>;
}

pub struct PgOutbox {
    pool: sqlx::PgPool,
}

impl PgOutbox {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

fn db(e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(e.to_string())
}

#[async_trait::async_trait]
impl LabOutbox for PgOutbox {
    async fn enqueue(&self, events: &[LabEvent]) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        for e in events {
            let account: Uuid = e
                .account_id
                .parse()
                .map_err(|_| db(format!("invalid account_id {}", e.account_id)))?;
            sqlx::query(
                "INSERT INTO lab_events (id, type, account_id, payload) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING",
            )
            .bind(e.id)
            .bind(&e.kind)
            .bind(account)
            .bind(serde_json::to_value(e).map_err(db)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)
    }

    async fn due(&self, limit: i64) -> Result<Vec<LabEvent>, StoreError> {
        let rows: Vec<Value> = sqlx::query_scalar(
            "SELECT payload FROM lab_events \
             WHERE delivered_at IS NULL AND next_attempt_at <= now() \
             ORDER BY created_at, id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.into_iter()
            .map(|v| serde_json::from_value(v).map_err(db))
            .collect()
    }

    async fn mark_delivered(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        sqlx::query("UPDATE lab_events SET delivered_at = now() WHERE id = ANY($1)")
            .bind(ids)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(db)
    }

    async fn reschedule(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "UPDATE lab_events SET attempts = attempts + 1, \
             next_attempt_at = now() + make_interval(secs => LEAST(power(2, LEAST(attempts + 1, 9)), 300)) \
             WHERE id = ANY($1) AND delivered_at IS NULL",
        )
        .bind(ids)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(db)
    }

    async fn pending_stats(&self) -> Result<(i64, Option<DateTime<Utc>>), StoreError> {
        sqlx::query_as(
            "SELECT count(*), min(created_at) FROM lab_events WHERE delivered_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)
    }

    async fn purge_delivered(&self, older_than_secs: i64) -> Result<u64, StoreError> {
        sqlx::query(
            "DELETE FROM lab_events WHERE delivered_at IS NOT NULL \
             AND delivered_at < now() - make_interval(secs => $1)",
        )
        .bind(older_than_secs as f64)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected())
        .map_err(db)
    }
}

/// Retry delay after a failed delivery, given the attempts made so far:
/// `min(2^min(attempts + 1, 9), 300)` seconds, the same rule as
/// [`PgOutbox::reschedule`].
fn backoff_secs(attempts: i64) -> i64 {
    2i64.pow(attempts.saturating_add(1).clamp(0, 9) as u32)
        .min(300)
}

/// [`LabOutbox`] over the engine's SQLite database (same semantics as
/// [`PgOutbox`]; timestamps are RFC 3339 text, compared as strings).
pub struct SqliteOutbox {
    pool: sqlx::SqlitePool,
}

impl SqliteOutbox {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }
}

fn sqlite_ts(t: DateTime<Utc>) -> String {
    crate::job_store::sqlite::ts(Some(t)).unwrap_or_default()
}

#[async_trait::async_trait]
impl LabOutbox for SqliteOutbox {
    async fn enqueue(&self, events: &[LabEvent]) -> Result<(), StoreError> {
        let now = sqlite_ts(Utc::now());
        let mut tx = self.pool.begin().await.map_err(db)?;
        for e in events {
            let account: Uuid = e
                .account_id
                .parse()
                .map_err(|_| db(format!("invalid account_id {}", e.account_id)))?;
            sqlx::query(
                "INSERT INTO lab_events (id, type, account_id, payload, created_at, next_attempt_at) \
                 VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
            )
            .bind(e.id.to_string())
            .bind(&e.kind)
            .bind(account.to_string())
            .bind(serde_json::to_string(e).map_err(db)?)
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)
    }

    async fn due(&self, limit: i64) -> Result<Vec<LabEvent>, StoreError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT payload FROM lab_events \
             WHERE delivered_at IS NULL AND next_attempt_at <= ? \
             ORDER BY created_at, rowid LIMIT ?",
        )
        .bind(sqlite_ts(Utc::now()))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter()
            .map(|v| serde_json::from_str(v).map_err(db))
            .collect()
    }

    async fn mark_delivered(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        let now = sqlite_ts(Utc::now());
        let mut tx = self.pool.begin().await.map_err(db)?;
        for id in ids {
            sqlx::query("UPDATE lab_events SET delivered_at = ? WHERE id = ?")
                .bind(&now)
                .bind(id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        tx.commit().await.map_err(db)
    }

    async fn reschedule(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(db)?;
        for id in ids {
            let attempts: Option<i64> = sqlx::query_scalar(
                "SELECT attempts FROM lab_events WHERE id = ? AND delivered_at IS NULL",
            )
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
            let Some(attempts) = attempts else { continue };
            let next = now + chrono::Duration::seconds(backoff_secs(attempts));
            sqlx::query(
                "UPDATE lab_events SET attempts = attempts + 1, next_attempt_at = ? WHERE id = ?",
            )
            .bind(sqlite_ts(next))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)
    }

    async fn pending_stats(&self) -> Result<(i64, Option<DateTime<Utc>>), StoreError> {
        let (count, oldest): (i64, Option<String>) = sqlx::query_as(
            "SELECT count(*), min(created_at) FROM lab_events WHERE delivered_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok((count, crate::job_store::sqlite::parse_ts(oldest)))
    }

    async fn purge_delivered(&self, older_than_secs: i64) -> Result<u64, StoreError> {
        let cutoff = Utc::now() - chrono::Duration::seconds(older_than_secs);
        sqlx::query("DELETE FROM lab_events WHERE delivered_at IS NOT NULL AND delivered_at < ?")
            .bind(sqlite_ts(cutoff))
            .execute(&self.pool)
            .await
            .map(|r| r.rows_affected())
            .map_err(db)
    }
}

/// Records events; the only entry point handlers use.
pub struct Lab {
    outbox: Arc<dyn LabOutbox>,
}

impl Lab {
    pub fn new(outbox: Arc<dyn LabOutbox>) -> Self {
        Self { outbox }
    }

    pub async fn record(&self, events: &[LabEvent]) -> Result<(), StoreError> {
        let r = self.outbox.enqueue(events).await;
        if let Err(ref e) = r {
            tracing::warn!(error = %e, count = events.len(), "Failed to record lab events");
            // One line per event so a lost charge can be reconstructed from logs.
            for ev in events {
                let operation = ev.data.get("operation").and_then(|v| v.as_str());
                let units = ev.data.get("units").map(|u| u.to_string());
                tracing::warn!(
                    event_id = %ev.id,
                    account_id = %ev.account_id,
                    event_type = %ev.kind,
                    operation,
                    units,
                    "Lab event not recorded"
                );
            }
        }
        r
    }
}

/// In-memory outbox for tests (mirrors `PgOutbox` semantics).
#[cfg(test)]
#[derive(Default)]
pub struct MemoryOutbox {
    rows: parking_lot::Mutex<Vec<Row>>,
}

#[cfg(test)]
struct Row {
    event: LabEvent,
    next_attempt: std::time::Instant,
    attempts: u32,
    delivered: bool,
}

#[cfg(test)]
impl MemoryOutbox {
    /// Every recorded event (delivered or not), in insertion order.
    pub fn events(&self) -> Vec<LabEvent> {
        self.rows.lock().iter().map(|r| r.event.clone()).collect()
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl LabOutbox for MemoryOutbox {
    async fn enqueue(&self, events: &[LabEvent]) -> Result<(), StoreError> {
        let mut rows = self.rows.lock();
        for e in events {
            if rows.iter().any(|r| r.event.id == e.id) {
                continue;
            }
            rows.push(Row {
                event: e.clone(),
                next_attempt: std::time::Instant::now(),
                attempts: 0,
                delivered: false,
            });
        }
        Ok(())
    }

    async fn due(&self, limit: i64) -> Result<Vec<LabEvent>, StoreError> {
        let now = std::time::Instant::now();
        Ok(self
            .rows
            .lock()
            .iter()
            .filter(|r| !r.delivered && r.next_attempt <= now)
            .take(limit.max(0) as usize)
            .map(|r| r.event.clone())
            .collect())
    }

    async fn mark_delivered(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        for r in self.rows.lock().iter_mut() {
            if ids.contains(&r.event.id) {
                r.delivered = true;
            }
        }
        Ok(())
    }

    async fn reschedule(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        for r in self.rows.lock().iter_mut() {
            if ids.contains(&r.event.id) && !r.delivered {
                let backoff = 2u64.pow(r.attempts.saturating_add(1).min(9)).min(300);
                r.next_attempt =
                    std::time::Instant::now() + std::time::Duration::from_secs(backoff);
                r.attempts = r.attempts.saturating_add(1);
            }
        }
        Ok(())
    }

    async fn pending_stats(&self) -> Result<(i64, Option<DateTime<Utc>>), StoreError> {
        let rows = self.rows.lock();
        let mut pending = rows.iter().filter(|r| !r.delivered);
        let oldest = pending.clone().next().map(|r| r.event.occurred_at);
        Ok((pending.by_ref().count() as i64, oldest))
    }

    async fn purge_delivered(&self, _older_than_secs: i64) -> Result<u64, StoreError> {
        let mut rows = self.rows.lock();
        let before = rows.len();
        rows.retain(|r| !r.delivered);
        Ok((before - rows.len()) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn crawl_final_and_lifecycle_ids_are_deterministic_and_distinct() {
        let a = LabEvent::crawl_final_usage("job-1", "acc", json!({}), "d".into());
        let b = LabEvent::crawl_final_usage("job-1", "acc", json!({}), "x".into());
        assert_eq!(a.id, b.id);
        let c = LabEvent::job_completed("job-1", "acc", json!({}));
        let d = LabEvent::job_failed("job-1", "acc", json!({}));
        assert_eq!(
            c.id, d.id,
            "a job has one lifecycle email, completed or failed"
        );
        assert_ne!(a.id, c.id);
        assert_ne!(
            a.id,
            LabEvent::crawl_final_usage("job-2", "acc", json!({}), "d".into()).id
        );
    }

    #[test]
    fn usage_serializes_to_the_contract_shape() {
        let e = LabEvent::usage(
            "acc",
            Some("key"),
            "scrape",
            json!({"pages_http": 1}),
            "https://e.com".into(),
            None,
        );
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "usage.recorded");
        assert_eq!(v["product"], "scrapix");
        assert_eq!(v["account_id"], "acc");
        assert_eq!(v["api_key_id"], "key");
        assert_eq!(v["data"]["operation"], "scrape");
        assert_eq!(v["data"]["provider_cost_micro_usd"], 0);
        assert_eq!(v["data"]["description"], "https://e.com");
        assert!(v["data"].get("job_id").is_none());
        assert_eq!(e.id.get_version_num(), 7);
    }

    #[test]
    fn usage_description_is_stripped_of_control_characters() {
        let e = LabEvent::usage(
            "acc",
            None,
            "search",
            json!({}),
            "Search 'a\0b\nc\u{7}d\u{9b}e' (1 credit)".into(),
            None,
        );
        assert_eq!(e.data["description"], "Search 'abcde' (1 credit)");
        let f = LabEvent::crawl_final_usage("j", "acc", json!({}), "Job\r\n j\0".into());
        assert_eq!(f.data["description"], "Job j");
    }

    /// The engine's serialized events must satisfy the Lab-owned contract
    /// (`contracts/vendor/lab/lab-events.schema.json`), which the Lab's
    /// receiver is also tested against.
    fn contract_validator() -> jsonschema::Validator {
        let raw = include_str!("../../../contracts/vendor/lab/lab-events.schema.json");
        let schema: Value = serde_json::from_str(raw).unwrap();
        jsonschema::validator_for(&schema).unwrap()
    }

    fn assert_valid(v: &jsonschema::Validator, e: &LabEvent) {
        let value = serde_json::to_value(e).unwrap();
        let errors: Vec<String> = v.iter_errors(&value).map(|e| e.to_string()).collect();
        assert!(
            errors.is_empty(),
            "{value} violates the contract: {errors:?}"
        );
    }

    #[test]
    fn events_satisfy_the_contract_schema() {
        let v = contract_validator();
        let acct = "7f1c2a8e-0000-4000-8000-000000000001";
        assert_valid(
            &v,
            &LabEvent::usage(
                acct,
                Some("key_1"),
                "scrape",
                json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 0, "ai_extraction": 0}),
                "https://e.com".into(),
                None,
            ),
        );
        assert_valid(
            &v,
            &LabEvent::usage(
                acct,
                None,
                "extract",
                json!({"documents": 1}),
                "extract".into(),
                Some("job-1"),
            ),
        );
        assert_valid(
            &v,
            &LabEvent::crawl_final_usage(
                "job-1",
                acct,
                json!({"pages_http":10,"pages_browser":2,"pages_ai":0,"pages_ocr":0}),
                "Job job-1 (10 http + 2 browser pages, 0 AI-enriched)".into(),
            ),
        );
        assert_valid(
            &v,
            &LabEvent::job_completed(
                "job-1",
                acct,
                json!({"job_id":"job-1","index_uid":"docs","pages_crawled":12,
                       "documents_indexed":12,"duration_secs":30}),
            ),
        );
        assert_valid(
            &v,
            &LabEvent::job_failed(
                "job-1",
                acct,
                json!({"job_id":"job-1","error_message":"boom","pages_crawled":0}),
            ),
        );
    }

    #[test]
    fn usage_events_carry_units_and_provider_cost_and_never_credits() {
        let e = LabEvent::usage(
            "7f1c2a8e-0000-4000-8000-000000000001",
            None,
            "map",
            json!({"requests": 1, "urls_found": 17}),
            "https://e.com".into(),
            None,
        );
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["data"]["units"], json!({"requests": 1, "urls_found": 17}));
        assert_eq!(v["data"]["provider_cost_micro_usd"], 0);
        assert!(
            v["data"].get("credits").is_none(),
            "credits are priced by the Lab"
        );
    }

    #[test]
    fn the_contract_schema_rejects_malformed_events() {
        let v = contract_validator();
        let acct = "7f1c2a8e-0000-4000-8000-000000000001";
        let good = serde_json::to_value(LabEvent::usage(
            acct,
            None,
            "map",
            json!({"requests": 1}),
            "m".into(),
            None,
        ))
        .unwrap();
        assert!(v.is_valid(&good));
        let mut bad = good.clone();
        bad["data"]["units"]["requests"] = json!(-1);
        assert!(!v.is_valid(&bad), "negative unit");
        let mut bad = good.clone();
        bad["data"]["units"]["formats"] = json!(["markdown"]);
        assert!(!v.is_valid(&bad), "units are integers only");
        let mut bad = good.clone();
        bad["data"]["provider_cost_micro_usd"] = json!(-5);
        assert!(!v.is_valid(&bad), "negative provider cost");
        let mut bad = good.clone();
        bad["data"]["operation"] = json!("teleport");
        assert!(!v.is_valid(&bad), "unknown scrapix operation");
        let mut bad = good.clone();
        bad["account_id"] = json!("acc");
        assert!(!v.is_valid(&bad), "non-uuid account");
        let mut bad = good;
        bad["type"] = json!("job.completed");
        assert!(!v.is_valid(&bad), "usage data under a job type");
    }

    #[tokio::test]
    async fn memory_outbox_enqueue_is_idempotent_and_due_is_oldest_first() {
        let o = MemoryOutbox::default();
        let e1 = LabEvent::usage("a", None, "map", json!({}), "m".into(), None);
        let e2 = LabEvent::usage("a", None, "map", json!({}), "m".into(), None);
        o.enqueue(&[e1.clone(), e2.clone()]).await.unwrap();
        o.enqueue(std::slice::from_ref(&e1)).await.unwrap();
        let due = o.due(10).await.unwrap();
        assert_eq!(
            due.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![e1.id, e2.id]
        );
        o.mark_delivered(&[e1.id]).await.unwrap();
        assert_eq!(o.due(10).await.unwrap().len(), 1);
        o.reschedule(&[e2.id]).await.unwrap();
        assert!(
            o.due(10).await.unwrap().is_empty(),
            "rescheduled event is not due yet"
        );
    }

    #[tokio::test]
    async fn pg_reschedule_caps_backoff_and_survives_huge_attempt_counts() {
        let Some(pool) = crate::job_store::postgres::test_pg_pool().await else {
            eprintln!("skipped");
            return;
        };
        let o = PgOutbox::new(pool.clone());
        let e = LabEvent::usage(
            "7f1c2a8e-0000-4000-8000-000000000001",
            None,
            "map",
            json!({}),
            "m".into(),
            None,
        );
        o.enqueue(std::slice::from_ref(&e)).await.unwrap();
        sqlx::query("UPDATE lab_events SET attempts = 2000")
            .execute(&pool)
            .await
            .unwrap();
        o.reschedule(&[e.id]).await.unwrap();
        let (attempts, secs): (i32, f64) = sqlx::query_as(
            "SELECT attempts, EXTRACT(EPOCH FROM (next_attempt_at - now()))::float8 \
             FROM lab_events WHERE id = $1",
        )
        .bind(e.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(attempts, 2001);
        assert!((290.0..=310.0).contains(&secs), "backoff was {secs}s");
    }

    /// Shared outbox semantics: idempotent enqueue, oldest-first `due`,
    /// reschedule hides an event, delivery clears it from the pending stats,
    /// purge removes it.
    async fn outbox_roundtrip(o: &dyn LabOutbox) {
        let acct = "7f1c2a8e-0000-4000-8000-000000000001";
        let e = LabEvent::crawl_final_usage("j", acct, json!({"pages_http":5}), "Job j".into());
        let u = LabEvent::usage(acct, Some("k"), "map", json!({}), "m".into(), None);
        o.enqueue(std::slice::from_ref(&e)).await.unwrap();
        o.enqueue(&[e.clone(), u.clone()]).await.unwrap();
        assert_eq!(o.due(10).await.unwrap(), vec![e.clone(), u.clone()]);
        assert_eq!(o.due(1).await.unwrap(), vec![e.clone()]);
        let (pending, oldest) = o.pending_stats().await.unwrap();
        assert_eq!(pending, 2);
        assert!(oldest.is_some());
        o.reschedule(&[e.id]).await.unwrap();
        assert_eq!(o.due(10).await.unwrap(), vec![u.clone()]);
        o.mark_delivered(&[e.id, u.id]).await.unwrap();
        assert!(o.due(10).await.unwrap().is_empty());
        assert_eq!(o.pending_stats().await.unwrap(), (0, None));
        assert_eq!(o.purge_delivered(-1).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn memory_outbox_roundtrip() {
        outbox_roundtrip(&MemoryOutbox::default()).await;
    }

    #[tokio::test]
    async fn pg_outbox_roundtrip() {
        let Some(pool) = crate::job_store::postgres::test_pg_pool().await else {
            eprintln!("skipped");
            return;
        };
        outbox_roundtrip(&PgOutbox::new(pool)).await;
    }

    #[tokio::test]
    async fn sqlite_outbox_roundtrip() {
        let (_dir, pool) = crate::job_store::sqlite::test_sqlite_pool().await;
        outbox_roundtrip(&SqliteOutbox::new(pool)).await;
    }

    #[tokio::test]
    async fn sqlite_outbox_rejects_a_non_uuid_account_like_postgres() {
        let (_dir, pool) = crate::job_store::sqlite::test_sqlite_pool().await;
        let o = SqliteOutbox::new(pool);
        let e = LabEvent::usage("acc", None, "map", json!({}), "m".into(), None);
        assert!(o.enqueue(&[e]).await.is_err());
        assert_eq!(o.pending_stats().await.unwrap().0, 0);
    }

    #[tokio::test]
    async fn sqlite_reschedule_caps_backoff_and_survives_huge_attempt_counts() {
        let (_dir, pool) = crate::job_store::sqlite::test_sqlite_pool().await;
        let o = SqliteOutbox::new(pool.clone());
        let acct = "7f1c2a8e-0000-4000-8000-000000000001";
        let e = LabEvent::usage(acct, None, "map", json!({}), "m".into(), None);
        let next = |id: Uuid| {
            let pool = pool.clone();
            async move {
                let (attempts, at): (i64, String) =
                    sqlx::query_as("SELECT attempts, next_attempt_at FROM lab_events WHERE id = ?")
                        .bind(id.to_string())
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                let at = DateTime::parse_from_rfc3339(&at)
                    .unwrap()
                    .with_timezone(&Utc);
                (
                    attempts,
                    (at - Utc::now()).num_milliseconds() as f64 / 1000.0,
                )
            }
        };
        o.enqueue(std::slice::from_ref(&e)).await.unwrap();
        o.reschedule(&[e.id]).await.unwrap();
        let (attempts, secs) = next(e.id).await;
        assert_eq!(attempts, 1);
        assert!((1.0..=2.5).contains(&secs), "first backoff was {secs}s");
        sqlx::query("UPDATE lab_events SET attempts = 2000")
            .execute(&pool)
            .await
            .unwrap();
        o.reschedule(&[e.id]).await.unwrap();
        let (attempts, secs) = next(e.id).await;
        assert_eq!(attempts, 2001);
        assert!((290.0..=310.0).contains(&secs), "backoff was {secs}s");
        o.mark_delivered(&[e.id]).await.unwrap();
        o.reschedule(&[e.id]).await.unwrap();
        assert_eq!(
            next(e.id).await.0,
            2001,
            "a delivered event is never rescheduled"
        );
    }

    #[tokio::test]
    async fn sqlite_purge_keeps_recently_delivered_events() {
        let (_dir, pool) = crate::job_store::sqlite::test_sqlite_pool().await;
        let o = SqliteOutbox::new(pool);
        let acct = "7f1c2a8e-0000-4000-8000-000000000001";
        let e = LabEvent::usage(acct, None, "map", json!({}), "m".into(), None);
        o.enqueue(std::slice::from_ref(&e)).await.unwrap();
        assert_eq!(
            o.purge_delivered(-1).await.unwrap(),
            0,
            "undelivered is never purged"
        );
        o.mark_delivered(&[e.id]).await.unwrap();
        assert_eq!(o.purge_delivered(3600).await.unwrap(), 0);
        assert_eq!(o.purge_delivered(-1).await.unwrap(), 1);
    }

    #[test]
    fn backoff_matches_the_postgres_rule() {
        assert_eq!(backoff_secs(0), 2);
        assert_eq!(backoff_secs(1), 4);
        assert_eq!(backoff_secs(7), 256);
        assert_eq!(backoff_secs(8), 300);
        assert_eq!(backoff_secs(2000), 300);
    }
}
