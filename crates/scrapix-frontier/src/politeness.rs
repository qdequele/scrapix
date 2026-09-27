//! Politeness scheduling for respectful crawling
//!
//! Ensures crawlers respect rate limits and don't overwhelm servers.
//!
//! ## Model (spec R7)
//!
//! - A domain has at most `concurrent_per_domain` requests in flight. A slot
//!   is taken when a URL is dispatched and released when the crawler reports
//!   the fetch back (`FetchFeedback`), not when the URL reaches the bus.
//!   Every slot also expires after [`PolitenessConfig::slot_ttl`], so lost
//!   feedback cannot wedge a domain.
//! - Two requests to one domain are at least the *effective delay* apart:
//!   `max(adaptive delay, job min delay, 1000 / job rps, robots crawl-delay ×
//!   multiplier)`, clamped to `[min(min_delay_ms, default_delay_ms),
//!   max_delay_ms]`. The adaptive delay starts at `default_delay_ms` (the
//!   worker's `DOMAIN_DELAY_MS`), grows on errors and shrinks back on
//!   success. The robots term applies only to jobs that respect robots.txt:
//!   it is the domain's `Crawl-delay` when robots.txt sets one, else the
//!   job's `default_crawl_delay_ms` — but only when robots.txt was actually
//!   fetched (and set none) and the job set no explicit delay
//!   (`per_domain_delay_ms` > 0) or rate (`requests_per_second`/minute).
//! - A `Retry-After` (429/503) pauses the domain until
//!   `feedback timestamp + Retry-After` (ignored if already past).
//! - A job has at most `JobLimits::max_in_flight` requests in flight.
//!
//! [`PolitenessScheduler`] keeps this state in memory (one frontier
//! instance); `RedisPoliteness` (feature `redis-store`) shares it between
//! instances. Both implement [`PolitenessStore`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::RwLock;
use scrapix_core::Result;
use tracing::{debug, warn};

/// Configuration for politeness scheduling
#[derive(Debug, Clone)]
pub struct PolitenessConfig {
    /// Default delay between requests to the same domain (milliseconds)
    pub default_delay_ms: u64,
    /// Minimum delay between requests (milliseconds). Never raises the
    /// delay above `default_delay_ms` on its own: the floor is
    /// `min(min_delay_ms, default_delay_ms)`.
    pub min_delay_ms: u64,
    /// Maximum delay between requests (milliseconds)
    pub max_delay_ms: u64,
    /// Whether to respect robots.txt crawl-delay
    pub respect_robots_delay: bool,
    /// Multiplier for robots.txt delay (e.g., 1.5 to be extra polite)
    pub robots_delay_multiplier: f64,
    /// Number of concurrent requests per domain
    pub concurrent_per_domain: usize,
    /// How long an in-flight slot is held without feedback before it is
    /// considered lost and freed (2 × the crawler request timeout).
    pub slot_ttl: Duration,
}

impl Default for PolitenessConfig {
    fn default() -> Self {
        Self {
            default_delay_ms: 1000, // 1 second
            min_delay_ms: 100,      // 100ms
            max_delay_ms: 30_000,   // 30 seconds
            respect_robots_delay: true,
            robots_delay_multiplier: 1.0,
            concurrent_per_domain: 2,
            slot_ttl: Duration::from_secs(60),
        }
    }
}

/// Per-job limits applied when dispatching that job's URLs (from the job's
/// `rate_limit` / `concurrency` config).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JobLimits {
    /// `rate_limit.per_domain_delay_ms`
    pub min_delay_ms: u64,
    /// `rate_limit.requests_per_second` (or rpm / 60), applied per domain
    pub max_rps: Option<f64>,
    /// `rate_limit.respect_robots_txt`: whether the robots term applies
    pub respect_robots: bool,
    /// `rate_limit.default_crawl_delay_ms`: the robots term when the
    /// domain's robots.txt was fetched and set no `Crawl-delay`, for a job
    /// with no explicit `min_delay_ms` (> 0) or `max_rps`
    pub default_crawl_delay_ms: u64,
    /// `concurrency.max_concurrent_requests`: per-job in-flight cap
    pub max_in_flight: Option<u32>,
}

impl Default for JobLimits {
    /// No job-level limits (robots.txt respected, like a job's default).
    fn default() -> Self {
        Self {
            min_delay_ms: 0,
            max_rps: None,
            respect_robots: true,
            default_crawl_delay_ms: 0,
            max_in_flight: None,
        }
    }
}

impl JobLimits {
    /// Minimum spacing implied by `max_rps` (0 when unset/invalid).
    pub fn rps_delay_ms(&self) -> u64 {
        match self.max_rps {
            Some(rps) if rps.is_finite() && rps > 0.0 => (1000.0 / rps).ceil() as u64,
            _ => 0,
        }
    }

