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
//! | `{p}:pol:d:{domain}` | hash | `last_ms`, `delay_ms` (adaptive), `robots_ms` (Crawl-delay, absent when none), `robots_checked` (`1` once robots.txt was fetched), `errors`, `pause_until` |
//! | `{p}:pol:f:{domain}` | zset | in-flight slots: member = token, score = expiry ms |
//! | `{p}:pol:j:{job_id}` | zset | the job's in-flight slots, same encoding |
//!
//! Expired slots (lost feedback) are dropped by `ZREMRANGEBYSCORE` before
//! every check. Domain hashes expire a day after their last use; the zsets
//! after `2 × slot_ttl`. Only commands supported by Redis 7 and DragonflyDB
//! are used, and every key is declared in `KEYS`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use redis::aio::ConnectionManager;
use redis::Script;
use scrapix_core::{Result, ScrapixError};

use crate::politeness::{
    Acquire, FetchReport, FetchSignal, PolitenessConfig, PolitenessStore, SlotRequest,
};

/// Idle domain state is forgotten after a day.
const DOMAIN_STATE_TTL_MS: u64 = 86_400_000;
/// Window of [`RedisPoliteness::tracked_domain_count`].
const RECENT_DOMAIN_WINDOW: Duration = Duration::from_secs(600);

/// `KEYS`: domain hash, domain zset, job zset.
/// `ARGV`: now, concurrent_per_domain, job cap (0 = none), default delay,
/// floor, max delay, job min delay, rps delay, respect robots (0/1), default
/// crawl delay (`0` unless the job set no explicit delay/rate), robots
/// multiplier, token, slot ttl ms, state ttl ms.
/// Returns 0 (granted), -1 (domain busy), -2 (job busy) or a wait in ms.
const ACQUIRE_LUA: &str = r#"
local now = tonumber(ARGV[1])
redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now)
redis.call('ZREMRANGEBYSCORE', KEYS[3], '-inf', now)
local h = redis.call('HMGET', KEYS[1], 'last_ms', 'delay_ms', 'robots_ms', 'pause_until', 'robots_checked')
local pause = tonumber(h[4]) or 0
if pause > now then return pause - now end
local jobcap = tonumber(ARGV[3])
if jobcap > 0 and redis.call('ZCARD', KEYS[3]) >= jobcap then return -2 end
if redis.call('ZCARD', KEYS[2]) >= tonumber(ARGV[2]) then return -1 end
local eff = tonumber(h[2]) or tonumber(ARGV[4])
eff = math.max(eff, tonumber(ARGV[7]), tonumber(ARGV[8]))
if ARGV[9] == '1' then
  local r = tonumber(h[3])
  if not r and h[5] == '1' then r = tonumber(ARGV[10]) end
  if r then eff = math.max(eff, math.floor(r * tonumber(ARGV[11]))) end
end
local floor = tonumber(ARGV[5])
eff = math.min(math.max(eff, floor), math.max(tonumber(ARGV[6]), floor))
local last = tonumber(h[1])
if last and now - last < eff then return eff - (now - last) end
local ttl = tonumber(ARGV[13])
redis.call('ZADD', KEYS[2], now + ttl, ARGV[12])
redis.call('ZADD', KEYS[3], now + ttl, ARGV[12])
redis.call('HSET', KEYS[1], 'last_ms', now)
redis.call('PEXPIRE', KEYS[1], ARGV[14])
redis.call('PEXPIRE', KEYS[2], 2 * ttl)
redis.call('PEXPIRE', KEYS[3], 2 * ttl)
return 0
"#;

/// `KEYS`: domain hash, domain zset, job zset.
/// `ARGV`: token, signal (ok|rate|error|none), crawl delay ms (`""` = none),
/// retry-until ms since epoch (`""` = none), now, default delay, max delay,
/// state ttl, robots checked (0/1).
/// Error/success accounting only applies when the slot was still held
/// (duplicate or expired feedback only updates robots and Retry-After).
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
  if delay > base then delay = math.max(math.floor(delay * 0.9), base) end
elseif sig == 'rate' or sig == 'error' then
  errors = errors + 1
  if sig == 'rate' or errors >= 3 then delay = math.min(math.floor(delay * 1.5), maxd) end
  if errors >= 10 then pause = math.max(pause, now + maxd) end
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
    ) -> Result<()> {
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
            .arg(now_ms())
            .arg(self.config.default_delay_ms)
            .arg(self.config.max_delay_ms)
            .arg(DOMAIN_STATE_TTL_MS)
            .arg(if robots_checked { "1" } else { "0" })
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        Ok(())
    }
}

#[async_trait]
impl PolitenessStore for RedisPoliteness {
    async fn try_acquire(&self, req: &SlotRequest<'_>) -> Result<Acquire> {
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
        let r: i64 = self
            .acquire
            .key(self.domain_key(req.domain))
            .key(self.flight_key(req.domain))
            .key(self.job_key(req.job_id))
            .arg(now_ms())
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
            .arg(req.token)
            .arg(c.slot_ttl.as_millis() as u64)
            .arg(DOMAIN_STATE_TTL_MS)
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        Ok(match r {
            0 => Acquire::Granted,
            -1 => Acquire::DomainBusy,
            -2 => Acquire::JobBusy,
            ms => Acquire::Wait(Duration::from_millis(ms.max(1) as u64)),
        })
    }

    async fn release(&self, domain: &str, job_id: &str, token: &str) -> Result<()> {
        self.run_report(domain, job_id, token, "none", None, false, None)
            .await
    }

    async fn report(&self, r: &FetchReport<'_>) -> Result<()> {
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
        )
        .await
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
