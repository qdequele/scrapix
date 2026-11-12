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
//! - Requests to one domain are one *effective delay* apart (on average
//!   below 1 s, at least from 1 s up: see "Spacing credit"):
//!   `max(adaptive delay, job min delay, 1000 / job rps, robots crawl-delay ×
//!   multiplier)`, clamped to `[min(min_delay_ms, default_delay_ms),
//!   max_delay_ms]`. The adaptive delay starts at `default_delay_ms` (the
//!   worker's `DOMAIN_DELAY_MS`) and grows ×1.5 on rate limits and error
//!   streaks. On success it follows the server's response time
//!   ([`adapt_on_success`]): halfway toward `fetch_ms /
//!   concurrent_per_domain`, so a fast server is crawled with about
//!   `concurrent_per_domain` requests in flight and a slowing one gets a
//!   longer delay, down to the `min_delay_ms` floor. The robots term applies only to jobs that respect robots.txt:
//!   it is the domain's `Crawl-delay` when robots.txt sets one, else the
//!   job's `default_crawl_delay_ms` — but only when robots.txt was actually
//!   fetched (and set none) and the job set no explicit delay
//!   (`per_domain_delay_ms` > 0) or rate (`requests_per_second`/minute).
//! - A `Retry-After` (429/503) pauses the domain until
//!   `feedback timestamp + Retry-After` (ignored if already past).
//! - A job has at most `JobLimits::max_in_flight` requests in flight.
//!
//! ## Spacing credit (SCR-90)
//!
//! The dispatcher asks for slots in passes (every `DISPATCH_INTERVAL_MS` at
//! best). Spacing requests from the *time of the last grant* loses whatever
//! time passed between the moment a request was due and the pass that
//! granted it, so the achieved rate was `1 / (ceil(delay / period) ×
//! period)` instead of `1 / delay`. The stores therefore keep a *virtual*
//! last-request time per domain (GCRA, a leaky bucket in time units):
//!
//! - a request is granted when `now >= last + delay` (`delay` is the
//!   effective delay of the asking job, evaluated at each check, so a delay
//!   raised since the last grant applies at once);
//! - the grant then sets `last = max(last + delay, now - credit,
//!   pause end)`: the domain's schedule advances by one delay, and is at
//!   most `credit` behind the clock.
//!
//! **Only the adaptive delay gets credit.** `credit` is
//! [`PolitenessConfig::spacing_credit_ms`] (100 ms) when the effective
//! delay is the domain's adaptive delay itself and is below
//! [`PolitenessConfig::strict_spacing_ms`] (1 s). It is 0 whenever an
//! explicit term sets the delay, i.e. is strictly above the adaptive delay:
//! the job's `per_domain_delay_ms`, its `requests_per_second`, the robots
//! `Crawl-delay` or the job's `default_crawl_delay_ms`. Those are promises
//! made to the job's owner or asked by the site, so they stay a floor on
//! the real gap between two requests: such a request also needs `now >=
//! real last grant + delay`, where the real last grant is tracked next to
//! the virtual one, and sets `last = now`. A request whose delay is the
//! adaptive one but whose job also has a (lower) explicit term is credited,
//! and still never comes less than that explicit term after the real last
//! grant. Jobs with different limits share a domain's state; each call is
//! judged on its own delay ([`spacing`]).
//!
//! Consequences, for a constant adaptive delay `D` and credit `C`:
//!
//! - any `n + 1` consecutive grants span at least `n × D − C`: the long-run
//!   rate never exceeds `1 / D`, and a pass that comes late grants what it
//!   is owed (several grants for one domain in one call, each taking one
//!   in-flight slot);
//! - at most `1 + floor(C / D)` grants at one instant (5 at the 25 ms
//!   floor), and never more than the free in-flight slots of the domain and
//!   the job;
//! - an explicit delay (any value), and any delay of `strict_spacing_ms` or
//!   more, is strict: one grant per call, two requests never less than the
//!   delay apart, no catching up after an idle period or a late pass,
//!   exactly as before SCR-90;
//! - no credit builds up during a pause (`Retry-After`, error streak): the
//!   schedule restarts at the pause's end. A 429 or an error drops the
//!   credit banked so far.
//! - a domain never requested before starts with no credit.
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
    /// Starting delay between requests to the same domain (milliseconds),
    /// before any response time is known
    pub default_delay_ms: u64,
    /// Lowest delay the adaptive delay may reach (milliseconds). Never
    /// raises the delay above `default_delay_ms` on its own: the floor is
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
    /// How far a domain's schedule may lag behind the clock (milliseconds):
    /// time lost between a request becoming due and the dispatcher pass
    /// that grants it is kept, up to this much, and granted back (see the
    /// module docs). `0` spaces requests from the time of the last grant.
    pub spacing_credit_ms: u64,
    /// Effective delays at or above this (milliseconds) get no spacing
    /// credit: their requests are never less than the delay apart.
    pub strict_spacing_ms: u64,
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
            spacing_credit_ms: 100,
            strict_spacing_ms: 1_000,
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
    spacing(config, adaptive_ms, robots, limits).delay_ms
}

/// How one job's requests to a domain are spaced right now (see the module
/// docs, "Spacing credit").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spacing {
    /// The effective delay: a grant needs `now >= virtual last + delay_ms`.
    pub delay_ms: u64,
    /// How far the domain's schedule may lag behind the clock. 0 (strict
    /// spacing) when an explicit term sets the delay or the delay is
    /// `strict_spacing_ms` or more.
    pub credit_ms: u64,
    /// A grant also needs `now >= real last grant + real_gap_ms`: the whole
    /// delay when spacing is strict, else the job's explicit terms (0 for a
    /// job with none).
    pub real_gap_ms: u64,
}

