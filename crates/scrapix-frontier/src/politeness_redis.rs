//! Redis/DragonflyDB-backed [`PolitenessStore`]: politeness state shared by
//! every frontier instance (spec R7).
//!
//! Semantics mirror [`crate::PolitenessScheduler`] (see the
//! [`crate::politeness`] module docs); the delay formula is the same
//! [`crate::politeness::effective_delay`], evaluated inside the Lua script so
//! the check-and-take is atomic across instances.
//!
//! ## Key layout (`p` = key prefix)
//!
//! | Key | Type | Contents |
//! |-----|------|----------|
//! | `{p}:pol:d:{domain}` | hash | `last_ms` (virtual last request: the last grant's time, or up to the spacing credit earlier), `delay_ms` (adaptive), `robots_ms` (Crawl-delay, absent when none), `robots_checked` (`1` once robots.txt was fetched), `errors`, `pause_until` |
//! | `{p}:pol:f:{domain}` | zset | in-flight slots: member = token, score = expiry ms |
//! | `{p}:pol:j:{job_id}` | zset | the job's in-flight slots, same encoding |
//!
//! Expired slots (lost feedback) are dropped by `ZREMRANGEBYSCORE` before
//! every check. Domain hashes expire a day after their last use; the zsets
//! after `2 × slot_ttl`. Only commands supported by Redis 7 and DragonflyDB
//! are used, and every key is declared in `KEYS`.
//!
//! ## Mixed versions
//!
//! `last_ms` is the only spacing state, as before SCR-90, so instances
//! running the previous script can share the keys with this one. An old
//! instance grants when `now - last_ms >= delay` and writes `last_ms = now`:
//! under this script's rule that is a grant that banks no credit. It reads
//! a `last_ms` written here as the last request time, so it may grant up to
//! the credit (100 ms) earlier than a full delay after the real last
//! request, which is what this script would do too. Either way a domain's
//! long-run rate stays at most `1 / delay`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use redis::aio::ConnectionManager;
use redis::Script;
use scrapix_core::{Result, ScrapixError};

use crate::politeness::{
    Acquire, FetchReport, FetchSignal, Grants, PolitenessConfig, PolitenessStore, SlotRequest,
    SlotsRequest,
};

/// Idle domain state is forgotten after a day.
const DOMAIN_STATE_TTL_MS: u64 = 86_400_000;
/// Window of [`RedisPoliteness::tracked_domain_count`].
const RECENT_DOMAIN_WINDOW: Duration = Duration::from_secs(600);

/// `KEYS`: domain hash, domain zset, job zset.
/// `ARGV`: now, concurrent_per_domain, job cap (0 = none), default delay,
/// floor, max delay, job min delay, rps delay, respect robots (0/1), default
/// crawl delay (`0` unless the job set no explicit delay/rate), robots
/// multiplier, spacing credit ms, slot ttl ms, state ttl ms, strict spacing
/// ms, then one token per wanted slot.
/// Returns `{status, ms, granted}`: `granted` slots were taken (for the
/// first `granted` tokens), and one more request would get `status`: `0`
/// (a slot, only when every token was granted), `1` (wait `ms`: the delay
/// or a pause has not elapsed), `-1` (domain busy) or `-2` (job busy).
///
/// The spacing rule is the one of [`crate::politeness`] ("Spacing credit"):
/// a grant needs `now - last_ms >= effective delay` and sets `last_ms =
/// max(last_ms + delay, now - credit, pause_until)` (`now` for a domain
/// never requested), with no credit for a delay at or above the strict
/// spacing threshold. So one call grants at most `1 + floor(credit /
/// delay)` slots, and never more than the domain and the job have free.
const ACQUIRE_LUA: &str = r#"
local now = tonumber(ARGV[1])
redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now)
redis.call('ZREMRANGEBYSCORE', KEYS[3], '-inf', now)
local h = redis.call('HMGET', KEYS[1], 'last_ms', 'delay_ms', 'robots_ms', 'pause_until', 'robots_checked')
local pause = tonumber(h[4]) or 0
if pause > now then return {1, pause - now, 0} end
local eff = tonumber(h[2]) or tonumber(ARGV[4])
eff = math.max(eff, tonumber(ARGV[7]), tonumber(ARGV[8]))
if ARGV[9] == '1' then
  local r = tonumber(h[3])
  if not r and h[5] == '1' then r = tonumber(ARGV[10]) end
  if r then eff = math.max(eff, math.floor(r * tonumber(ARGV[11]))) end
