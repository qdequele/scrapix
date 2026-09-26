//! Frontier storage abstraction.
//!
//! `FrontierStore` defines the state a distributed frontier needs: per-job
//! run state, counters, a priority queue of ready-to-dispatch URLs, and a
//! dedup set. This module provides [`MemoryFrontierStore`], an in-process
//! reference implementation, validated by the conformance suite in
//! [`conformance`]. A future Redis-backed implementation must behave
//! identically to it — that suite is written generically over
//! `Arc<dyn FrontierStore>` so it can be reused unchanged.
//!
//! ## Admission order
//!
//! `admit` evaluates, for a single [`CrawlUrl`], in this exact order:
//!
//! 1. **state** — the job must be [`JobRunState::Running`]; otherwise
//!    [`Admission::JobNotRunning`].
//! 2. **depth** — if the job has a `max_depth` and the URL exceeds it,
//!    [`Admission::OverDepth`].
//! 3. **retry bypass** — a URL with `retry_count > 0` is a redispatch of a
//!    page that was already counted against the budget and already marked
//!    seen; it skips the budget and dedup checks below (but not capacity).
//! 4. **budget** — unless bypassed, if `admitted >= max_pages`,
//!    [`Admission::OverMaxPages`]. This is the only place `admitted` is
//!    incremented, so a URL is counted at most once no matter how many
//!    times it is later requeued.
//! 5. **capacity** — if the queue already holds `queue_cap` URLs,
//!    [`Admission::QueueFull`]. Checked before dedup, so a rejected URL is
//!    never marked seen — a later `admit` of the same URL can still
//!    succeed.
//! 6. **dedup** — unless bypassed, if the URL's hash is already in the seen
//!    set, [`Admission::Duplicate`].
//! 7. **enqueue** — otherwise [`Admission::Admitted`]: mark seen (unless
//!    bypassed), enqueue with a fresh sequence number, and (unless
//!    bypassed) increment `admitted`.
//!
//! `received` is incremented on every call regardless of outcome.
//!
//! A Redis-backed implementation's Lua script must implement exactly this
//! order to behave identically to [`MemoryFrontierStore`].
//!
//! ## Ordering within the queue
//!
//! URLs pop in descending `priority`, then FIFO within the same priority via
//! a monotonically increasing sequence number. `requeue` assigns each URL a
//! *fresh* sequence number, so requeued URLs sort behind existing URLs of
//! the same priority rather than jumping the line.
//!
//! `pop_ready` skips (without removing) any URL whose `not_before_ms` is in
//! the future, so a delayed high-priority URL never blocks a due
//! lower-priority one behind it in the queue.

mod memory;

#[cfg(any(test, feature = "conformance"))]
pub mod conformance;

pub use memory::MemoryFrontierStore;

use async_trait::async_trait;
use scrapix_core::{CrawlUrl, Result};
use serde::{Deserialize, Serialize};

/// Run state of a crawl job as tracked by the frontier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobRunState {
    Running,
    Paused,
    Cancelled,
    Finished,
}

/// Per-job bookkeeping counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCounters {
    /// Every `admit` call, regardless of outcome.
    pub received: u64,
    /// URLs that passed all admission checks and were enqueued. Incremented
    /// at most once per URL (retries do not increment it again).
    pub admitted: u64,
    /// URLs popped via `pop_ready`.
    pub dispatched: u64,
    /// `admit` calls not admitted for depth, budget, capacity, or run
    /// state (i.e. any `Admission` other than `Admitted`; this includes
    /// `Duplicate`, `OverDepth`, `OverMaxPages`, `QueueFull`, and
    /// `JobNotRunning`).
    pub rejected: u64,
    /// URLs removed from the queue by `release` (a finished/cancelled job
    /// discarding whatever was still pending) rather than by `pop_ready`.
    pub dropped: u64,
}

/// Outcome of a single `admit` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Admitted,
    Duplicate,
    OverDepth,
    OverMaxPages,
    QueueFull,
    JobNotRunning,
}

/// Storage abstraction for the URL frontier.
///
/// See the module docs for the exact `admit` decision order and queue
/// ordering guarantees that every implementation must uphold.
#[async_trait]
pub trait FrontierStore: Send + Sync {
    /// Store the job template (first writer wins; later calls are no-ops).
    async fn ensure_job(
        &self,
        job_id: &str,
        template_json: &str,
        max_pages: Option<u64>,
        max_depth: Option<u32>,
    ) -> Result<()>;

    async fn job_template(&self, job_id: &str) -> Result<Option<String>>;

    /// Atomically: check run state, depth, max_pages (admitted, not
    /// dispatched, is the budget), queue capacity, and dedup (retries with
    /// `retry_count > 0` bypass dedup and budget); only then mark seen and
    /// enqueue. Increments `received` always.
    async fn admit(&self, job_id: &str, url: &CrawlUrl, queue_cap: usize) -> Result<Admission>;

    /// Pop up to `n` highest-priority URLs whose `not_before_ms` has passed.
    /// Increments `dispatched`.
    async fn pop_ready(&self, job_id: &str, n: usize, now_ms: i64) -> Result<Vec<CrawlUrl>>;

    /// Put URLs back without touching counters (politeness said "not yet").
    async fn requeue(&self, job_id: &str, urls: Vec<CrawlUrl>) -> Result<()>;

    async fn queued(&self, job_id: &str) -> Result<u64>;

    async fn counters(&self, job_id: &str) -> Result<JobCounters>;

    async fn set_state(&self, job_id: &str, state: JobRunState) -> Result<()>;

    async fn state(&self, job_id: &str) -> Result<Option<JobRunState>>;

    async fn active_jobs(&self) -> Result<Vec<String>>;

    /// Drop queue + seen set; keep counters/state for `retention`.
    async fn release(&self, job_id: &str, retention: std::time::Duration) -> Result<()>;

    /// Per-job dispatch lease for multi-instance safety. Returns true if
    /// held by `owner`.
    async fn try_lease(&self, job_id: &str, owner: &str, ttl: std::time::Duration) -> Result<bool>;
}