/// The spacing of a job's requests to a domain whose adaptive delay is
/// `adaptive_ms`. The explicit terms are the job's `min_delay_ms`, its
/// rate, and the robots term (`Crawl-delay` × multiplier, or the job's
/// default crawl delay); the spacing is strict when one of them is above
/// the (clamped) adaptive delay.
pub fn spacing(
    config: &PolitenessConfig,
    adaptive_ms: u64,
    robots: RobotsInfo,
    limits: &JobLimits,
) -> Spacing {
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
    let cap = config.max_delay_ms.max(floor);
    let explicit = limits
        .min_delay_ms
        .max(limits.rps_delay_ms())
        .max(robots)
        .min(cap);
    let adaptive = adaptive_ms.max(floor).min(cap);
    let delay_ms = adaptive.max(explicit);
    let credit_ms = if explicit > adaptive || delay_ms >= config.strict_spacing_ms {
        0
    } else {
        config.spacing_credit_ms
    };
    Spacing {
        delay_ms,
        credit_ms,
        real_gap_ms: if credit_ms == 0 { delay_ms } else { explicit },
    }
}

/// The adaptive delay after a successful fetch.
///
/// With the fetch time known, the delay moves halfway toward
/// `fetch_ms / concurrent_per_domain` (the spacing that keeps about
/// `concurrent_per_domain` requests in flight), within `[floor, max]`
/// where `floor = min(min_delay_ms, default_delay_ms)`. It drops by at most
/// ×0.9 per success, so the successes already in flight when a 429 or an
/// error streak raised it do not undo the backoff at once. Without the
/// fetch time (old crawlers), a delay raised by errors decays ×0.9 back
/// toward `default_delay_ms`, as before.
pub fn adapt_on_success(config: &PolitenessConfig, delay_ms: u64, fetch_ms: Option<u64>) -> u64 {
    match fetch_ms {
        Some(fetch_ms) => {
            let floor = config.min_delay_ms.min(config.default_delay_ms);
            let target = fetch_ms / config.concurrent_per_domain.max(1) as u64;
            let halfway = delay_ms.saturating_add(target) / 2;
            let next = if halfway < delay_ms {
                halfway.max(delay_ms.saturating_mul(9) / 10)
            } else {
                halfway
            };
            next.clamp(floor, config.max_delay_ms.max(floor))
        }
        None if delay_ms > config.default_delay_ms => {
            ((delay_ms as f64 * 0.9) as u64).max(config.default_delay_ms)
        }
        None => delay_ms,
    }
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

/// One dispatch pass asking for up to `tokens.len()` slots on one domain.
#[derive(Debug, Clone)]
pub struct SlotsRequest<'a> {
    pub domain: &'a str,
    pub job_id: &'a str,
    /// One token per wanted slot, taken in order (see
    /// [`SlotRequest::token`]).
    pub tokens: &'a [String],
    pub limits: JobLimits,
}

/// Result of [`PolitenessStore::try_acquire_many`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grants {
    /// Slots taken, for the first `granted` tokens of the request.
    pub granted: usize,
    /// What one more request for the domain would be answered right now:
    /// why the store stopped granting (`Wait`: until the domain's next
    /// grant is due; `DomainBusy`; `JobBusy`), or `Granted` when every
    /// token got a slot and another one is available.
    pub next: Acquire,
}

impl Grants {
    /// As the answer to a one-token request: the outcome and, for a grant,
    /// the wait before the domain's next one (see
    /// [`PolitenessStore::try_acquire_spaced`]).
    pub fn into_single(self) -> (Acquire, Duration) {
        match (self.granted, self.next) {
            (0, next) => (next, Duration::ZERO),
            (_, Acquire::Wait(wait)) => (Acquire::Granted, wait),
            _ => (Acquire::Granted, Duration::ZERO),
        }
    }
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
    /// How long the fetch took (ms). On success the domain's delay moves
    /// toward `fetch_ms / concurrent_per_domain` (see [`adapt_on_success`]).
    pub fetch_ms: Option<u64>,
}

