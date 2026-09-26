//! Frontier storage abstraction.
//!
//! `FrontierStore` defines the state a distributed frontier needs: per-job
//! run state, counters, a priority queue of ready-to-dispatch URLs, and a
//! dedup set. This module provides [`MemoryFrontierStore`], an in-process
//! reference implementation, validated by the conformance suite in
//! [`conformance`], and (behind the `redis-store` feature)
//! `RedisFrontierStore`, which must behave identically to it — that suite is
//! written generically over `Arc<dyn FrontierStore>` so both run it
//! unchanged.
//!
//! ## Admission order
//!
//! `admit` evaluates, for a single [`CrawlUrl`], in this exact order:
//!
//! 1. **state** — the job must be [`JobRunState::Running`] or
//!    [`JobRunState::Paused`] (a paused job keeps queuing the links its
//!    in-flight pages discover; it only stops dispatching); otherwise
//!    (`Cancelled` / `Finished`) [`Admission::JobNotRunning`].
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
//! ## Storage layout: two structures, not one (contract for Task 10's Redis store)
//!
//! A naive single ready-queue design would make `pop_ready` scan past every
//! not-yet-due URL to find a due one behind it — `O(queue)` in the worst
//! case (e.g. many delayed URLs sorted ahead of a due one by priority).
//! Every implementation must instead split a job's pending URLs into two
//! structures:
//!
//! - **`queue`** — URLs that are ready to dispatch *right now*, ordered by
//!   descending priority then FIFO sequence. `pop_ready` only ever pops
//!   from here.
//! - **`later`** — URLs admitted with a `not_before_ms`, ordered by
//!   `(not_before_ms, seq)`.
//!
//! At `admit`/`requeue` time, a URL carrying `not_before_ms: Some(_)` goes
//! into `later` *unconditionally* — regardless of whether that timestamp
//! has already passed — rather than checked against a clock. Only
//! `pop_ready`, which already receives `now_ms` from its caller, needs to
//! be clock-aware; this keeps `admit`/`requeue` deterministic and
//! clock-free, which is also what keeps the conformance suite's `now_ms`
//! parameterization sufficient to exercise this path.
//!
//! `pop_ready(n, now_ms)` runs in two phases:
//!
//! 1. **Promote** — move every `later` entry whose `not_before_ms <=
//!    now_ms` into `queue`, each with a *fresh* sequence number (so
//!    promotion order among ties follows promotion time, not original
//!    admission time). This is `O(k log n)`, where `k` is the number
//!    promoted in this call — never a scan of the whole map, and `k` is
//!    bounded by how many delayed URLs are actually due, not by how many
//!    are still waiting.
//! 2. **Pop** — take up to `n` entries off the front of `queue`. This never
//!    needs to skip anything, because `queue` holds only ready URLs:
//!    `O(n log n)`.
//!
//! `admit`'s capacity check and `queued()` both use `queue.len() +
//! later.len()` — a full queue is full regardless of which structure a
//! pending URL happens to sit in.
//!
//! A Redis-backed implementation should mirror this with two keys (e.g. a
//! sorted set `q` for the ready queue and a sorted set `later` keyed by
//! `not_before_ms`), promoting due members from `later` into `q` the same
//! way before popping.
//!
//! ## Known caveat: no claim/ack step between pop and send
//!
//! `pop_ready` removes URLs from the store immediately. The caller then
//! either sends each URL downstream or puts it back with `requeue`. If the
//! caller crashes between `pop_ready` and that send/`requeue`, the URLs of
//! that in-flight batch are lost: they are marked seen, already counted in
//! `admitted`/`dispatched`, and no longer pending. There is no
//! claim-with-visibility-timeout step that would let another instance
//! recover them. Callers keep the window small by popping adaptively sized
//! batches.
//!
//! ## Behavior for an unknown or released job
//!
//! Every implementation must agree on this, since it's the contract a
//! Redis-backed store must mirror bit-for-bit:
//!
//! - `admit` and `set_state` return `Err` — the job must exist. A caller
//!   that forgot `ensure_job`, or that raced past a job's `release`
//!   retention window, has a bug; these two methods are where mutating a
//!   job that doesn't exist would silently do nothing useful, so they
//!   surface it instead.
//! - `pop_ready` returns `Ok(vec![])`.
//! - `requeue` is a silent no-op: `Ok(())`.
//! - `queued` returns `Ok(0)`.
//! - `counters` returns `Ok(JobCounters::default())`.
//! - `state` and `job_template` return `Ok(None)`.
//! - `release` is itself a no-op: `Ok(())`.
//! - `active_jobs` simply omits it.
//!
//! `release` (while the job still exists, during its retention window)
//! also makes `job_template` return `None` and removes the job from
//! `active_jobs` immediately, even though `counters`/`state` remain
//! queryable until the retention window elapses and the whole entry is
//! evicted.