    /// Whether `default_crawl_delay_ms` may stand in for a missing robots
    /// `Crawl-delay`: only when the job set no explicit delay or rate.
    pub fn uses_default_crawl_delay(&self) -> bool {
        self.min_delay_ms == 0 && self.max_rps.is_none()
    }
}

/// What the frontier knows about a domain's robots.txt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RobotsInfo {
    /// `Crawl-delay` (ms), when robots.txt sets one
    pub crawl_delay_ms: Option<u64>,
    /// robots.txt was fetched (so `crawl_delay_ms: None` means "none set")
    pub checked: bool,
}

/// The effective delay between two requests to one domain (see the module
/// docs). `adaptive_ms` is the domain's error-adapted delay.
pub fn effective_delay(
    config: &PolitenessConfig,
    adaptive_ms: u64,
    robots: RobotsInfo,
    limits: &JobLimits,
) -> u64 {
    let robots = if config.respect_robots_delay && limits.respect_robots {
        let raw = match robots.crawl_delay_ms {
            Some(d) => d,
            None if robots.checked && limits.uses_default_crawl_delay() => {
                limits.default_crawl_delay_ms
            }
            None => 0,
        };
        (raw as f64 * config.robots_delay_multiplier) as u64
    } else {
        0
    };
    let floor = config.min_delay_ms.min(config.default_delay_ms);
    adaptive_ms
        .max(limits.min_delay_ms)
        .max(limits.rps_delay_ms())
        .max(robots)
        .max(floor)
        .min(config.max_delay_ms.max(floor))
}

/// Result of [`PolitenessStore::try_acquire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquire {
    /// A slot was taken: dispatch now.
    Granted,
    /// The domain's delay (or a pause) has not elapsed: retry after this.
    Wait(Duration),
    /// The domain has `concurrent_per_domain` requests in flight.
    DomainBusy,
    /// The job has `max_in_flight` requests in flight.
    JobBusy,
}

/// One dispatch asking for a politeness slot.
#[derive(Debug, Clone)]
pub struct SlotRequest<'a> {
    pub domain: &'a str,
    pub job_id: &'a str,
    /// Identifies the slot until feedback releases it (the dispatched
    /// message id).
    pub token: &'a str,
    pub limits: JobLimits,
}

/// What a fetch told us about the domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchSignal {
    /// The server answered normally (2xx/3xx/304/4xx other than 429).
    Success,
    /// 429 or 503: back off.
    RateLimited,
    /// 5xx or a transport error (timeout, connection reset).
    Error,
    /// No request reached the domain (fail-closed before fetching): only
    /// release the slot.
    NoRequest,
}

/// Crawler feedback for one dispatched URL.
#[derive(Debug, Clone)]
pub struct FetchReport<'a> {
    pub domain: &'a str,
    pub job_id: &'a str,
    pub token: &'a str,
    pub signal: FetchSignal,
    /// robots.txt `Crawl-delay` of the domain, when known
    pub crawl_delay_ms: Option<u64>,
    /// robots.txt was fetched (`crawl_delay_ms: None` then means none set)
    pub robots_checked: bool,
    /// Server `Retry-After` as an absolute deadline (ms since epoch,
    /// measured from when the crawler saw the response): pause the domain
    /// until then; ignored if already past
    pub retry_until_ms: Option<i64>,
}