/// Politeness state behind the frontier dispatcher: in memory
/// ([`PolitenessScheduler`]) or shared in Redis (`RedisPoliteness`).
#[async_trait]
pub trait PolitenessStore: Send + Sync {
    /// Atomically check the domain's delay/pause, the domain and job
    /// in-flight caps, and take a slot if all allow it.
    async fn try_acquire(&self, req: &SlotRequest<'_>) -> Result<Acquire>;
    /// [`Self::try_acquire`], plus, for a grant, how long the domain makes
    /// this job wait before its next request (the effective delay, less any
    /// spacing credit left). `Duration::ZERO` means no such wait is known:
    /// the domain may be granted again right away (a zero delay, credit
    /// left, or a store that does not report it) or is busy. Always zero
    /// when the outcome is not a grant.
    async fn try_acquire_spaced(&self, req: &SlotRequest<'_>) -> Result<(Acquire, Duration)> {
        Ok((self.try_acquire(req).await?, Duration::ZERO))
    }
    /// Atomically take as many slots as the domain's schedule, the domain
    /// cap and the job cap allow right now, at most one per token (see the
    /// module docs, "Spacing credit"), and say what one more request would
    /// get. The dispatcher sends one URL per granted token and parks the
    /// domain's other URLs for the reported wait instead of asking again.
    ///
    /// The default asks for one slot at a time; a store should override it
    /// with a single atomic call.
    async fn try_acquire_many(&self, req: &SlotsRequest<'_>) -> Result<Grants> {
        let mut granted = 0;
        for token in req.tokens {
            let one = SlotRequest {
                domain: req.domain,
                job_id: req.job_id,
                token,
                limits: req.limits,
            };
            match self.try_acquire_spaced(&one).await? {
                (Acquire::Granted, spacing) => {
                    granted += 1;
                    if !spacing.is_zero() {
                        return Ok(Grants {
                            granted,
                            next: Acquire::Wait(spacing),
                        });
                    }
                }
                (next, _) => return Ok(Grants { granted, next }),
            }
        }
        Ok(Grants {
            granted,
            next: Acquire::Granted,
        })
    }
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
    /// Virtual last request time (`None`: never requested): the time of the
    /// last grant, or up to the spacing credit earlier when that grant came
    /// late (see the module docs)
    last_request: Option<Instant>,
    /// Time of the last grant (what explicit delays are measured from);
    /// only meaningful while `last_request` is set
    last_grant: Option<Instant>,
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
            last_grant: None,
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

    fn expire(&mut self, ttl: Duration, now: Instant) {
        self.in_flight
            .retain(|s| now.saturating_duration_since(s.started) < ttl);
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
    fn pause_left(&self, now: Instant) -> Option<Duration> {
        if self.paused {
            return Some(Duration::from_secs(60));
        }
        let until = self.paused_until?;
        (until > now).then(|| until - now)
    }

    fn pause_for(&mut self, d: Duration, now: Instant) {
        let until = now + d;
        self.paused_until = Some(self.paused_until.map_or(until, |u| u.max(until)));
    }

    /// Drop the spacing credit banked so far: the next grant is due no
    /// earlier than `now` (a 429 or an error must not be followed by a
    /// catch-up burst).
    fn forfeit_credit(&mut self, now: Instant) {
        let floor = now.checked_sub(Duration::from_millis(self.delay_ms));
        if let (Some(last), Some(floor)) = (self.last_request, floor) {
            self.last_request = Some(last.max(floor));
        }
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
        if let Some(left) = state.pause_left(Instant::now()) {
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
            let now = Instant::now();
            state.last_request = Some(now);
            state.last_grant = Some(now);
        });
    }

    fn on_success(&self, state: &mut DomainState, fetch_ms: Option<u64>) {
        state.consecutive_errors = 0;
        state.delay_ms = adapt_on_success(&self.config, state.delay_ms, fetch_ms);
    }

    fn on_error(&self, domain: &str, state: &mut DomainState, is_rate_limited: bool, now: Instant) {
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
            state.pause_for(Duration::from_millis(self.config.max_delay_ms), now);
            warn!(domain, "Domain paused due to excessive errors");
        }
        state.forfeit_credit(now);
    }

    /// Record that a request completed successfully (frees one slot)
    pub fn complete_request(&self, domain: &str) {
        if let Some(state) = self.domains.write().get_mut(domain) {
            state.free(None);
            self.on_success(state, None);
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
            self.on_error(domain, state, is_rate_limited, Instant::now());
        }
    }

