//! In-memory reference implementation of [`FrontierStore`].

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hasher;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use scrapix_core::{CrawlUrl, Result, ScrapixError};

use super::{Admission, FrontierStore, JobCounters, JobRunState};

/// Stable 64-bit hash of a URL, used only for the seen-set. `siphasher` with
/// fixed keys (the default `SipHasher13::new()`) is deterministic across
/// processes, unlike `std::collections::hash_map::DefaultHasher` whose seed
/// is randomized per process — the Redis-backed store (`redis.rs`) reuses
/// this exact hash so both stores agree on the seen-set contents.
pub(super) fn hash_url(url: &str) -> u64 {
    let mut hasher = siphasher::sip::SipHasher13::new();
    hasher.write(url.as_bytes());
    hasher.finish()
}

struct JobEntry {
    /// `None` once `release` has run (see the trait docs on missing-job /
    /// released-job semantics), even though the entry itself sticks around
    /// for `counters`/`state` until its retention window elapses.
    template: Option<String>,
    max_pages: Option<u64>,
    max_depth: Option<u32>,
    state: JobRunState,
    counters: JobCounters,
    seen: HashSet<u64>,
    /// URLs ready to dispatch right now, ordered by descending priority
    /// then FIFO sequence. See the module docs for why `pop_ready` never
    /// needs to scan past a not-yet-due URL.
    queue: BTreeMap<(Reverse<i32>, u64), CrawlUrl>,
    /// URLs admitted with a `not_before_ms`, ordered by `(not_before_ms,
    /// seq)`. `pop_ready` promotes due entries from here into `queue`.
    later: BTreeMap<(i64, u64), CrawlUrl>,
    seq: u64,
    /// Set by `release`; once passed, the whole entry is evicted lazily on
    /// the next access, since "keep counters/state for `retention`" implies
    /// they need not be kept forever.
    release_deadline: Option<Instant>,
}

impl JobEntry {
    fn new(template: &str, max_pages: Option<u64>, max_depth: Option<u32>) -> Self {
        Self {
            template: Some(template.to_string()),
            max_pages,
            max_depth,
            state: JobRunState::Paused,
            counters: JobCounters::default(),
            seen: HashSet::new(),
            queue: BTreeMap::new(),
            later: BTreeMap::new(),
            seq: 0,
            release_deadline: None,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn pending_len(&self) -> usize {
        self.queue.len() + self.later.len()
    }

    /// Enqueue an already-admitted/requeued URL into whichever of
    /// `queue`/`later` it belongs in, per the module docs: any URL carrying
    /// a `not_before_ms` goes to `later` unconditionally (no clock check
    /// here — only `pop_ready` is clock-aware), everything else goes
    /// straight into the ready `queue`.
    fn enqueue(&mut self, url: CrawlUrl) {
        let seq = self.next_seq();
        if let Some(not_before_ms) = url.not_before_ms {
            self.later.insert((not_before_ms, seq), url);
        } else {
            self.queue.insert((Reverse(url.priority), seq), url);
        }
    }
}

/// Decide the outcome of admitting `url` into `entry`, without mutating
/// anything. This is the single place the admission order documented on
/// [`FrontierStore::admit`] is encoded; nothing else in this crate
/// duplicates it, and a future Redis Lua script must mirror it exactly:
/// state -> depth -> retry bypass (skips budget + dedup) -> budget ->
/// capacity -> dedup -> enqueue.
fn decide_admission(entry: &JobEntry, url: &CrawlUrl, queue_cap: usize, hash: u64) -> Admission {
    if entry.state != JobRunState::Running {
        return Admission::JobNotRunning;
    }
    if let Some(max_depth) = entry.max_depth {
        if url.depth > max_depth {
            return Admission::OverDepth;
        }
    }
    let is_retry = url.retry_count > 0;
    if !is_retry {
        if let Some(max_pages) = entry.max_pages {
            if entry.counters.admitted >= max_pages {
                return Admission::OverMaxPages;
            }
        }
    }
    if entry.pending_len() >= queue_cap {
        return Admission::QueueFull;
    }
    if !is_retry && entry.seen.contains(&hash) {
        return Admission::Duplicate;
    }
    Admission::Admitted
}

/// Remove job entries whose `release` retention window has elapsed.
fn evict_expired(jobs: &mut HashMap<String, JobEntry>) {
    let now = Instant::now();
    jobs.retain(|_, entry| {
        entry
            .release_deadline
            .map_or(true, |deadline| now < deadline)
    });
}

/// In-memory [`FrontierStore`]. State lives entirely in process memory
/// behind a single mutex — fine as the reference implementation and for
/// single-instance deployments, but lost on restart and not shared across
/// instances (that's what the Redis-backed store is for).
#[derive(Default)]
pub struct MemoryFrontierStore {
    jobs: Mutex<HashMap<String, JobEntry>>,
    leases: Mutex<HashMap<String, (String, Instant)>>,
}

pub(super) fn not_found(job_id: &str) -> ScrapixError {
    ScrapixError::NotFound(format!("frontier job `{job_id}` (ensure_job not called)"))
}

#[async_trait]
impl FrontierStore for MemoryFrontierStore {
    async fn ensure_job(
        &self,
        job_id: &str,
        template_json: &str,
        max_pages: Option<u64>,
        max_depth: Option<u32>,
    ) -> Result<()> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        jobs.entry(job_id.to_string())
            .or_insert_with(|| JobEntry::new(template_json, max_pages, max_depth));
        Ok(())
    }

    async fn job_template(&self, job_id: &str) -> Result<Option<String>> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        Ok(jobs.get(job_id).and_then(|e| e.template.clone()))
    }