/// Politeness state behind the frontier dispatcher: in memory
/// ([`PolitenessScheduler`]) or shared in Redis (`RedisPoliteness`).
#[async_trait]
pub trait PolitenessStore: Send + Sync {
    /// Atomically check the domain's delay/pause, the domain and job
    /// in-flight caps, and take a slot if all allow it.
    async fn try_acquire(&self, req: &SlotRequest<'_>) -> Result<Acquire>;
    /// Free a slot without any accounting (the dispatch never reached the
    /// domain, e.g. the bus refused it).
    async fn release(&self, domain: &str, job_id: &str, token: &str) -> Result<()>;
    /// Apply crawler feedback: free the slot and adapt the domain's delay.
    async fn report(&self, report: &FetchReport<'_>) -> Result<()>;
    /// Whether the state is shared between frontier instances (then one
    /// instance per feedback message is enough).
    fn is_shared(&self) -> bool;
    /// Number of domains with local state (metrics; 0 when not tracked).
    fn tracked_domain_count(&self) -> usize {
        0
    }
}

/// One in-flight request.
struct Slot {
    /// `None` for the token-less `start_request` API.
    token: Option<String>,
    started: Instant,
}

/// Per-domain scheduling state
struct DomainState {
    /// Last request time (`None`: never requested)
    last_request: Option<Instant>,
    /// Error-adapted delay (starts at `default_delay_ms`)
    delay_ms: u64,
    /// robots.txt `Crawl-delay` / whether robots.txt was fetched
    robots: RobotsInfo,
    /// Limits used by the token-less API (`set_job_limits`)
    job_limits: JobLimits,
    /// Currently in-flight requests (oldest first)
    in_flight: Vec<Slot>,
    /// Paused until `resume_domain`
    paused: bool,
    /// Paused until this instant (Retry-After, error streaks)
    paused_until: Option<Instant>,
    /// Consecutive errors (for adaptive rate limiting)
    consecutive_errors: u32,
}

impl DomainState {
    fn new(delay_ms: u64) -> Self {
        Self {
            last_request: None,
            delay_ms,
            robots: RobotsInfo::default(),
            job_limits: JobLimits::default(),
            in_flight: Vec::new(),
            paused: false,
            paused_until: None,
            consecutive_errors: 0,
        }
    }

    fn live_in_flight(&self, ttl: Duration) -> usize {
        self.in_flight
            .iter()
            .filter(|s| s.started.elapsed() < ttl)
            .count()
    }

    fn expire(&mut self, ttl: Duration) {
        self.in_flight.retain(|s| s.started.elapsed() < ttl);
    }

    /// Remove the slot for `token` (or the oldest token-less/any slot when
    /// `token` is `None`). Returns whether a slot was freed.
    fn free(&mut self, token: Option<&str>) -> bool {
        let idx = match token {
            Some(t) => self
                .in_flight
                .iter()
                .position(|s| s.token.as_deref() == Some(t)),
            None => (!self.in_flight.is_empty()).then_some(0),
        };
        match idx {
            Some(i) => {
                self.in_flight.remove(i);
                true
            }
            None => false,
        }
    }

    /// Remaining pause, if any.
    fn pause_left(&self) -> Option<Duration> {
        if self.paused {
            return Some(Duration::from_secs(60));
        }
        let until = self.paused_until?;
        let now = Instant::now();
        (until > now).then(|| until - now)
    }

    fn pause_for(&mut self, d: Duration) {
        let until = Instant::now() + d;
        self.paused_until = Some(self.paused_until.map_or(until, |u| u.max(until)));
    }

    /// Time left before the delay since the last request elapsed.
    fn delay_left(&self, delay_ms: u64) -> Duration {
        let required = Duration::from_millis(delay_ms);
        match self.last_request {
            Some(last) => required.saturating_sub(last.elapsed()),
            None => Duration::ZERO,
        }
    }
}

/// In-memory politeness scheduler (per frontier instance).
pub struct PolitenessScheduler {
    config: PolitenessConfig,
    domains: RwLock<HashMap<String, DomainState>>,
    /// Per-job in-flight slots: job id → (token, start).
    jobs: RwLock<HashMap<String, Vec<(String, Instant)>>>,
}

impl PolitenessScheduler {
    /// Create a new politeness scheduler
    pub fn new(config: PolitenessConfig) -> Self {
        Self {
            config,
            domains: RwLock::new(HashMap::new()),
            jobs: RwLock::new(HashMap::new()),
        }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(PolitenessConfig::default())
    }

    /// The configuration.
    pub fn config(&self) -> &PolitenessConfig {
        &self.config
    }

    fn state_delay(&self, state: &DomainState, limits: &JobLimits) -> u64 {
        effective_delay(&self.config, state.delay_ms, state.robots, limits)
    }

    /// Check if a request to a domain can be made (token-less API; uses the
    /// limits from [`Self::set_job_limits`]).
    pub fn can_fetch(&self, domain: &str) -> bool {
        self.wait_time(domain).is_zero()
    }

    /// Get the wait time until a domain can be fetched
    pub fn wait_time(&self, domain: &str) -> Duration {
        let domains = self.domains.read();
        let Some(state) = domains.get(domain) else {
            return Duration::ZERO;
        };
        if let Some(left) = state.pause_left() {
            return left;
        }
        if state.live_in_flight(self.config.slot_ttl) >= self.config.concurrent_per_domain {
            return Duration::from_millis(100); // Short poll interval
        }
        state.delay_left(self.state_delay(state, &state.job_limits))
    }

    /// The effective delay for `domain` with the limits from
    /// [`Self::set_job_limits`].
    pub fn effective_delay_ms(&self, domain: &str) -> u64 {
        let domains = self.domains.read();
        match domains.get(domain) {
            Some(state) => self.state_delay(state, &state.job_limits),
            None => effective_delay(
                &self.config,
                self.config.default_delay_ms,
                RobotsInfo::default(),
                &JobLimits::default(),
            ),
        }
    }

    fn with_state<R>(&self, domain: &str, f: impl FnOnce(&mut DomainState) -> R) -> R {
        let mut domains = self.domains.write();
        let state = domains
            .entry(domain.to_string())
            .or_insert_with(|| DomainState::new(self.config.default_delay_ms));
        f(state)
    }

    /// Record that a request is starting
    pub fn start_request(&self, domain: &str) {
        self.with_state(domain, |state| {
            state.in_flight.push(Slot {
                token: None,
                started: Instant::now(),
            });
            state.last_request = Some(Instant::now());
        });
    }

    fn on_success(&self, state: &mut DomainState) {
        state.consecutive_errors = 0;
        // Gradually reduce delay back toward default after recovery
        if state.delay_ms > self.config.default_delay_ms {
            state.delay_ms =
                ((state.delay_ms as f64 * 0.9) as u64).max(self.config.default_delay_ms);
        }
    }

    fn on_error(&self, domain: &str, state: &mut DomainState, is_rate_limited: bool) {
        state.consecutive_errors += 1;

        // Adaptive backoff
        if is_rate_limited || state.consecutive_errors >= 3 {
            let new_delay = (state.delay_ms as f64 * 1.5) as u64;
            state.delay_ms = new_delay.min(self.config.max_delay_ms);
            warn!(
                domain,
                new_delay_ms = state.delay_ms,
                consecutive_errors = state.consecutive_errors,
                "Increasing delay due to errors"
            );
        }

        // Pause the domain for a while if too many errors (timed, so a
        // flaky domain cannot wedge a job forever)
        if state.consecutive_errors >= 10 {
            state.pause_for(Duration::from_millis(self.config.max_delay_ms));
            warn!(domain, "Domain paused due to excessive errors");
        }
    }

    /// Record that a request completed successfully (frees one slot)
    pub fn complete_request(&self, domain: &str) {
        if let Some(state) = self.domains.write().get_mut(domain) {
            state.free(None);
            self.on_success(state);
        }
    }

    /// Release an in-flight slot without any success/error accounting: no
    /// delay change, no error streak, no pause. For requests that never
    /// reached the domain (e.g. the message bus refused the dispatch).
    pub fn release_slot(&self, domain: &str) {
        if let Some(state) = self.domains.write().get_mut(domain) {
            state.free(None);
        }
    }

    /// Record that a request failed (frees one slot)
    pub fn failed_request(&self, domain: &str, is_rate_limited: bool) {
        if let Some(state) = self.domains.write().get_mut(domain) {
            state.free(None);
            self.on_error(domain, state, is_rate_limited);
        }
    }

    /// A rate-limited failure with a server `Retry-After`: frees one slot,
    /// backs off, and pauses the domain until `retry_after` elapsed.
    pub fn failed_request_with_retry_after(&self, domain: &str, retry_after: Duration) {
        self.with_state(domain, |state| {
            state.free(None);
            self.on_error(domain, state, true);
            state.pause_for(retry_after);
        });
    }

    /// Set the robots.txt `Crawl-delay` for a domain (multiplied by
    /// `robots_delay_multiplier` and clamped when computing the delay).
    pub fn set_delay(&self, domain: &str, delay_ms: u64) {
        self.with_state(domain, |state| {
            state.robots = RobotsInfo {
                crawl_delay_ms: Some(delay_ms),
                checked: true,
            }
        });
        debug!(domain, delay_ms, "Set domain robots delay");
    }

    /// Set the job limits the token-less API applies to `domain`.
    pub fn set_job_limits(&self, domain: &str, min_delay_ms: u64, max_rps: Option<f64>) {
        self.with_state(domain, |state| {
            state.job_limits = JobLimits {
                min_delay_ms,
                max_rps,
                ..JobLimits::default()
            };
        });
    }

    /// Pause crawling for a domain
    pub fn pause_domain(&self, domain: &str) {
        self.with_state(domain, |state| state.paused = true);
    }

    /// Resume crawling for a domain
    pub fn resume_domain(&self, domain: &str) {
        if let Some(state) = self.domains.write().get_mut(domain) {
            state.paused = false;
            state.paused_until = None;
            state.consecutive_errors = 0;
        }
    }

    /// Get stats for a domain (`delay_ms` is the effective delay)
    pub fn domain_stats(&self, domain: &str) -> Option<DomainStats> {
        let domains = self.domains.read();

        domains.get(domain).map(|state| DomainStats {
            delay_ms: self.state_delay(state, &state.job_limits),
            in_flight: state.live_in_flight(self.config.slot_ttl),
            paused: state.pause_left().is_some(),
            consecutive_errors: state.consecutive_errors,
            time_since_last_request_ms: state
                .last_request
                .map_or(u64::MAX, |t| t.elapsed().as_millis() as u64),
        })
    }

    /// Get all tracked domains
    pub fn tracked_domains(&self) -> Vec<String> {
        let domains = self.domains.read();
        domains.keys().cloned().collect()
    }

    /// Clear state for a domain
    pub fn clear_domain(&self, domain: &str) {
        let mut domains = self.domains.write();
        domains.remove(domain);
    }

    /// Clear all domain states
    pub fn clear_all(&self) {
        self.domains.write().clear();
        self.jobs.write().clear();
    }

    /// Number of live in-flight slots held by `job_id`.
    pub fn job_in_flight(&self, job_id: &str) -> usize {
        let ttl = self.config.slot_ttl;
        self.jobs
            .read()
            .get(job_id)
            .map_or(0, |v| v.iter().filter(|(_, t)| t.elapsed() < ttl).count())
    }

    /// Synchronous core of [`PolitenessStore::try_acquire`].
    pub fn acquire(&self, req: &SlotRequest<'_>) -> Acquire {
        let ttl = self.config.slot_ttl;
        let mut domains = self.domains.write();
        let mut jobs = self.jobs.write();
        let state = domains
            .entry(req.domain.to_string())
            .or_insert_with(|| DomainState::new(self.config.default_delay_ms));
        state.expire(ttl);
        if let Some(left) = state.pause_left() {
            return Acquire::Wait(left);
        }
        let job_held = match jobs.get_mut(req.job_id) {
            Some(slots) => {
                slots.retain(|(_, t)| t.elapsed() < ttl);
                slots.len()
            }
            None => 0,
        };
        if job_held == 0 {
            jobs.remove(req.job_id);
        }
        if let Some(cap) = req.limits.max_in_flight.filter(|c| *c > 0) {
            if job_held >= cap as usize {
                return Acquire::JobBusy;
            }
        }
        if state.in_flight.len() >= self.config.concurrent_per_domain {
            return Acquire::DomainBusy;
        }
        let left = state.delay_left(self.state_delay(state, &req.limits));
        if !left.is_zero() {
            return Acquire::Wait(left);
        }
        let now = Instant::now();
        state.in_flight.push(Slot {
            token: Some(req.token.to_string()),
            started: now,
        });
        state.last_request = Some(now);
        jobs.entry(req.job_id.to_string())
            .or_default()
            .push((req.token.to_string(), now));
        Acquire::Granted
    }

    /// Free `token`'s slots. Returns whether this instance held the domain
    /// slot (feedback for another instance's dispatch returns false).
    fn free_token(&self, domain: &str, job_id: &str, token: &str) -> bool {
        {
            let mut jobs = self.jobs.write();
            if let Some(slots) = jobs.get_mut(job_id) {
                slots.retain(|(t, _)| t != token);
                if slots.is_empty() {
                    jobs.remove(job_id);
                }
            }
        }
        self.domains
            .write()
            .get_mut(domain)
            .is_some_and(|s| s.free(Some(token)))
    }

    /// Synchronous core of [`PolitenessStore::report`].
    pub fn apply_report(&self, r: &FetchReport<'_>) {
        let owned = self.free_token(r.domain, r.job_id, r.token);
        self.with_state(r.domain, |state| {
            if r.crawl_delay_ms.is_some() || r.robots_checked {
                state.robots = RobotsInfo {
                    crawl_delay_ms: r.crawl_delay_ms,
                    checked: true,
                };
            }
            // Error accounting only for this instance's own dispatches, so a
            // failure seen by every instance is not counted N times.
            if owned {
                match r.signal {
                    FetchSignal::Success => self.on_success(state),
                    FetchSignal::RateLimited => self.on_error(r.domain, state, true),
                    FetchSignal::Error => self.on_error(r.domain, state, false),
                    FetchSignal::NoRequest => {}
                }
            }
            if let Some(until) = r.retry_until_ms {
                let left = until - chrono::Utc::now().timestamp_millis();
                if left > 0 {
                    state.pause_for(Duration::from_millis(left as u64));
                }
            }
        });
    }
}

#[async_trait]
impl PolitenessStore for PolitenessScheduler {
    async fn try_acquire(&self, req: &SlotRequest<'_>) -> Result<Acquire> {
        Ok(self.acquire(req))
    }

    async fn release(&self, domain: &str, job_id: &str, token: &str) -> Result<()> {
        self.free_token(domain, job_id, token);
        Ok(())
    }

    async fn report(&self, report: &FetchReport<'_>) -> Result<()> {
        self.apply_report(report);
        Ok(())
    }

    fn is_shared(&self) -> bool {
        false
    }

    fn tracked_domain_count(&self) -> usize {
        self.domains.read().len()
    }
}

/// Statistics for a domain
#[derive(Debug, Clone)]
pub struct DomainStats {
    /// Current delay between requests (ms)
    pub delay_ms: u64,
    /// Number of in-flight requests
    pub in_flight: usize,
    /// Whether the domain is paused
    pub paused: bool,
    /// Number of consecutive errors
    pub consecutive_errors: u32,
    /// Time since last request (ms)
    pub time_since_last_request_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn slot_is_held_until_feedback() {
        let p = PolitenessScheduler::new(PolitenessConfig {
            concurrent_per_domain: 1,
            default_delay_ms: 0,
            min_delay_ms: 0,
            ..Default::default()
        });
        assert!(p.can_fetch("a.test"));
        p.start_request("a.test");
        assert!(
            !p.can_fetch("a.test"),
            "in flight until the crawler reports back"
        );
        p.complete_request("a.test");
        assert!(p.can_fetch("a.test"));
    }

    #[test]
    fn robots_crawl_delay_and_job_rps_raise_the_delay() {
        let p = PolitenessScheduler::new(PolitenessConfig {
            default_delay_ms: 50,
            min_delay_ms: 0,
            ..Default::default()
        });
        p.set_delay("a.test", 2_000); // robots Crawl-delay: 2
        p.set_job_limits("a.test", 0, Some(0.25)); // 4 s per request
        assert_eq!(p.effective_delay_ms("a.test"), 4_000);
    }

    #[test]
    fn retry_after_pauses_domain() {
        let p = PolitenessScheduler::with_defaults();
        p.start_request("a.test");
        p.failed_request_with_retry_after("a.test", std::time::Duration::from_secs(60));
        assert!(!p.can_fetch("a.test"));
    }

    fn fast(concurrent: usize) -> PolitenessScheduler {
        PolitenessScheduler::new(PolitenessConfig {
            concurrent_per_domain: concurrent,
            default_delay_ms: 0,
            min_delay_ms: 0,
            ..Default::default()
        })
    }

    fn req<'a>(
        domain: &'a str,
        job: &'a str,
        token: &'a str,
        limits: JobLimits,
    ) -> SlotRequest<'a> {
        SlotRequest {
            domain,
            job_id: job,
            token,
            limits,
        }
    }