mod memory;
#[cfg(feature = "redis-store")]
mod redis;

#[cfg(any(test, feature = "conformance"))]
pub mod conformance;

#[cfg(feature = "redis-store")]
pub use self::redis::RedisFrontierStore;
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
    /// URLs popped via `pop_ready` and not put back via `requeue`, i.e.
    /// dispatch attempts that actually left the frontier (a retry that is
    /// re-admitted and popped again counts again).
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

    /// `Ok(None)` for an unknown job, and also once the job has been
    /// `release`d (even though `counters`/`state` remain queryable).
    async fn job_template(&self, job_id: &str) -> Result<Option<String>>;

    /// Atomically: check run state, depth, max_pages (admitted, not
    /// dispatched, is the budget), queue capacity (ready + delayed URLs
    /// combined — see the module docs), and dedup (retries with
    /// `retry_count > 0` bypass dedup and budget); only then mark seen and
    /// enqueue. Increments `received` always. `Err` for an unknown job —
    /// see the module docs' missing-job section.
    async fn admit(&self, job_id: &str, url: &CrawlUrl, queue_cap: usize) -> Result<Admission>;

    /// Pop up to `n` highest-priority URLs whose `not_before_ms` has passed,
    /// promoting due delayed URLs first (see the module docs' storage
    /// layout section — this never scans past a not-yet-due URL). Returns
    /// `Ok(vec![])` for an unknown job. Increments `dispatched`.
    async fn pop_ready(&self, job_id: &str, n: usize, now_ms: i64) -> Result<Vec<CrawlUrl>>;

    /// Put URLs previously returned by `pop_ready` back (politeness said "not
    /// yet"). Undoes the pop: `dispatched` is decremented by `urls.len()`
    /// (floored at 0); every other counter — in particular the `admitted`
    /// budget — is untouched. A no-op for an unknown job. For a job
    /// `release`d since the pop (e.g. cancelled mid-batch) the URLs are not
    /// queued again but counted in `dropped`.
    async fn requeue(&self, job_id: &str, urls: Vec<CrawlUrl>) -> Result<()>;

    /// Total pending URLs (ready + delayed). `0` for an unknown job.
    async fn queued(&self, job_id: &str) -> Result<u64>;

    /// `JobCounters::default()` for an unknown job.
    async fn counters(&self, job_id: &str) -> Result<JobCounters>;

    /// `Err` for an unknown job — see the module docs' missing-job section.
    async fn set_state(&self, job_id: &str, state: JobRunState) -> Result<()>;

    /// `Ok(None)` for an unknown job.
    async fn state(&self, job_id: &str) -> Result<Option<JobRunState>>;

    /// Jobs that exist and have not been `release`d. A released job is
    /// omitted immediately, even before its retention window elapses.
    async fn active_jobs(&self) -> Result<Vec<String>>;

    /// Drop queue + seen set + template; keep counters/state queryable for
    /// `retention`. A no-op for an unknown job.
    async fn release(&self, job_id: &str, retention: std::time::Duration) -> Result<()>;

    /// Per-job dispatch lease for multi-instance safety. Returns true if
    /// held by `owner`.
    async fn try_lease(&self, job_id: &str, owner: &str, ttl: std::time::Duration) -> Result<bool>;
}