end
local floor = tonumber(ARGV[5])
eff = math.min(math.max(eff, floor), math.max(tonumber(ARGV[6]), floor))
local credit = tonumber(ARGV[12])
if eff >= tonumber(ARGV[15]) then credit = 0 end
local domcap = tonumber(ARGV[2])
local jobcap = tonumber(ARGV[3])
local dom_n = redis.call('ZCARD', KEYS[2])
local job_n = redis.call('ZCARD', KEYS[3])
local ttl = tonumber(ARGV[13])
local want = #ARGV - 15
local last = tonumber(h[1])
local granted = 0
local status = 0
local ms = 0
while true do
  if jobcap > 0 and job_n >= jobcap then status = -2 break end
  if dom_n >= domcap then status = -1 break end
  if last and now - last < eff then
    status = 1
    ms = eff - (now - last)
    break
  end
  if granted >= want then break end
  if last then last = math.max(last + eff, now - credit, pause) else last = now end
  granted = granted + 1
  local token = ARGV[15 + granted]
  redis.call('ZADD', KEYS[2], now + ttl, token)
  redis.call('ZADD', KEYS[3], now + ttl, token)
  dom_n = dom_n + 1
  job_n = job_n + 1
end
if granted > 0 then
  redis.call('HSET', KEYS[1], 'last_ms', last)
  redis.call('PEXPIRE', KEYS[1], ARGV[14])
  redis.call('PEXPIRE', KEYS[2], 2 * ttl)
  redis.call('PEXPIRE', KEYS[3], 2 * ttl)
end
return {status, ms, granted}
"#;

/// `KEYS`: domain hash, domain zset, job zset.
/// `ARGV`: token, signal (ok|rate|error|none), crawl delay ms (`""` = none),
/// retry-until ms since epoch (`""` = none), now, default delay, max delay,
/// state ttl, robots checked (0/1), fetch ms (`""` = none),
/// concurrent_per_domain, floor.
/// On success the delay follows [`crate::politeness::adapt_on_success`].
/// Error/success accounting only applies when the slot was still held
/// (duplicate or expired feedback only updates robots and Retry-After).
/// A rate limit or an error also drops the spacing credit banked so far
/// (`last_ms` is raised to `now - delay`).
const REPORT_LUA: &str = r#"
local owned = redis.call('ZREM', KEYS[2], ARGV[1])
redis.call('ZREM', KEYS[3], ARGV[1])
local now = tonumber(ARGV[5])
local base = tonumber(ARGV[6])
local maxd = tonumber(ARGV[7])
local h = redis.call('HMGET', KEYS[1], 'delay_ms', 'errors', 'pause_until')
local delay = tonumber(h[1]) or base
local errors = tonumber(h[2]) or 0
local pause = tonumber(h[3]) or 0
local sig = ARGV[2]
if owned == 0 then sig = 'none' end
if sig == 'ok' then
  errors = 0
  if ARGV[10] ~= '' then
    local floor = tonumber(ARGV[12])
    local target = math.floor(tonumber(ARGV[10]) / tonumber(ARGV[11]))
    local halfway = math.floor((delay + target) / 2)
    if halfway < delay then halfway = math.max(halfway, math.floor(delay * 9 / 10)) end
    delay = halfway
    delay = math.min(math.max(delay, floor), math.max(maxd, floor))
  elseif delay > base then delay = math.max(math.floor(delay * 0.9), base) end
