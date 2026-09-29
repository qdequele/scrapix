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

fn usage_data(
    operation: &str,
    credits: i64,
    units: Value,
    description: String,
    job_id: Option<&str>,
) -> Value {
    let mut d = json!({
        "operation": operation,
        "credits": credits,
        "units": units,
        "description": description,
    });
    if let Some(j) = job_id {
        d["job_id"] = json!(j);
    }
    d
}

// Constructors are unused until the charge sites / job lifecycle are wired to
// the outbox in later tasks of the engine-lab boundary plan.
#[allow(dead_code)] // used by lab_sink / charge sites (engine-lab boundary plan)
impl LabEvent {
    pub fn usage(
        account_id: &str,
        api_key_id: Option<&str>,
        operation: &str,
        credits: i64,
        units: Value,
        description: String,
        job_id: Option<&str>,
    ) -> Self {
        event(
            Uuid::now_v7(),
            "usage.recorded",
            account_id,
            api_key_id,
            usage_data(operation, credits, units, description, job_id),
        )
    }

    pub fn crawl_final_usage(
        job_id: &str,
        account_id: &str,
        credits: i64,
        units: Value,
        description: String,
    ) -> Self {
        let id = Uuid::new_v5(&LAB_NAMESPACE, format!("job:{job_id}:final").as_bytes());
        event(
            id,
            "usage.recorded",
            account_id,
            None,
            usage_data("crawl", credits, units, description, Some(job_id)),
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
#[allow(dead_code)] // used by lab_sink / charge sites (engine-lab boundary plan)
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

#[allow(dead_code)] // used by lab_sink / charge sites (engine-lab boundary plan)
pub struct PgOutbox {
    pool: sqlx::PgPool,
}

#[allow(dead_code)] // used by lab_sink / charge sites (engine-lab boundary plan)
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

/// Records events; the only entry point handlers use.
#[allow(dead_code)] // used by lab_sink / charge sites (engine-lab boundary plan)
pub struct Lab {
    outbox: Arc<dyn LabOutbox>,
}

#[allow(dead_code)] // used by lab_sink / charge sites (engine-lab boundary plan)
impl Lab {
    pub fn new(outbox: Arc<dyn LabOutbox>) -> Self {
        Self { outbox }
    }

    pub fn outbox(&self) -> &Arc<dyn LabOutbox> {
        &self.outbox
    }

    pub async fn record(&self, events: &[LabEvent]) -> Result<(), StoreError> {
        let r = self.outbox.enqueue(events).await;
        if let Err(ref e) = r {
            tracing::warn!(error = %e, count = events.len(), "Failed to record lab events");
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
#[allow(dead_code)] // used by lab_sink / charge sites (engine-lab boundary plan)
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
        let a = LabEvent::crawl_final_usage("job-1", "acc", 10, json!({}), "d".into());
        let b = LabEvent::crawl_final_usage("job-1", "acc", 99, json!({}), "x".into());
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
            LabEvent::crawl_final_usage("job-2", "acc", 10, json!({}), "d".into()).id
        );
    }

    #[test]
    fn usage_serializes_to_the_contract_shape() {
        let e = LabEvent::usage(
            "acc",
            Some("key"),
            "scrape",
            3,
            json!({"formats":["markdown"]}),
            "https://e.com (3 credits)".into(),
            None,
        );
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "usage.recorded");
        assert_eq!(v["product"], "scrapix");
        assert_eq!(v["account_id"], "acc");
        assert_eq!(v["api_key_id"], "key");
        assert_eq!(v["data"]["operation"], "scrape");
        assert_eq!(v["data"]["credits"], 3);
        assert_eq!(v["data"]["description"], "https://e.com (3 credits)");
        assert!(v["data"].get("job_id").is_none());
        assert_eq!(e.id.get_version_num(), 7);
    }

    #[tokio::test]
    async fn memory_outbox_enqueue_is_idempotent_and_due_is_oldest_first() {
        let o = MemoryOutbox::default();
        let e1 = LabEvent::usage("a", None, "map", 2, json!({}), "m".into(), None);
        let e2 = LabEvent::usage("a", None, "map", 2, json!({}), "m".into(), None);
        o.enqueue(&[e1.clone(), e2.clone()]).await.unwrap();
        o.enqueue(&[e1.clone()]).await.unwrap();
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
            1,
            json!({}),
            "m".into(),
            None,
        );
        o.enqueue(&[e.clone()]).await.unwrap();
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

    #[tokio::test]
    async fn pg_outbox_roundtrip() {
        let Some(pool) = crate::job_store::postgres::test_pg_pool().await else {
            eprintln!("skipped");
            return;
        };
        let o = PgOutbox::new(pool);
        let e = LabEvent::crawl_final_usage(
            "j",
            "7f1c2a8e-0000-4000-8000-000000000001",
            5,
            json!({"pages_http":5}),
            "Job j".into(),
        );
        o.enqueue(&[e.clone()]).await.unwrap();
        o.enqueue(&[e.clone()]).await.unwrap();
        let due = o.due(10).await.unwrap();
        assert_eq!(due, vec![e.clone()]);
        assert_eq!(o.pending_stats().await.unwrap().0, 1);
        o.reschedule(&[e.id]).await.unwrap();
        assert!(o.due(10).await.unwrap().is_empty());
        o.mark_delivered(&[e.id]).await.unwrap();
        assert_eq!(o.pending_stats().await.unwrap().0, 0);
        assert_eq!(o.purge_delivered(-1).await.unwrap(), 1);
    }
}