    /// A rate-limited failure with a server `Retry-After`: frees one slot,
    /// backs off, and pauses the domain until `retry_after` elapsed.
    pub fn failed_request_with_retry_after(&self, domain: &str, retry_after: Duration) {
        self.with_state(domain, |state| {
            state.free(None);
            let now = Instant::now();
            self.on_error(domain, state, true, now);
            state.pause_for(retry_after, now);
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
            paused: state.pause_left(Instant::now()).is_some(),
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
        self.acquire_spaced(req).0
    }

    /// Synchronous core of [`PolitenessStore::try_acquire_spaced`]: the
    /// outcome and, for a grant, the wait before the domain's next one.
    pub fn acquire_spaced(&self, req: &SlotRequest<'_>) -> (Acquire, Duration) {
        let tokens = [req.token.to_string()];
        self.acquire_many(&SlotsRequest {
            domain: req.domain,
            job_id: req.job_id,
            tokens: &tokens,
            limits: req.limits,
        })
        .into_single()
    }

    /// Synchronous core of [`PolitenessStore::try_acquire_many`].
    pub fn acquire_many(&self, req: &SlotsRequest<'_>) -> Grants {
        self.acquire_many_at(req, Instant::now())
    }

    /// [`Self::acquire_many`] at a given time (tests drive the clock).
    #[doc(hidden)]
    pub fn acquire_many_at(&self, req: &SlotsRequest<'_>, now: Instant) -> Grants {
        let ttl = self.config.slot_ttl;
        let mut domains = self.domains.write();
        let mut jobs = self.jobs.write();
        let state = domains
            .entry(req.domain.to_string())
            .or_insert_with(|| DomainState::new(self.config.default_delay_ms));
        state.expire(ttl, now);
        if let Some(left) = state.pause_left(now) {
            return Grants {
                granted: 0,
                next: Acquire::Wait(left),
            };
        }
        let mut job_held = match jobs.get_mut(req.job_id) {
            Some(slots) => {
                slots.retain(|(_, t)| now.saturating_duration_since(*t) < ttl);
                slots.len()
            }
            None => 0,
        };
        if job_held == 0 {
            jobs.remove(req.job_id);
        }
        let job_cap = req.limits.max_in_flight.filter(|c| *c > 0);
        let spacing = spacing(&self.config, state.delay_ms, state.robots, &req.limits);
        let delay = Duration::from_millis(spacing.delay_ms);
        let credit = Duration::from_millis(spacing.credit_ms);
        let real_gap = Duration::from_millis(spacing.real_gap_ms);
        // The schedule is at most `credit` behind the clock, and never
        // behind the end of a pause.
        let floor = match (now.checked_sub(credit), state.paused_until) {
            (Some(a), Some(b)) => Some(a.max(b.min(now))),
            (a, b) => a.or(b.map(|b| b.min(now))),
        };
        let mut granted = 0;
        let next = loop {
            if job_cap.is_some_and(|cap| job_held >= cap as usize) {
                break Acquire::JobBusy;
            }
            if state.in_flight.len() >= self.config.concurrent_per_domain {
                break Acquire::DomainBusy;
            }
            let due = state.last_request.map(|last| last + delay);
            // The real last grant is never before the virtual one.
            let real_due = state
                .last_request
                .map(|last| state.last_grant.map_or(last, |real| real.max(last)) + real_gap);
            if let Some(due) = due.max(real_due).filter(|due| *due > now) {
                break Acquire::Wait(due - now);
            }
            let Some(token) = req.tokens.get(granted) else {
                break Acquire::Granted;
            };
            state.last_request = Some(match (due, floor) {
                (Some(due), Some(floor)) => due.max(floor),
                (Some(due), None) => due,
                (None, _) => now,
            });
            state.last_grant = Some(now);
            state.in_flight.push(Slot {
                token: Some(token.clone()),
                started: now,
            });
            jobs.entry(req.job_id.to_string())
                .or_default()
                .push((token.clone(), now));
            job_held += 1;
            granted += 1;
        };
        Grants { granted, next }
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
        self.apply_report_at(r, Instant::now(), chrono::Utc::now().timestamp_millis())
    }

    /// [`Self::apply_report`] at a given time, `now_ms` being the same
    /// time in ms since the epoch (tests drive the clock).
    #[doc(hidden)]
    pub fn apply_report_at(&self, r: &FetchReport<'_>, now: Instant, now_ms: i64) {
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
                    FetchSignal::Success => self.on_success(state, r.fetch_ms),
                    FetchSignal::RateLimited => self.on_error(r.domain, state, true, now),
                    FetchSignal::Error => self.on_error(r.domain, state, false, now),
                    FetchSignal::NoRequest => {}
                }
            }
            if let Some(until) = r.retry_until_ms {
                let left = until - now_ms;
                if left > 0 {
                    state.pause_for(Duration::from_millis(left as u64), now);
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

    async fn try_acquire_spaced(&self, req: &SlotRequest<'_>) -> Result<(Acquire, Duration)> {
        Ok(self.acquire_spaced(req))
    }

    async fn try_acquire_many(&self, req: &SlotsRequest<'_>) -> Result<Grants> {
        Ok(self.acquire_many(req))
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
            fetch_ms: None,
        }
    }

    /// A grant reports the effective delay it was checked against (what
    /// the dispatcher parks the domain's other URLs for); every other
    /// outcome, and a zero delay, report none.
    #[test]
    fn grant_reports_the_effective_delay() {
        let p = PolitenessScheduler::new(PolitenessConfig {
            concurrent_per_domain: 8,
            default_delay_ms: 250,
            min_delay_ms: 25,
            ..Default::default()
        });
        let l = JobLimits::default();
        assert_eq!(
            p.acquire_spaced(&req("a.test", "j", "t1", l)),
            (Acquire::Granted, Duration::from_millis(250))
        );
        // Free slots do not help: the delay has not elapsed.
        let (again, spacing) = p.acquire_spaced(&req("a.test", "j", "t2", l));
        assert!(matches!(again, Acquire::Wait(_)), "{again:?}");
        assert_eq!(spacing, Duration::ZERO);
        // A job delay above the adaptive one is the one reported.
        let slow = JobLimits {
            min_delay_ms: 2_000,
            ..JobLimits::default()
        };
        assert_eq!(
            p.acquire_spaced(&req("b.test", "j", "t3", slow)),
            (Acquire::Granted, Duration::from_millis(2_000))
        );

        // Zero delay: the domain can be granted again at once.
        let p = fast(2);
        assert_eq!(
            p.acquire_spaced(&req("a.test", "j", "t1", l)),
            (Acquire::Granted, Duration::ZERO)
        );
        assert_eq!(
            p.acquire_spaced(&req("a.test", "j", "t2", l)),
            (Acquire::Granted, Duration::ZERO)
        );
        assert_eq!(
            p.acquire_spaced(&req("a.test", "j", "t3", l)),
            (Acquire::DomainBusy, Duration::ZERO)
        );
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

    fn adaptive(concurrent: usize) -> PolitenessConfig {
        PolitenessConfig {
            default_delay_ms: 250,
            min_delay_ms: 25,
            max_delay_ms: 30_000,
            concurrent_per_domain: concurrent,
            ..Default::default()
        }
    }

    #[test]
    fn success_moves_the_delay_halfway_toward_fetch_time_over_concurrency() {
        let c = adaptive(8);
        // 400 ms fetches with 8 slots: target 50 ms. Halfway is 150, but a
        // success lowers the delay by at most ×0.9.
        assert_eq!(adapt_on_success(&c, 250, Some(400)), 225);
        assert_eq!(adapt_on_success(&c, 60, Some(400)), 55);
        // A slow server raises the delay above the starting one.
        assert_eq!(adapt_on_success(&c, 250, Some(8_000)), 625);
        // Clamped to the floor and to max_delay_ms.
        assert_eq!(adapt_on_success(&c, 26, Some(0)), 25);
        assert_eq!(adapt_on_success(&c, 30_000, Some(10_000_000)), 30_000);
        // The floor never exceeds the starting delay.
        let low = PolitenessConfig {
            default_delay_ms: 10,
            ..c.clone()
        };
        assert_eq!(adapt_on_success(&low, 10, Some(0)), 10);
    }

    #[test]
    fn success_without_fetch_time_keeps_the_old_decay() {
        let c = adaptive(8);
        assert_eq!(adapt_on_success(&c, 1_000, None), 900);
        assert_eq!(adapt_on_success(&c, 260, None), 250);
        // Never below the starting delay without a fetch time.
        assert_eq!(adapt_on_success(&c, 250, None), 250);
        assert_eq!(adapt_on_success(&c, 100, None), 100);
    }

    #[test]
    fn fast_domain_converges_below_the_starting_delay() {
        let p = PolitenessScheduler::new(adaptive(8));
        let l = JobLimits::default();
        for i in 0..40 {
            let token = format!("t{i}");
            p.with_state("a.test", |s| s.last_request = None);
            assert_eq!(p.acquire(&req("a.test", "j", &token, l)), Acquire::Granted);
            p.apply_report(&FetchReport {
                fetch_ms: Some(200),
                ..report("a.test", "j", &token, FetchSignal::Success)
            });
        }
        // 200 ms / 8 slots = 25 ms: the floor.
        assert_eq!(p.effective_delay_ms("a.test"), 25);

        // A rate limit still backs off from there, and the next success
        // only takes back a tenth of it.
        p.with_state("a.test", |s| s.last_request = None);
        assert_eq!(p.acquire(&req("a.test", "j", "rl", l)), Acquire::Granted);
        p.apply_report(&report("a.test", "j", "rl", FetchSignal::RateLimited));
        assert_eq!(p.effective_delay_ms("a.test"), 37);
        p.with_state("a.test", |s| s.last_request = None);
        assert_eq!(p.acquire(&req("a.test", "j", "ok", l)), Acquire::Granted);
        p.apply_report(&FetchReport {
            fetch_ms: Some(200),
            ..report("a.test", "j", "ok", FetchSignal::Success)
        });
        assert_eq!(p.effective_delay_ms("a.test"), 33);
    }

    #[test]
    fn job_delay_and_rate_still_floor_the_adaptive_delay() {
        let c = adaptive(8);
        let none = RobotsInfo::default();
        let explicit = JobLimits {
            min_delay_ms: 100,
            ..JobLimits::default()
        };
        assert_eq!(effective_delay(&c, 25, none, &explicit), 100);
        let rate = JobLimits {
            max_rps: Some(5.0),
            ..JobLimits::default()
        };
        assert_eq!(effective_delay(&c, 25, none, &rate), 200);
        assert_eq!(effective_delay(&c, 25, none, &JobLimits::default()), 25);
    }

    // ---- Spacing credit (SCR-90) ----

    /// One domain with a fixed `delay_ms` and room for any number of
    /// requests in flight.
    fn spaced(delay_ms: u64, credit_ms: u64) -> PolitenessScheduler {
        PolitenessScheduler::new(PolitenessConfig {
            default_delay_ms: delay_ms,
            min_delay_ms: delay_ms,
            concurrent_per_domain: 100_000,
            spacing_credit_ms: credit_ms,
            ..Default::default()
        })
    }

    /// A simulated dispatcher on one domain: tokens are numbered, time is
    /// `base + ms`.
    struct Sim {
        p: PolitenessScheduler,
        base: Instant,
        limits: JobLimits,
        next_token: usize,
        /// Time (ms) of every grant, in order.
        grants: Vec<u64>,
    }

    impl Sim {
        fn new(p: PolitenessScheduler, limits: JobLimits) -> Self {
            Self {
                p,
                base: Instant::now(),
                limits,
                next_token: 0,
                grants: Vec::new(),
            }
        }

        /// A pass at `at_ms` asking for `want` slots.
        fn pass(&mut self, at_ms: u64, want: usize) -> Grants {
            let tokens: Vec<String> = (0..want)
                .map(|i| format!("t{}", self.next_token + i))
                .collect();
            let got = self.p.acquire_many_at(
                &SlotsRequest {
                    domain: "a.test",
                    job_id: "j",
                    tokens: &tokens,
                    limits: self.limits,
                },
                self.base + Duration::from_millis(at_ms),
            );
            self.next_token += got.granted;
            self.grants.extend(std::iter::repeat_n(at_ms, got.granted));
            got
        }

        /// A pass every `period_ms` over `[from_ms, to_ms)`, each asking
        /// for 50 slots.
        fn run(&mut self, from_ms: u64, to_ms: u64, period_ms: u64) {
            let mut t = from_ms;
            while t < to_ms {
                self.pass(t, 50);
                t += period_ms;
            }
        }

        fn report(&mut self, token: &str, signal: FetchSignal, at_ms: u64) {
            self.report_with(report("a.test", "j", token, signal), at_ms);
        }

        fn report_with(&mut self, r: FetchReport<'_>, at_ms: u64) {
            // The wall clock of the simulation starts at 0.
            self.p
                .apply_report_at(&r, self.base + Duration::from_millis(at_ms), at_ms as i64);
        }
    }

    /// The politeness guarantee: any `n + 1` consecutive grants span at
    /// least `n × delay − credit`, so the rate never exceeds `1 / delay` on
    /// average and a burst is at most `1 + credit / delay` grants.
    fn assert_conforms(grants: &[u64], delay_ms: u64, credit_ms: u64) {
        for (i, a) in grants.iter().enumerate() {
            for (n, b) in grants[i..].iter().enumerate() {
                assert!(
                    b - a + credit_ms >= n as u64 * delay_ms,
                    "{} grants within {} ms (delay {delay_ms}, credit {credit_ms})",
                    n + 1,
                    b - a
                );
            }
        }
    }

    fn most_at_once(grants: &[u64]) -> usize {
        grants
            .chunk_by(|a, b| a == b)
            .map(|same| same.len())
            .max()
            .unwrap_or(0)
    }

    /// The rate a domain gets is the one its delay allows, whatever the
    /// dispatcher's pass period, and never more.
    #[test]
    fn rate_follows_the_delay_whatever_the_pass_period() {
        for delay in [10u64, 25, 40, 100] {
            for period in [20u64, 25, 33] {
                let mut sim = Sim::new(spaced(delay, 100), JobLimits::default());
                sim.run(0, 1_000, period);
                // Exactly the requests due by the last pass of the second
                // (one at 0, then one per delay): no pass loses time. That
                // is `1000 / delay`, less what falls due after that pass.
                let last_pass = (999 / period) * period;
                assert_eq!(
                    sim.grants.len() as u64,
                    1 + last_pass / delay,
                    "delay {delay} ms, pass every {period} ms"
                );
                let short = 1_000 / delay - (sim.grants.len() as u64).min(1_000 / delay);
                assert!(
                    short <= (1_000 - last_pass) / delay,
                    "delay {delay} ms, pass every {period} ms: {short} grants short"
                );
                assert_conforms(&sim.grants, delay, 100);
                // Passes are at most 33 ms apart: a pass grants what became
                // due since the previous one, not the whole credit.
                assert!(
                    most_at_once(&sim.grants) as u64 <= period.div_ceil(delay).max(1),
                    "delay {delay} ms, pass every {period} ms: {} at once",
                    most_at_once(&sim.grants)
                );
            }
        }
    }

    /// Without credit (`spacing_credit_ms = 0`, the rule before SCR-90) the
    /// rate is quantized by the pass period: a 25 ms delay with a pass
    /// every 33 ms gets one grant per pass, a 40 ms delay one every other.
    #[test]
    fn without_credit_the_rate_is_quantized_by_the_pass_period() {
        let mut sim = Sim::new(spaced(25, 0), JobLimits::default());
        sim.run(0, 1_000, 33);
        assert_eq!(sim.grants.len(), 31);
        assert_eq!(most_at_once(&sim.grants), 1);
        let mut sim = Sim::new(spaced(40, 0), JobLimits::default());
        sim.run(0, 1_000, 33);
        assert_eq!(sim.grants.len(), 16);
        assert_conforms(&sim.grants, 40, 0);
    }

    /// The burst after an idle period is `1 + credit / delay` grants, and a
    /// domain never requested starts with one.
    #[test]
    fn burst_is_bounded_by_the_credit() {
        let mut sim = Sim::new(spaced(25, 100), JobLimits::default());
        let first = sim.pass(0, 50);
        assert_eq!(
            first,
            Grants {
                granted: 1,
                next: Acquire::Wait(Duration::from_millis(25))
            }
        );
        // Ten seconds idle bank 100 ms, not ten seconds: 1 + 100 / 25.
        let late = sim.pass(10_000, 50);
        assert_eq!(
            late,
            Grants {
                granted: 5,
                next: Acquire::Wait(Duration::from_millis(25))
            }
        );
        // Then the delay again.
        assert_eq!(sim.pass(10_010, 50).granted, 0);
        assert_eq!(sim.pass(10_025, 50).granted, 1);
        assert_conforms(&sim.grants, 25, 100);

        // A pass 60 ms late is owed two grants, not the whole credit.
        let mut sim = Sim::new(spaced(25, 100), JobLimits::default());
        sim.pass(0, 50);
        assert_eq!(
            sim.pass(60, 50),
            Grants {
                granted: 2,
                next: Acquire::Wait(Duration::from_millis(15))
            }
        );
        // Asking for fewer than are owed leaves the rest available.
        let mut sim = Sim::new(spaced(25, 100), JobLimits::default());
        sim.pass(0, 1);
        assert_eq!(
            sim.pass(1_000, 2),
            Grants {
                granted: 2,
                next: Acquire::Granted
            }
        );
        assert_eq!(sim.pass(1_000, 50).granted, 3);
    }

    /// A delay of a second or more (a robots `Crawl-delay`, a slow job
    /// rate) gets no credit at all: requests are never closer than it.
    #[test]
    fn long_delays_are_spaced_strictly() {
        let robots = spaced(25, 100);
        robots.set_delay("a.test", 2_000);
        let slow_job = JobLimits {
            min_delay_ms: 2_000,
            ..JobLimits::default()
        };
        let at_threshold = JobLimits {
            min_delay_ms: 1_000,
            ..JobLimits::default()
        };
        for (p, limits, delay) in [
            (robots, JobLimits::default(), 2_000),
            (spaced(25, 100), slow_job, 2_000),
            (spaced(25, 100), at_threshold, 1_000),
        ] {
            let mut sim = Sim::new(p, limits);
            sim.run(0, 9_000, 33);
            // Idle for a while, then passes again: still no catching up.
            sim.run(30_000, 39_000, 33);
            assert_conforms(&sim.grants, delay, 0);
            assert_eq!(most_at_once(&sim.grants), 1);
            for w in sim.grants.windows(2) {
                assert!(w[1] - w[0] >= delay, "{} ms apart", w[1] - w[0]);
            }
            // Each grant waits for the first pass after the delay.
            assert_eq!(sim.grants.len() as u64, 2 * (9_000 / (delay + 33) + 1));
        }
    }

    /// An explicit delay is a floor on the real gap between two requests,
    /// whatever its value: a job's `per_domain_delay_ms`, its
    /// `requests_per_second`, a robots `Crawl-delay`. No credit, no burst
    /// after an idle period, nothing given back after a late pass.
    #[test]
    fn explicit_delays_are_spaced_strictly() {
        let job = |min_delay_ms| JobLimits {
            min_delay_ms,
            ..JobLimits::default()
        };
        let rate = JobLimits {
            max_rps: Some(10.0),
            ..JobLimits::default()
        };
        let robots = spaced(25, 100);
        robots.set_delay("a.test", 300);
        for (p, limits, delay) in [
            (spaced(25, 100), job(50), 50u64),
            (spaced(25, 100), job(250), 250),
            (spaced(25, 100), rate, 100),
            (robots, JobLimits::default(), 300),
        ] {
            assert_eq!(
                spacing(
                    p.config(),
                    25,
                    p.domains
                        .read()
                        .get("a.test")
                        .map_or(RobotsInfo::default(), |s| s.robots),
                    &limits
                ),
                Spacing {
                    delay_ms: delay,
                    credit_ms: 0,
                    real_gap_ms: delay
                }
            );
            let mut sim = Sim::new(p, limits);
            sim.run(0, 3_000, 33);
            // One grant per delay, each at the first pass after it.
            let step = delay.div_ceil(33) * 33;
            assert_eq!(sim.grants.len() as u64, 1 + 2_970 / step, "delay {delay}");
            // After ten idle seconds: one grant, and the delay again.
            assert_eq!(
                sim.pass(13_000, 50),
                Grants {
                    granted: 1,
                    next: Acquire::Wait(Duration::from_millis(delay))
                },
                "delay {delay}"
            );
            assert_eq!(sim.pass(13_000 + delay - 1, 50).granted, 0);
            // A pass that comes 60 ms late: one grant, and the next one a
            // full delay after it, not 60 ms sooner.
            assert_eq!(sim.pass(13_000 + delay + 60, 50).granted, 1);
            assert_eq!(
                sim.pass(13_000 + 2 * delay + 59, 50),
                Grants {
                    granted: 0,
                    next: Acquire::Wait(Duration::from_millis(1))
                },
                "delay {delay}"
            );
            sim.run(20_000, 23_000, 33);
            assert_eq!(most_at_once(&sim.grants), 1, "delay {delay}");
            for w in sim.grants.windows(2) {
                assert!(
                    w[1] - w[0] >= delay,
                    "delay {delay}: {} ms apart",
                    w[1] - w[0]
                );
            }
        }
    }

    /// Jobs with and without an explicit delay share a domain: each call is
    /// judged on its own delay, and the strict one is measured from the
    /// real last request, not from the credited job's schedule.
    #[test]
    fn strict_job_is_never_granted_early_on_a_domain_shared_with_a_credited_job() {
        let p = spaced(25, 100);
        let base = Instant::now();
        let adaptive = JobLimits::default();
        let strict = JobLimits {
            min_delay_ms: 50,
            ..JobLimits::default()
        };
        let mut n = 0;
        let mut ask = |job: &str, limits: JobLimits, at_ms: u64, want: usize| {
            let tokens: Vec<String> = (0..want).map(|i| format!("t{}", n + i)).collect();
            n += want;
            p.acquire_many_at(
                &SlotsRequest {
                    domain: "a.test",
                    job_id: job,
                    tokens: &tokens,
                    limits,
                },
                base + Duration::from_millis(at_ms),
            )
        };
        let wait = |ms| Acquire::Wait(Duration::from_millis(ms));
        assert_eq!(ask("a", adaptive, 0, 50).granted, 1);
        // The adaptive job uses its credit after an idle second...
        assert_eq!(ask("a", adaptive, 1_000, 50).granted, 5);
        // ...and the strict job waits its 50 ms from that real request.
        assert_eq!(
            ask("b", strict, 1_010, 50),
            Grants {
                granted: 0,
                next: wait(40)
            }
        );
        assert_eq!(
            ask("b", strict, 1_050, 50),
            Grants {
                granted: 1,
                next: wait(50)
            }
        );
        // A strict grant leaves no credit behind for the adaptive job.
        assert_eq!(
            ask("a", adaptive, 1_060, 50),
            Grants {
                granted: 0,
                next: wait(15)
            }
        );
        // The adaptive job is granted late (due at 1075, granted at 1200):
        // its schedule only advances to 1100, but the strict job is spaced
        // from 1200.
        assert_eq!(ask("a", adaptive, 1_200, 1).granted, 1);
        assert_eq!(
            ask("b", strict, 1_210, 50),
            Grants {
                granted: 0,
                next: wait(40)
            }
        );
        assert_eq!(ask("b", strict, 1_250, 50).granted, 1);
    }

    /// A job's explicit delay below the adaptive delay does not set the
    /// spacing (the adaptive delay does, with its credit), but stays a
    /// floor on the real gap: never two requests at once.
    #[test]
    fn explicit_delay_below_the_adaptive_one_is_still_a_floor() {
        let limits = JobLimits {
            min_delay_ms: 50,
            ..JobLimits::default()
        };
        let p = spaced(80, 100);
        assert_eq!(
            spacing(p.config(), 80, RobotsInfo::default(), &limits),
            Spacing {
                delay_ms: 80,
                credit_ms: 100,
                real_gap_ms: 50
            }
        );
        let mut sim = Sim::new(p, limits);
        sim.run(0, 2_000, 33);
        // After an idle period: one grant, the next 50 ms later.
        assert_eq!(
            sim.pass(10_000, 50),
            Grants {
                granted: 1,
                next: Acquire::Wait(Duration::from_millis(50))
            }
        );
        sim.run(10_033, 12_000, 33);
        assert_eq!(most_at_once(&sim.grants), 1);
        for w in sim.grants.windows(2) {
            assert!(w[1] - w[0] >= 50, "{} ms apart", w[1] - w[0]);
        }
        assert_conforms(&sim.grants, 80, 100);
        // 4 s of passes at one per 80 ms, give or take a couple.
        assert!(
            (48..=52).contains(&sim.grants.len()),
            "{}",
            sim.grants.len()
        );
    }

    /// A pause (`Retry-After`) is waited out, banks no credit, and the
    /// schedule restarts at its end.
    #[test]
    fn pause_is_respected_and_banks_no_credit() {
        let mut sim = Sim::new(spaced(25, 100), JobLimits::default());
        assert_eq!(sim.pass(0, 50).granted, 1);
        sim.report_with(
            FetchReport {
                retry_until_ms: Some(1_000),
                ..report("a.test", "j", "t0", FetchSignal::NoRequest)
            },
            0,
        );
        assert_eq!(
            sim.pass(400, 50),
            Grants {
                granted: 0,
                next: Acquire::Wait(Duration::from_millis(600))
            }
        );
        assert_eq!(sim.pass(999, 50).granted, 0);
        // 60 ms after the pause: the requests due at 1000, 1025 and 1050,
        // not the five a full credit would give.
        assert_eq!(
            sim.pass(1_060, 50),
            Grants {
                granted: 3,
                next: Acquire::Wait(Duration::from_millis(15))
            }
        );
        // Right at the end of a pause: one.
        let mut sim = Sim::new(spaced(25, 100), JobLimits::default());
        sim.pass(0, 50);
        sim.report_with(
            FetchReport {
                retry_until_ms: Some(1_000),
                ..report("a.test", "j", "t0", FetchSignal::NoRequest)
            },
            0,
        );
        assert_eq!(sim.pass(1_000, 50).granted, 1);
    }

    /// A 429 raises the delay (×1.5) and drops the banked credit; the rate
    /// then follows the raised delay.
    #[test]
    fn rate_limit_drops_the_credit_and_the_backoff_holds() {
        let mut sim = Sim::new(spaced(25, 100), JobLimits::default());
        assert_eq!(sim.pass(0, 50).granted, 1);
        // Idle until 1000 (full credit), then the 429 comes in.
        sim.report("t0", FetchSignal::RateLimited, 1_000);
        assert_eq!(sim.p.effective_delay_ms("a.test"), 37);
        assert_eq!(
            sim.pass(1_000, 50),
            Grants {
                granted: 1,
                next: Acquire::Wait(Duration::from_millis(37))
            }
        );
        sim.run(1_033, 2_000, 33);
        assert_conforms(&sim.grants[1..], 37, 100);
        // 1 s at 37 ms: 27 grants, give or take one.
        let after = sim.grants.len() as i64 - 1;
        assert!((26..=28).contains(&after), "{after} grants");

        // An error streak pauses the domain: nothing is granted, whatever
        // the credit.
        let mut sim = Sim::new(spaced(25, 100), JobLimits::default());
        assert_eq!(sim.pass(0, 10).granted, 1);
        sim.run(25, 300, 25);
        for i in 0..10 {
            sim.report(&format!("t{i}"), FetchSignal::Error, 300);
        }
        let granted = sim.grants.len();
        sim.run(300, 5_000, 33);
        assert_eq!(sim.grants.len(), granted, "paused for max_delay_ms");
    }

    /// Credit never grants more than the free in-flight slots of the
    /// domain or of the job.
    #[test]
    fn credit_never_exceeds_the_in_flight_caps() {
        let capped = |concurrent: usize| {
            PolitenessScheduler::new(PolitenessConfig {
                default_delay_ms: 10,
                min_delay_ms: 10,
                concurrent_per_domain: concurrent,
                ..Default::default()
            })
        };
        // Domain cap 3: the 11 grants the credit allows become 2 (one slot
        // is held by the first request).
        let mut sim = Sim::new(capped(3), JobLimits::default());
        assert_eq!(sim.pass(0, 50).granted, 1);
        assert_eq!(
            sim.pass(5_000, 50),
            Grants {
                granted: 2,
                next: Acquire::DomainBusy
            }
        );
        assert_eq!(sim.pass(5_033, 50).granted, 0);
        // A freed slot is granted at once while credit is left.
        sim.report("t1", FetchSignal::Success, 5_040);
        assert_eq!(
            sim.pass(5_040, 50),
            Grants {
                granted: 1,
                next: Acquire::DomainBusy
            }
        );

        // Job cap 2, checked before the domain's.
        let limits = JobLimits {
            max_in_flight: Some(2),
            ..JobLimits::default()
        };
        let mut sim = Sim::new(capped(8), limits);
        assert_eq!(sim.pass(0, 50).granted, 1);
        assert_eq!(
            sim.pass(5_000, 50),
            Grants {
                granted: 1,
                next: Acquire::JobBusy
            }
        );
        assert_eq!(sim.p.job_in_flight("j"), 2);
        assert_eq!(
            sim.pass(5_100, 50),
            Grants {
                granted: 0,
                next: Acquire::JobBusy
            }
        );
    }

    /// A store that only implements the single-slot calls still serves
    /// `try_acquire_many` (one slot at a time).
    #[tokio::test]
    async fn default_try_acquire_many_asks_one_slot_at_a_time() {
        struct Single(PolitenessScheduler);
        #[async_trait]
        impl PolitenessStore for Single {
            async fn try_acquire(&self, req: &SlotRequest<'_>) -> Result<Acquire> {
                Ok(self.0.acquire(req))
            }
            async fn release(&self, d: &str, j: &str, t: &str) -> Result<()> {
                self.0.release(d, j, t).await
            }
            async fn report(&self, r: &FetchReport<'_>) -> Result<()> {
                self.0.report(r).await
            }
            fn is_shared(&self) -> bool {
                false
            }
        }
        let tokens: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        let many = |domain| SlotsRequest {
            domain,
            job_id: "j",
            tokens: &tokens,
            limits: JobLimits::default(),
        };
        let p = Single(fast(3));
        assert_eq!(
            p.try_acquire_many(&many("a.test")).await.unwrap(),
            Grants {
                granted: 3,
                next: Acquire::DomainBusy
            }
        );
        let p = Single(fast(8));
        assert_eq!(
            p.try_acquire_many(&many("a.test")).await.unwrap(),
            Grants {
                granted: 5,
                next: Acquire::Granted
            }
        );
        let p = Single(spaced(250, 100));
        let got = p.try_acquire_many(&many("a.test")).await.unwrap();
        assert_eq!(got.granted, 1);
        assert!(matches!(got.next, Acquire::Wait(_)), "{got:?}");
    }
}