elseif sig == 'rate' or sig == 'error' then
  errors = errors + 1
  if sig == 'rate' or errors >= 3 then delay = math.min(math.floor(delay * 1.5), maxd) end
  if errors >= 10 then pause = math.max(pause, now + maxd) end
  local last = tonumber(redis.call('HGET', KEYS[1], 'last_ms'))
  if last and last < now - delay then redis.call('HSET', KEYS[1], 'last_ms', now - delay) end
end
if ARGV[4] ~= '' then pause = math.max(pause, tonumber(ARGV[4])) end
redis.call('HSET', KEYS[1], 'delay_ms', delay, 'errors', errors, 'pause_until', pause)
if ARGV[3] ~= '' then
  redis.call('HSET', KEYS[1], 'robots_ms', ARGV[3], 'robots_checked', 1)
elseif ARGV[9] == '1' then
  redis.call('HDEL', KEYS[1], 'robots_ms')
  redis.call('HSET', KEYS[1], 'robots_checked', 1)
end
redis.call('PEXPIRE', KEYS[1], ARGV[8])
return 1
"#;

fn storage_err(err: redis::RedisError) -> ScrapixError {
    ScrapixError::Storage(format!("redis politeness: {err}"))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Politeness state in Redis, shared by all frontier instances.
pub struct RedisPoliteness {
    conn: ConnectionManager,
    prefix: String,
    config: PolitenessConfig,
    acquire: Script,
    report: Script,
    /// Domains this instance asked a slot for recently (metrics only).
    recent_domains: Mutex<HashMap<String, Instant>>,
}

impl RedisPoliteness {
    /// Connect to `url`; keys are namespaced under `key_prefix`.
    pub async fn new(url: &str, key_prefix: &str, config: PolitenessConfig) -> Result<Self> {
        let client = redis::Client::open(url).map_err(storage_err)?;
        let conn = ConnectionManager::new(client).await.map_err(storage_err)?;
        Ok(Self {
            conn,
            prefix: key_prefix.to_string(),
            config,
            acquire: Script::new(ACQUIRE_LUA),
            report: Script::new(REPORT_LUA),
            recent_domains: Mutex::new(HashMap::new()),
        })
    }

    fn domain_key(&self, domain: &str) -> String {
        format!("{}:pol:d:{}", self.prefix, domain)
    }

    fn flight_key(&self, domain: &str) -> String {
        format!("{}:pol:f:{}", self.prefix, domain)
    }

    fn job_key(&self, job_id: &str) -> String {
        format!("{}:pol:j:{}", self.prefix, job_id)
    }

    /// [`PolitenessStore::try_acquire_many`] at a given time, in ms since
    /// the epoch (tests drive the clock).
    #[doc(hidden)]
    pub async fn acquire_many_at(&self, req: &SlotsRequest<'_>, now_ms: i64) -> Result<Grants> {
        let c = &self.config;
        let l = &req.limits;
        let respect = c.respect_robots_delay && l.respect_robots;
        let default_crawl = if l.uses_default_crawl_delay() {
            l.default_crawl_delay_ms
        } else {
            0
        };
        {
            let mut recent = self.recent_domains.lock();
            if recent.len() >= 100_000 {
                recent.retain(|_, t| t.elapsed() < RECENT_DOMAIN_WINDOW);
            }
            recent.insert(req.domain.to_string(), Instant::now());
        }
        let mut conn = self.conn.clone();
        let mut call = self.acquire.prepare_invoke();
        call.key(self.domain_key(req.domain))
            .key(self.flight_key(req.domain))
            .key(self.job_key(req.job_id))
            .arg(now_ms)
            .arg(c.concurrent_per_domain)
            .arg(l.max_in_flight.unwrap_or(0))
            .arg(c.default_delay_ms)
            .arg(c.min_delay_ms.min(c.default_delay_ms))
            .arg(c.max_delay_ms)
            .arg(l.min_delay_ms)
            .arg(l.rps_delay_ms())
            .arg(if respect { "1" } else { "0" })
            .arg(default_crawl)
            .arg(c.robots_delay_multiplier)
            .arg(c.spacing_credit_ms)
            .arg(c.slot_ttl.as_millis() as u64)
            .arg(DOMAIN_STATE_TTL_MS)
            .arg(c.strict_spacing_ms);
        for token in req.tokens {
            call.arg(token.as_str());
        }
        let (status, ms, granted): (i64, i64, i64) =
            call.invoke_async(&mut conn).await.map_err(storage_err)?;
        Ok(Grants {
            granted: granted.max(0) as usize,
            next: match status {
                0 => Acquire::Granted,
                -1 => Acquire::DomainBusy,
                -2 => Acquire::JobBusy,
                _ => Acquire::Wait(Duration::from_millis(ms.max(1) as u64)),
            },
        })
    }

    /// [`PolitenessStore::report`] at a given time, in ms since the epoch
    /// (tests drive the clock).
    #[doc(hidden)]
    pub async fn report_at(&self, r: &FetchReport<'_>, now_ms: i64) -> Result<()> {
        let signal = match r.signal {
            FetchSignal::Success => "ok",
            FetchSignal::RateLimited => "rate",
            FetchSignal::Error => "error",
            FetchSignal::NoRequest => "none",
        };
        self.run_report(
            r.domain,
            r.job_id,
            r.token,
            signal,
            r.crawl_delay_ms,
            r.robots_checked,
            r.retry_until_ms,
            r.fetch_ms,
            now_ms,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_report(
        &self,
        domain: &str,
        job_id: &str,
        token: &str,
        signal: &str,
        crawl_delay_ms: Option<u64>,
        robots_checked: bool,
        retry_until_ms: Option<i64>,
        fetch_ms: Option<u64>,
        now_ms: i64,
    ) -> Result<()> {
        let c = &self.config;
        let mut conn = self.conn.clone();
        let _: i64 = self
            .report
            .key(self.domain_key(domain))
            .key(self.flight_key(domain))
            .key(self.job_key(job_id))
            .arg(token)
            .arg(signal)
            .arg(crawl_delay_ms.map(|v| v.to_string()).unwrap_or_default())
            .arg(retry_until_ms.map(|v| v.to_string()).unwrap_or_default())
            .arg(now_ms)
            .arg(c.default_delay_ms)
            .arg(c.max_delay_ms)
            .arg(DOMAIN_STATE_TTL_MS)
            .arg(if robots_checked { "1" } else { "0" })
            .arg(fetch_ms.map(|v| v.to_string()).unwrap_or_default())
            .arg(c.concurrent_per_domain.max(1))
            .arg(c.min_delay_ms.min(c.default_delay_ms))
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        Ok(())
    }
}

#[async_trait]
impl PolitenessStore for RedisPoliteness {
    async fn try_acquire(&self, req: &SlotRequest<'_>) -> Result<Acquire> {
        Ok(self.try_acquire_spaced(req).await?.0)
    }

    async fn try_acquire_spaced(&self, req: &SlotRequest<'_>) -> Result<(Acquire, Duration)> {
        let tokens = [req.token.to_string()];
        let many = SlotsRequest {
            domain: req.domain,
            job_id: req.job_id,
            tokens: &tokens,
            limits: req.limits,
        };
        Ok(self.try_acquire_many(&many).await?.into_single())
    }

    async fn try_acquire_many(&self, req: &SlotsRequest<'_>) -> Result<Grants> {
        self.acquire_many_at(req, now_ms()).await
    }

    async fn release(&self, domain: &str, job_id: &str, token: &str) -> Result<()> {
        self.run_report(
            domain,
            job_id,
            token,
            "none",
            None,
            false,
            None,
            None,
            now_ms(),
        )
        .await
    }

    async fn report(&self, r: &FetchReport<'_>) -> Result<()> {
        self.report_at(r, now_ms()).await
    }

    fn is_shared(&self) -> bool {
        true
    }

    /// Domains this instance dispatched to (or tried to) in the last 10
    /// minutes — the shared state itself is not counted (no SCAN).
    fn tracked_domain_count(&self) -> usize {
        let mut recent = self.recent_domains.lock();
        recent.retain(|_, t| t.elapsed() < RECENT_DOMAIN_WINDOW);
        recent.len()
    }
}