    async fn admit(&self, job_id: &str, url: &CrawlUrl, queue_cap: usize) -> Result<Admission> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        let entry = jobs.get_mut(job_id).ok_or_else(|| not_found(job_id))?;

        entry.counters.received += 1;
        let hash = hash_url(&url.url);
        let decision = decide_admission(entry, url, queue_cap, hash);

        if decision == Admission::Admitted {
            let is_retry = url.retry_count > 0;
            if !is_retry {
                entry.seen.insert(hash);
                entry.counters.admitted += 1;
            }
            entry.enqueue(url.clone());
        } else {
            entry.counters.rejected += 1;
        }

        Ok(decision)
    }

    async fn pop_ready(&self, job_id: &str, n: usize, now_ms: i64) -> Result<Vec<CrawlUrl>> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        let Some(entry) = jobs.get_mut(job_id) else {
            return Ok(Vec::new());
        };

        // Phase 1 — promote: move every `later` entry whose `not_before_ms`
        // has passed into `queue`, with a fresh sequence number. `later` is
        // ordered by `(not_before_ms, seq)`, so the due entries are exactly
        // the ones at or before `(now_ms, u64::MAX)` — an O(k log n) range
        // operation, never a scan of the whole map.
        let due_keys: Vec<(i64, u64)> = entry
            .later
            .range(..=(now_ms, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        for key in due_keys {
            if let Some(url) = entry.later.remove(&key) {
                let seq = entry.next_seq();
                entry.queue.insert((Reverse(url.priority), seq), url);
            }
        }

        // Phase 2 — pop: `queue` now holds only ready URLs, so popping the
        // first `n` never needs to skip anything.
        let keys: Vec<_> = entry.queue.keys().take(n).copied().collect();
        let mut popped = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(url) = entry.queue.remove(&key) {
                popped.push(url);
            }
        }
        entry.counters.dispatched += popped.len() as u64;
        Ok(popped)
    }

    async fn requeue(&self, job_id: &str, urls: Vec<CrawlUrl>) -> Result<()> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        let Some(entry) = jobs.get_mut(job_id) else {
            return Ok(());
        };
        for url in urls {
            entry.enqueue(url);
        }
        Ok(())
    }

    async fn queued(&self, job_id: &str) -> Result<u64> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        Ok(jobs
            .get(job_id)
            .map(|e| e.pending_len() as u64)
            .unwrap_or(0))
    }

    async fn counters(&self, job_id: &str) -> Result<JobCounters> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        Ok(jobs
            .get(job_id)
            .map(|e| e.counters.clone())
            .unwrap_or_default())
    }

    async fn set_state(&self, job_id: &str, state: JobRunState) -> Result<()> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        let entry = jobs.get_mut(job_id).ok_or_else(|| not_found(job_id))?;
        entry.state = state;
        Ok(())
    }

    async fn state(&self, job_id: &str) -> Result<Option<JobRunState>> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        Ok(jobs.get(job_id).map(|e| e.state))
    }

    async fn active_jobs(&self) -> Result<Vec<String>> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        Ok(jobs
            .iter()
            .filter(|(_, e)| e.release_deadline.is_none())
            .map(|(job_id, _)| job_id.clone())
            .collect())
    }

    async fn release(&self, job_id: &str, retention: Duration) -> Result<()> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        if let Some(entry) = jobs.get_mut(job_id) {
            entry.counters.dropped += entry.pending_len() as u64;
            entry.queue.clear();
            entry.later.clear();
            entry.seen.clear();
            entry.template = None;
            entry.release_deadline = Some(Instant::now() + retention);
        }
        Ok(())
    }

    async fn try_lease(&self, job_id: &str, owner: &str, ttl: Duration) -> Result<bool> {
        let mut leases = self.leases.lock();
        let now = Instant::now();
        let grant = match leases.get(job_id) {
            Some((held_by, expires_at)) => *expires_at <= now || held_by == owner,
            None => true,
        };
        if grant {
            leases.insert(job_id.to_string(), (owner.to_string(), now + ttl));
        }
        Ok(grant)
    }
}
