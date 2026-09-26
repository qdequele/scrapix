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
/// is randomized per process — a future Redis-backed store needs the same
/// hash on both sides.
fn hash_url(url: &str) -> u64 {
    let mut hasher = siphasher::sip::SipHasher13::new();
    hasher.write(url.as_bytes());
    hasher.finish()
}

struct JobEntry {
    template: String,
    max_pages: Option<u64>,
    max_depth: Option<u32>,
    state: JobRunState,
    counters: JobCounters,
    seen: HashSet<u64>,
    queue: BTreeMap<(Reverse<i32>, u64), CrawlUrl>,
    seq: u64,
    /// Set by `release`; once passed, the whole entry is evicted lazily on
    /// the next access, since "keep counters/state for `retention`" implies
    /// they need not be kept forever.
    release_deadline: Option<Instant>,
}

impl JobEntry {
    fn new(template: &str, max_pages: Option<u64>, max_depth: Option<u32>) -> Self {
        Self {
            template: template.to_string(),
            max_pages,
            max_depth,
            state: JobRunState::Paused,
            counters: JobCounters::default(),
            seen: HashSet::new(),
            queue: BTreeMap::new(),
            seq: 0,
            release_deadline: None,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
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
    if entry.queue.len() >= queue_cap {
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
    jobs.retain(|_, entry| entry.release_deadline.map_or(true, |deadline| now < deadline));
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

fn not_found(job_id: &str) -> ScrapixError {
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
        Ok(jobs.get(job_id).map(|e| e.template.clone()))
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
            let seq = entry.next_seq();
            entry
                .queue
                .insert((Reverse(url.priority), seq), url.clone());
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

        // Collect the keys of the first `n` *ready* URLs in priority/FIFO
        // order, skipping (not removing) any URL whose `not_before_ms` is
        // still in the future so it doesn't block due URLs behind it.
        let mut keys = Vec::with_capacity(n.min(entry.queue.len()));
        for (key, candidate) in entry.queue.iter() {
            if keys.len() >= n {
                break;
            }
            let ready = candidate.not_before_ms.map_or(true, |t| t <= now_ms);
            if ready {
                keys.push(*key);
            }
        }

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
            let seq = entry.next_seq();
            entry.queue.insert((Reverse(url.priority), seq), url);
        }
        Ok(())
    }

    async fn queued(&self, job_id: &str) -> Result<u64> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        Ok(jobs.get(job_id).map(|e| e.queue.len() as u64).unwrap_or(0))
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
        Ok(jobs.keys().cloned().collect())
    }

    async fn release(&self, job_id: &str, retention: Duration) -> Result<()> {
        let mut jobs = self.jobs.lock();
        evict_expired(&mut jobs);
        if let Some(entry) = jobs.get_mut(job_id) {
            entry.counters.dropped += entry.queue.len() as u64;
            entry.queue.clear();
            entry.seen.clear();
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