    fn report<'a>(
        domain: &'a str,
        job: &'a str,
        token: &'a str,
        signal: FetchSignal,
    ) -> FetchReport<'a> {
        FetchReport {
            domain,
            job_id: job,
            token,
            signal,
            crawl_delay_ms: None,
            robots_checked: false,
            retry_until_ms: None,
        }
    }

    #[test]
    fn token_slot_is_released_only_by_its_own_feedback() {
        let p = fast(1);
        let l = JobLimits::default();
        assert_eq!(p.acquire(&req("a.test", "j", "t1", l)), Acquire::Granted);
        assert_eq!(p.acquire(&req("a.test", "j", "t2", l)), Acquire::DomainBusy);
        // Feedback for a dispatch this instance never made frees nothing.
        p.apply_report(&report("a.test", "j", "other", FetchSignal::Success));
        assert_eq!(p.acquire(&req("a.test", "j", "t2", l)), Acquire::DomainBusy);
        p.apply_report(&report("a.test", "j", "t1", FetchSignal::Success));
        assert_eq!(p.acquire(&req("a.test", "j", "t2", l)), Acquire::Granted);
    }

    #[test]
    fn lost_feedback_expires_after_slot_ttl() {
        let p = PolitenessScheduler::new(PolitenessConfig {
            concurrent_per_domain: 1,
            default_delay_ms: 0,
            min_delay_ms: 0,
            slot_ttl: Duration::from_millis(50),
            ..Default::default()
        });
        let l = JobLimits {
            max_in_flight: Some(1),
            ..JobLimits::default()
        };
        assert_eq!(p.acquire(&req("a.test", "j", "t1", l)), Acquire::Granted);
        assert_eq!(p.acquire(&req("b.test", "j", "t2", l)), Acquire::JobBusy);
        sleep(Duration::from_millis(70));
        assert_eq!(p.acquire(&req("a.test", "j", "t3", l)), Acquire::Granted);
    }

    #[test]
    fn job_cap_limits_in_flight_across_domains() {
        let p = fast(10);
        let l = JobLimits {
            max_in_flight: Some(2),
            ..JobLimits::default()
        };
        assert_eq!(p.acquire(&req("a.test", "j", "t1", l)), Acquire::Granted);
        assert_eq!(p.acquire(&req("b.test", "j", "t2", l)), Acquire::Granted);
        assert_eq!(p.acquire(&req("c.test", "j", "t3", l)), Acquire::JobBusy);
        // Another job is not affected.
        assert_eq!(p.acquire(&req("c.test", "k", "t4", l)), Acquire::Granted);
        assert_eq!(p.job_in_flight("j"), 2);
        p.apply_report(&report("b.test", "j", "t2", FetchSignal::NoRequest));
        assert_eq!(p.acquire(&req("c.test", "j", "t3", l)), Acquire::Granted);
    }

    #[test]
    fn report_with_retry_after_pauses_and_crawl_delay_applies_to_robots_jobs() {
        let p = fast(10);
        let l = JobLimits::default();
        assert_eq!(p.acquire(&req("a.test", "j", "t1", l)), Acquire::Granted);
        p.apply_report(&FetchReport {
            retry_until_ms: Some(chrono::Utc::now().timestamp_millis() + 60_000),
            crawl_delay_ms: Some(3_000),
            ..report("a.test", "j", "t1", FetchSignal::RateLimited)
        });
        assert!(
            matches!(p.acquire(&req("a.test", "j", "t2", l)), Acquire::Wait(d) if d > Duration::from_secs(50))
        );
        p.resume_domain("a.test");
        // robots Crawl-delay 3 s (since t1) applies to a job that respects
        // robots.txt...
        assert!(
            matches!(p.acquire(&req("a.test", "j", "t2", l)), Acquire::Wait(d) if d > Duration::from_millis(2_500))
        );
        // ...but not to one that ignores it.
        let ignore = JobLimits {
            respect_robots: false,
            ..l
        };
        assert_eq!(
            p.acquire(&req("a.test", "k", "t3", ignore)),
            Acquire::Granted
        );
    }

    #[test]
    fn default_crawl_delay_applies_when_robots_set_none() {
        let p = fast(10);
        let l = JobLimits {
            default_crawl_delay_ms: 1_000,
            ..JobLimits::default()
        };
        let checked = |d: Option<u64>| RobotsInfo {
            crawl_delay_ms: d,
            checked: true,
        };
        assert_eq!(effective_delay(p.config(), 0, checked(None), &l), 1_000);
        assert_eq!(effective_delay(p.config(), 0, checked(Some(200)), &l), 200);
        // robots.txt never fetched: no default.
        assert_eq!(effective_delay(p.config(), 0, RobotsInfo::default(), &l), 0);
        // An explicit job delay or rate replaces the default.
        let explicit = JobLimits {
            min_delay_ms: 200,
            ..l
        };
        assert_eq!(
            effective_delay(p.config(), 0, checked(None), &explicit),
            200
        );
        let rate = JobLimits {
            max_rps: Some(10.0),
            ..l
        };
        assert_eq!(effective_delay(p.config(), 0, checked(None), &rate), 100);
        let mult = PolitenessConfig {
            robots_delay_multiplier: 1.5,
            min_delay_ms: 0,
            default_delay_ms: 0,
            ..Default::default()
        };
        assert_eq!(effective_delay(&mult, 0, checked(Some(2_000)), &l), 3_000);
        // Clamped to max_delay_ms.
        assert_eq!(
            effective_delay(&mult, 0, checked(Some(600_000)), &l),
            30_000
        );
    }

    #[test]
    fn release_slot_frees_in_flight_without_error_accounting() {
        let config = PolitenessConfig {
            default_delay_ms: 0,
            concurrent_per_domain: 1,
            ..Default::default()
        };
        let scheduler = PolitenessScheduler::new(config);
        let before = scheduler.domain_stats("example.com");
        assert!(before.is_none());

        // Many more releases than `failed_request` would tolerate before
        // pausing the domain.
        for _ in 0..25 {
            scheduler.start_request("example.com");
            assert!(!scheduler.can_fetch("example.com"), "slot is taken");
            scheduler.release_slot("example.com");
        }
        let stats = scheduler.domain_stats("example.com").unwrap();
        assert_eq!(stats.in_flight, 0);
        assert_eq!(stats.consecutive_errors, 0);
        assert!(!stats.paused);
        assert_eq!(stats.delay_ms, 0);
        assert!(scheduler.can_fetch("example.com"));

        // Saturates at zero and ignores unknown domains.
        scheduler.release_slot("example.com");
        scheduler.release_slot("unknown.test");
        assert_eq!(scheduler.domain_stats("example.com").unwrap().in_flight, 0);
        assert!(scheduler.domain_stats("unknown.test").is_none());
    }

    #[test]
    fn test_first_request_allowed() {
        let scheduler = PolitenessScheduler::with_defaults();
        assert!(scheduler.can_fetch("example.com"));
    }

    #[test]
    fn test_delay_enforced() {
        let config = PolitenessConfig {
            default_delay_ms: 100,
            ..Default::default()
        };
        let scheduler = PolitenessScheduler::new(config);

        scheduler.start_request("example.com");
        scheduler.complete_request("example.com");

        // Should not be able to fetch immediately
        assert!(!scheduler.can_fetch("example.com"));

        // Wait for delay
        sleep(Duration::from_millis(120));
        assert!(scheduler.can_fetch("example.com"));
    }

    #[test]
    fn test_concurrent_limit() {
        let config = PolitenessConfig {
            default_delay_ms: 0, // No delay
            concurrent_per_domain: 2,
            ..Default::default()
        };
        let scheduler = PolitenessScheduler::new(config);

        scheduler.start_request("example.com");
        assert!(scheduler.can_fetch("example.com")); // 1 in flight, limit is 2

        scheduler.start_request("example.com");
        assert!(!scheduler.can_fetch("example.com")); // 2 in flight, at limit

        scheduler.complete_request("example.com");
        assert!(scheduler.can_fetch("example.com")); // Back to 1 in flight
    }

    #[test]
    fn test_pause_resume() {
        let scheduler = PolitenessScheduler::with_defaults();

        scheduler.pause_domain("example.com");
        assert!(!scheduler.can_fetch("example.com"));

        scheduler.resume_domain("example.com");
        assert!(scheduler.can_fetch("example.com"));
    }

    #[test]
    fn test_adaptive_backoff() {
        let config = PolitenessConfig {
            default_delay_ms: 100,
            max_delay_ms: 1000,
            ..Default::default()
        };
        let scheduler = PolitenessScheduler::new(config);

        scheduler.start_request("example.com");

        // Simulate rate limiting
        scheduler.failed_request("example.com", true);

        let stats = scheduler.domain_stats("example.com").unwrap();
        assert!(stats.delay_ms > 100); // Delay should have increased
    }

    #[test]
    fn test_set_delay_from_robots() {
        let scheduler = PolitenessScheduler::with_defaults();

        scheduler.set_delay("example.com", 5000);

        let stats = scheduler.domain_stats("example.com").unwrap();
        assert_eq!(stats.delay_ms, 5000);
    }

    #[test]
    fn test_delay_reduces_after_recovery() {
        let config = PolitenessConfig {
            default_delay_ms: 100,
            max_delay_ms: 10_000,
            ..Default::default()
        };
        let scheduler = PolitenessScheduler::new(config);

        // Simulate 3 failures to increase delay
        scheduler.start_request("example.com");
        scheduler.failed_request("example.com", false);
        scheduler.start_request("example.com");
        scheduler.failed_request("example.com", false);
        scheduler.start_request("example.com");
        scheduler.failed_request("example.com", false);

        let elevated = scheduler.domain_stats("example.com").unwrap().delay_ms;
        assert!(elevated > 100, "Delay should have increased: {}", elevated);

        // Simulate 10 successes — delay should reduce toward default
        for _ in 0..10 {
            scheduler.start_request("example.com");
            scheduler.complete_request("example.com");
        }

        let recovered = scheduler.domain_stats("example.com").unwrap().delay_ms;
        assert!(
            recovered < elevated,
            "Delay should decrease after recovery: {} vs {}",
            recovered,
            elevated
        );
    }

    #[test]
    fn test_delay_stays_elevated_during_errors() {
        let config = PolitenessConfig {
            default_delay_ms: 100,
            max_delay_ms: 10_000,
            ..Default::default()
        };
        let scheduler = PolitenessScheduler::new(config);

        // 3 failures
        for _ in 0..3 {
            scheduler.start_request("example.com");
            scheduler.failed_request("example.com", false);
        }
        let after_first_batch = scheduler.domain_stats("example.com").unwrap().delay_ms;

        // 1 success (partial recovery)
        scheduler.start_request("example.com");
        scheduler.complete_request("example.com");

        // 3 more failures — delay should increase again
        for _ in 0..3 {
            scheduler.start_request("example.com");
            scheduler.failed_request("example.com", false);
        }
        let after_second_batch = scheduler.domain_stats("example.com").unwrap().delay_ms;

        assert!(
            after_second_batch >= after_first_batch,
            "Delay should increase again after new errors: {} vs {}",
            after_second_batch,
            after_first_batch
        );
    }

    #[test]
    fn test_delay_cannot_exceed_max() {
        let config = PolitenessConfig {
            default_delay_ms: 100,
            max_delay_ms: 1000,
            ..Default::default()
        };
        let scheduler = PolitenessScheduler::new(config);

        // Many failures
        for _ in 0..20 {
            scheduler.start_request("example.com");
            scheduler.failed_request("example.com", true);
        }

        let stats = scheduler.domain_stats("example.com").unwrap();
        assert!(
            stats.delay_ms <= 1000,
            "Delay should not exceed max: {}",
            stats.delay_ms
        );
    }
}
