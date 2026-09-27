//! A [`FrontierStore`] decorator that bounds every call.
//!
//! A hung Redis must not block the dispatcher, the admit path or shutdown
//! forever: each call gets `timeout`, and an elapsed call returns
//! `ScrapixError::Timeout`, which every caller already handles as a store
//! error (admit retries with backoff and leaves the message un-acked; the
//! dispatcher skips the job this tick; a failed `requeue` stashes the URLs).
//!
//! Caveat: a timed-out call may still have been applied by the server (the
//! reply was just late). For `pop_ready` that means the popped URLs are
//! never seen by this process — the same outcome as a connection error
//! after the command ran, which the store could already produce.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use scrapix_core::{CrawlUrl, Result, ScrapixError};
use scrapix_frontier::{Admission, FrontierStore, JobCounters, JobRunState};

/// Default per-call bound (well under the shutdown grace).
pub const STORE_CALL_TIMEOUT: Duration = Duration::from_secs(5);

pub struct TimeoutStore {
    inner: Arc<dyn FrontierStore>,
    timeout: Duration,
}

impl TimeoutStore {
    pub fn new(inner: Arc<dyn FrontierStore>, timeout: Duration) -> Self {
        Self { inner, timeout }
    }

    async fn bounded<T>(&self, op: &str, fut: impl Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(ScrapixError::Timeout(format!(
                "frontier store `{op}` did not answer within {}ms",
                self.timeout.as_millis()
            ))),
        }
    }
}

#[async_trait]
impl FrontierStore for TimeoutStore {
    async fn ensure_job(
        &self,
        job_id: &str,
        template_json: &str,
        max_pages: Option<u64>,
        max_depth: Option<u32>,
    ) -> Result<()> {
        self.bounded(
            "ensure_job",
            self.inner
                .ensure_job(job_id, template_json, max_pages, max_depth),
        )
        .await
    }

    async fn job_template(&self, job_id: &str) -> Result<Option<String>> {
        self.bounded("job_template", self.inner.job_template(job_id))
            .await
    }

    async fn admit(&self, job_id: &str, url: &CrawlUrl, queue_cap: usize) -> Result<Admission> {
        self.bounded("admit", self.inner.admit(job_id, url, queue_cap))
            .await
    }

    async fn pop_ready(&self, job_id: &str, n: usize, now_ms: i64) -> Result<Vec<CrawlUrl>> {
        self.bounded("pop_ready", self.inner.pop_ready(job_id, n, now_ms))
            .await
    }

    async fn requeue(&self, job_id: &str, urls: Vec<CrawlUrl>) -> Result<()> {
        self.bounded("requeue", self.inner.requeue(job_id, urls))
            .await
    }

    async fn queued(&self, job_id: &str) -> Result<u64> {
        self.bounded("queued", self.inner.queued(job_id)).await
    }

    async fn counters(&self, job_id: &str) -> Result<JobCounters> {
        self.bounded("counters", self.inner.counters(job_id)).await
    }

    async fn set_state(&self, job_id: &str, state: JobRunState) -> Result<()> {
        self.bounded("set_state", self.inner.set_state(job_id, state))
            .await
    }

    async fn state(&self, job_id: &str) -> Result<Option<JobRunState>> {
        self.bounded("state", self.inner.state(job_id)).await
    }

    async fn active_jobs(&self) -> Result<Vec<String>> {
        self.bounded("active_jobs", self.inner.active_jobs()).await
    }

    async fn release(&self, job_id: &str, retention: Duration) -> Result<()> {
        self.bounded("release", self.inner.release(job_id, retention))
            .await
    }

    async fn try_lease(&self, job_id: &str, owner: &str, ttl: Duration) -> Result<bool> {
        self.bounded("try_lease", self.inner.try_lease(job_id, owner, ttl))
            .await
    }
}
