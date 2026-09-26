//! Redis/DragonflyDB-backed [`FrontierStore`].
//!
//! Frontier state lives in Redis, so it survives a frontier restart and can
//! be shared by several frontier instances. Every mutating operation that
//! reads-then-writes is a single Lua script (run via `EVALSHA`, falling back
//! to `EVAL` on `NOSCRIPT`), so concurrent instances cannot interleave inside
//! a decision. The behavior mirrors [`super::MemoryFrontierStore`] exactly
//! and is validated by the same conformance suite
//! (`crates/scrapix-frontier/tests/store_redis.rs`).
//!
//! Only commands and Lua features supported by both Redis 7 (production)
//! and DragonflyDB (dev) are used: `EVAL`/`EVALSHA`, `ZADD`, `ZPOPMIN`,
//! `ZRANGEBYSCORE`, `ZREMRANGEBYSCORE`, `ZCARD`, `SADD`, `SREM`,
//! `SMEMBERS`, `HSET`, `HMGET`, `HGET`, `HINCRBY`, `SET NX PX`, `GET`,
//! `PEXPIRE`, `EXISTS`, `DEL`. Every key a script touches is passed in
//! `KEYS` (Dragonfly rejects undeclared key access by default). No Redis
//! Functions, no `ZMPOP`, no `cjson`.
//!
//! ## Key layout (`p` = the store's key prefix, `id` = job id)
//!
//! | Key | Type | Contents |
//! |-----|------|----------|
//! | `{p}:job:{id}:meta` | hash | `state`, `max_pages`, `max_depth` (`""` = unlimited), counters `received`/`admitted`/`dispatched`/`rejected`/`dropped`, and the per-job sequence counter `seq` |
//! | `{p}:job:{id}:tpl` | string | job template JSON |
//! | `{p}:job:{id}:seen` | set | 16-hex-char SipHash-1-3 (zero keys) of each admitted URL — the same hash `MemoryFrontierStore` uses |
//! | `{p}:job:{id}:q` | zset | ready URLs (see encoding below) |
//! | `{p}:job:{id}:later` | zset | delayed URLs, score = `not_before_ms` |
//! | `{p}:jobs` | set | ids of jobs that exist and are not released |
//! | `{p}:lease:{id}` | string | dispatch lease owner, with a `PX` TTL |
//!
//! A job "exists" iff its `meta` hash exists; that is how the scripts detect
//! the unknown-job case documented in [`super`] (the `admit`/`set_state`
//! scripts return a sentinel which Rust maps to the same
//! `ScrapixError::NotFound` the memory store returns).
//!
//! ## Queue encoding (priority desc, then FIFO)
//!
//! A double-precision score has only 53 bits of mantissa, so packing
//! `priority` (i32) and an ever-growing `seq` (u64) into one score cannot be
//! made collision-free. Instead:
//!
//! - `q` score = `-(priority)` (exact in a double), so `ZPOPMIN` yields the
//!   highest priority first;
//! - `q` member = `{seq:020}|{CrawlUrl JSON}`. Members with equal scores
//!   are ordered lexicographically by Redis, and a zero-padded 20-digit
//!   `seq` (u64 max is 20 digits) sorts lexicographically == numerically,
//!   so ties pop FIFO. The unique `seq` also keeps two identical URLs from
//!   collapsing into one member. The prefix is stripped on pop.
//! - `later` score = `not_before_ms`; member = `{seq:020}|{-(priority)}|{JSON}`
//!   so promotion into `q` can recover the priority without decoding JSON,
//!   and equal `not_before_ms` ties stay in `seq` order.
//!
//! Promotion (in `pop_ready`) gives each promoted URL a fresh `seq`, and
//! `requeue` gives every URL a fresh `seq`, exactly as the memory store does.

use std::time::Duration;

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Script};
use scrapix_core::{CrawlUrl, Result, ScrapixError};

use super::memory::{hash_url, not_found};
use super::{Admission, FrontierStore, JobCounters, JobRunState};

/// `KEYS`: meta, tpl, jobs. `ARGV`: template, max_pages, max_depth, job_id.
/// First writer wins: a no-op if the job already exists (including during a
/// released job's retention window, like the memory store).
const ENSURE_JOB_LUA: &str = r#"
if redis.call('EXISTS', KEYS[1]) == 1 then return 0 end
redis.call('HSET', KEYS[1],
  'state', 'Paused', 'max_pages', ARGV[2], 'max_depth', ARGV[3],
  'received', 0, 'admitted', 0, 'dispatched', 0, 'rejected', 0,
  'dropped', 0, 'seq', 0)
redis.call('SET', KEYS[2], ARGV[1])
redis.call('SADD', KEYS[3], ARGV[4])
return 1
"#;

/// `KEYS`: meta, seen, q, later.
/// `ARGV`: url hash, depth, is_retry ("1"/"0"), queue_cap, url JSON,
/// -(priority), not_before_ms ("" = none).
///
/// Returns -1 for an unknown job, else an outcome code (see
/// [`admission_from_code`]). Decision order is exactly the module docs'
/// order: state -> depth -> retry bypass -> budget -> capacity -> dedup ->
/// enqueue. `received` always increments (for an existing job).
const ADMIT_LUA: &str = r#"
if redis.call('EXISTS', KEYS[1]) == 0 then return -1 end
redis.call('HINCRBY', KEYS[1], 'received', 1)
local m = redis.call('HMGET', KEYS[1], 'state', 'max_depth', 'max_pages', 'admitted')
local is_retry = ARGV[3] == '1'
local outcome = 0
if m[1] ~= 'Running' and m[1] ~= 'Paused' then
  outcome = 5
elseif m[2] and m[2] ~= '' and tonumber(ARGV[2]) > tonumber(m[2]) then
  outcome = 2
elseif (not is_retry) and m[3] and m[3] ~= '' and tonumber(m[4] or '0') >= tonumber(m[3]) then
  outcome = 3
elseif redis.call('ZCARD', KEYS[3]) + redis.call('ZCARD', KEYS[4]) >= tonumber(ARGV[4]) then
  outcome = 4
elseif (not is_retry) and redis.call('SADD', KEYS[2], ARGV[1]) == 0 then
  outcome = 1
end
if outcome ~= 0 then
  redis.call('HINCRBY', KEYS[1], 'rejected', 1)
  return outcome
end
if not is_retry then redis.call('HINCRBY', KEYS[1], 'admitted', 1) end
local seq = string.format('%020d', redis.call('HINCRBY', KEYS[1], 'seq', 1))
if ARGV[7] == '' then
  redis.call('ZADD', KEYS[3], ARGV[6], seq .. '|' .. ARGV[5])
else
  redis.call('ZADD', KEYS[4], ARGV[7], seq .. '|' .. ARGV[6] .. '|' .. ARGV[5])
end
return 0
"#;

/// `KEYS`: meta, q, later. `ARGV`: n (clamped to i64::MAX by Rust), now_ms.
///
/// Phase 1 promotes every `later` member with `not_before_ms <= now_ms` into
/// `q` with a fresh seq (O(k log n), k = promoted); phase 2 `ZPOPMIN`s up to
/// n members from `q` (which holds only ready URLs, so nothing is skipped),
/// strips the seq prefix, and adds the count to `dispatched`. An unknown job
/// returns an empty array.
const POP_READY_LUA: &str = r#"
if redis.call('EXISTS', KEYS[1]) == 0 then return {} end
local due = redis.call('ZRANGEBYSCORE', KEYS[3], '-inf', ARGV[2])
if #due > 0 then
  for _, member in ipairs(due) do
    local sep = string.find(member, '|', 22, true)
    local negprio = string.sub(member, 22, sep - 1)
    local json = string.sub(member, sep + 1)
    local seq = string.format('%020d', redis.call('HINCRBY', KEYS[1], 'seq', 1))
    redis.call('ZADD', KEYS[2], negprio, seq .. '|' .. json)
  end
  redis.call('ZREMRANGEBYSCORE', KEYS[3], '-inf', ARGV[2])
end
local out = {}
if ARGV[1] ~= '0' then
  local popped = redis.call('ZPOPMIN', KEYS[2], ARGV[1])
  for i = 1, #popped, 2 do
    out[#out + 1] = string.sub(popped[i], 22)
  end
  if #out > 0 then redis.call('HINCRBY', KEYS[1], 'dispatched', #out) end
end
return out
"#;

/// `KEYS`: meta, q, later, tpl. `ARGV`: repeated triples
/// (-(priority), not_before_ms or "", url JSON). Undoes the pop: `dispatched`
/// is decremented (floored at 0) by the number of URLs; every other counter is
/// untouched. Each URL gets a fresh seq. A no-op for an unknown job; for a
/// released job (no `tpl`) the URLs are counted in `dropped` instead.
const REQUEUE_LUA: &str = r#"
if redis.call('EXISTS', KEYS[1]) == 0 then return 0 end
local dispatched = tonumber(redis.call('HGET', KEYS[1], 'dispatched') or '0')
local n = #ARGV / 3
if dispatched < n then n = dispatched end
if n > 0 then redis.call('HINCRBY', KEYS[1], 'dispatched', -n) end
if redis.call('EXISTS', KEYS[4]) == 0 then
  -- Released job: the URLs are dropped, never queued again.
  redis.call('HINCRBY', KEYS[1], 'dropped', #ARGV / 3)
  return 1
end
for i = 1, #ARGV, 3 do
  local seq = string.format('%020d', redis.call('HINCRBY', KEYS[1], 'seq', 1))
  if ARGV[i + 1] == '' then
    redis.call('ZADD', KEYS[2], ARGV[i], seq .. '|' .. ARGV[i + 2])
  else
    redis.call('ZADD', KEYS[3], ARGV[i + 1], seq .. '|' .. ARGV[i] .. '|' .. ARGV[i + 2])
  end
end
return 1
"#;

/// `KEYS`: meta. `ARGV`: state. Returns 0 for an unknown job (Rust maps it
/// to `NotFound`), 1 otherwise. A script so an expired `meta` is never
/// resurrected as a partial hash.
const SET_STATE_LUA: &str = r#"
if redis.call('EXISTS', KEYS[1]) == 0 then return 0 end
redis.call('HSET', KEYS[1], 'state', ARGV[1])
return 1
"#;

/// `KEYS`: meta, q, later, seen, tpl, jobs. `ARGV`: retention_ms, job_id.
/// `dropped += pending` before deleting the queue, seen set and template;
/// the job leaves `jobs` immediately; `meta` (counters/state) is kept for
/// the retention window (retention 0 deletes it now). No-op if unknown.
const RELEASE_LUA: &str = r#"
if redis.call('EXISTS', KEYS[1]) == 0 then return 0 end
local pending = redis.call('ZCARD', KEYS[2]) + redis.call('ZCARD', KEYS[3])
redis.call('HINCRBY', KEYS[1], 'dropped', pending)
redis.call('DEL', KEYS[2], KEYS[3], KEYS[4], KEYS[5])
redis.call('SREM', KEYS[6], ARGV[2])
if ARGV[1] == '0' then
  redis.call('DEL', KEYS[1])
else
  redis.call('PEXPIRE', KEYS[1], ARGV[1])
end
return 1
"#;

/// `KEYS`: lease. `ARGV`: owner, ttl_ms. Acquire if free (`SET NX PX`), or
/// renew (compare-and-`PEXPIRE`) if already held by `owner`.
const TRY_LEASE_LUA: &str = r#"
if redis.call('SET', KEYS[1], ARGV[1], 'NX', 'PX', ARGV[2]) then return 1 end
if redis.call('GET', KEYS[1]) == ARGV[1] then
  redis.call('PEXPIRE', KEYS[1], ARGV[2])
  return 1
end
return 0
"#;

fn storage_err(err: redis::RedisError) -> ScrapixError {
    ScrapixError::Storage(format!("redis frontier store: {err}"))
}

fn admission_from_code(code: i64) -> Result<Admission> {
    Ok(match code {
        0 => Admission::Admitted,
        1 => Admission::Duplicate,
        2 => Admission::OverDepth,
        3 => Admission::OverMaxPages,
        4 => Admission::QueueFull,
        5 => Admission::JobNotRunning,
        other => {
            return Err(ScrapixError::Storage(format!(
                "redis frontier store: unexpected admit outcome {other}"
            )))
        }
    })
}

fn state_to_str(state: JobRunState) -> &'static str {
    match state {
        JobRunState::Running => "Running",
        JobRunState::Paused => "Paused",
        JobRunState::Cancelled => "Cancelled",
        JobRunState::Finished => "Finished",
    }
}

fn state_from_str(s: &str) -> Result<JobRunState> {
    match s {
        "Running" => Ok(JobRunState::Running),
        "Paused" => Ok(JobRunState::Paused),
        "Cancelled" => Ok(JobRunState::Cancelled),
        "Finished" => Ok(JobRunState::Finished),
        other => Err(ScrapixError::Storage(format!(
            "redis frontier store: unknown job state `{other}`"
        ))),
    }
}

/// Duration -> whole milliseconds, rounding a non-zero sub-millisecond
/// duration up to 1 so it is not mistaken for "zero".
fn duration_ms(d: Duration) -> u128 {
    let ms = d.as_millis();
    if ms == 0 && !d.is_zero() {
        1
    } else {
        ms
    }
}

/// Redis/DragonflyDB [`FrontierStore`]. Cheap to share: the underlying
/// [`ConnectionManager`] multiplexes one auto-reconnecting connection and is
/// cloned per call.
pub struct RedisFrontierStore {
    conn: ConnectionManager,
    prefix: String,
    ensure_job: Script,
    admit: Script,
    pop_ready: Script,
    requeue: Script,
    set_state: Script,
    release: Script,
    try_lease: Script,
}

impl RedisFrontierStore {
    /// Connect to `url` (e.g. `redis://localhost:6379`). All keys are
    /// namespaced under `key_prefix` (see the module docs for the layout).
    pub async fn new(url: &str, key_prefix: &str) -> Result<Self> {
        let client = redis::Client::open(url).map_err(storage_err)?;
        let conn = ConnectionManager::new(client).await.map_err(storage_err)?;
        Ok(Self {
            conn,
            prefix: key_prefix.to_string(),
            ensure_job: Script::new(ENSURE_JOB_LUA),
            admit: Script::new(ADMIT_LUA),
            pop_ready: Script::new(POP_READY_LUA),
            requeue: Script::new(REQUEUE_LUA),
            set_state: Script::new(SET_STATE_LUA),
            release: Script::new(RELEASE_LUA),
            try_lease: Script::new(TRY_LEASE_LUA),
        })
    }

    fn job_key(&self, job_id: &str, suffix: &str) -> String {
        format!("{}:job:{}:{}", self.prefix, job_id, suffix)
    }

    fn jobs_key(&self) -> String {
        format!("{}:jobs", self.prefix)
    }

    fn lease_key(&self, job_id: &str) -> String {
        format!("{}:lease:{}", self.prefix, job_id)
    }
}

/// The per-URL arguments every enqueue needs: `-(priority)` as the `q` score,
/// `not_before_ms` (or `""`) to route into `later`, and the JSON member body.
fn enqueue_args(url: &CrawlUrl) -> Result<(String, String, String)> {
    let neg_priority = (-i64::from(url.priority)).to_string();
    let not_before = url
        .not_before_ms
        .map(|ms| ms.to_string())
        .unwrap_or_default();
    let json = serde_json::to_string(url)?;
    Ok((neg_priority, not_before, json))
}

#[async_trait]
impl FrontierStore for RedisFrontierStore {
    async fn ensure_job(
        &self,
        job_id: &str,
        template_json: &str,
        max_pages: Option<u64>,
        max_depth: Option<u32>,
    ) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: i64 = self
            .ensure_job
            .key(self.job_key(job_id, "meta"))
            .key(self.job_key(job_id, "tpl"))
            .key(self.jobs_key())
            .arg(template_json)
            .arg(max_pages.map(|v| v.to_string()).unwrap_or_default())
            .arg(max_depth.map(|v| v.to_string()).unwrap_or_default())
            .arg(job_id)
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn job_template(&self, job_id: &str) -> Result<Option<String>> {
        let mut conn = self.conn.clone();
        conn.get(self.job_key(job_id, "tpl"))
            .await
            .map_err(storage_err)
    }

    async fn admit(&self, job_id: &str, url: &CrawlUrl, queue_cap: usize) -> Result<Admission> {
        let (neg_priority, not_before, json) = enqueue_args(url)?;
        let hash = format!("{:016x}", hash_url(&url.url));
        let is_retry = if url.retry_count > 0 { "1" } else { "0" };
        let mut conn = self.conn.clone();
        let code: i64 = self
            .admit
            .key(self.job_key(job_id, "meta"))
            .key(self.job_key(job_id, "seen"))
            .key(self.job_key(job_id, "q"))
            .key(self.job_key(job_id, "later"))
            .arg(hash)
            .arg(url.depth)
            .arg(is_retry)
            .arg(queue_cap.to_string())
            .arg(json)
            .arg(neg_priority)
            .arg(not_before)
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        if code == -1 {
            return Err(not_found(job_id));
        }
        admission_from_code(code)
    }

    async fn pop_ready(&self, job_id: &str, n: usize, now_ms: i64) -> Result<Vec<CrawlUrl>> {
        // ZPOPMIN's count must fit a signed 64-bit integer, and DragonflyDB
        // may parse it as a u32 — clamp to the smaller of the two.
        let n = n.min(u32::MAX as usize);
        let mut conn = self.conn.clone();
        let members: Vec<String> = self
            .pop_ready
            .key(self.job_key(job_id, "meta"))
            .key(self.job_key(job_id, "q"))
            .key(self.job_key(job_id, "later"))
            .arg(n.to_string())
            .arg(now_ms)
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        members
            .iter()
            .map(|json| serde_json::from_str(json).map_err(ScrapixError::from))
            .collect()
    }

    async fn requeue(&self, job_id: &str, urls: Vec<CrawlUrl>) -> Result<()> {
        if urls.is_empty() {
            return Ok(());
        }
        let mut invocation = self.requeue.prepare_invoke();
        invocation
            .key(self.job_key(job_id, "meta"))
            .key(self.job_key(job_id, "q"))
            .key(self.job_key(job_id, "later"))
            .key(self.job_key(job_id, "tpl"));
        for url in &urls {
            let (neg_priority, not_before, json) = enqueue_args(url)?;
            invocation.arg(neg_priority).arg(not_before).arg(json);
        }
        let mut conn = self.conn.clone();
        let _: i64 = invocation
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn queued(&self, job_id: &str) -> Result<u64> {
        let mut conn = self.conn.clone();
        let (ready, delayed): (u64, u64) = redis::pipe()
            .atomic()
            .zcard(self.job_key(job_id, "q"))
            .zcard(self.job_key(job_id, "later"))
            .query_async(&mut conn)
            .await
            .map_err(storage_err)?;
        Ok(ready + delayed)
    }

    async fn counters(&self, job_id: &str) -> Result<JobCounters> {
        let mut conn = self.conn.clone();
        let values: Vec<Option<u64>> = redis::cmd("HMGET")
            .arg(self.job_key(job_id, "meta"))
            .arg(&["received", "admitted", "dispatched", "rejected", "dropped"])
            .query_async(&mut conn)
            .await
            .map_err(storage_err)?;
        let get = |i: usize| values.get(i).copied().flatten().unwrap_or(0);
        Ok(JobCounters {
            received: get(0),
            admitted: get(1),
            dispatched: get(2),
            rejected: get(3),
            dropped: get(4),
        })
    }

    async fn set_state(&self, job_id: &str, state: JobRunState) -> Result<()> {
        let mut conn = self.conn.clone();
        let found: i64 = self
            .set_state
            .key(self.job_key(job_id, "meta"))
            .arg(state_to_str(state))
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        if found == 0 {
            return Err(not_found(job_id));
        }
        Ok(())
    }

    async fn state(&self, job_id: &str) -> Result<Option<JobRunState>> {
        let mut conn = self.conn.clone();
        let state: Option<String> = conn
            .hget(self.job_key(job_id, "meta"), "state")
            .await
            .map_err(storage_err)?;
        state.as_deref().map(state_from_str).transpose()
    }

    async fn active_jobs(&self) -> Result<Vec<String>> {
        let mut conn = self.conn.clone();
        conn.smembers(self.jobs_key()).await.map_err(storage_err)
    }

    async fn release(&self, job_id: &str, retention: Duration) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: i64 = self
            .release
            .key(self.job_key(job_id, "meta"))
            .key(self.job_key(job_id, "q"))
            .key(self.job_key(job_id, "later"))
            .key(self.job_key(job_id, "seen"))
            .key(self.job_key(job_id, "tpl"))
            .key(self.jobs_key())
            .arg(duration_ms(retention).to_string())
            .arg(job_id)
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn try_lease(&self, job_id: &str, owner: &str, ttl: Duration) -> Result<bool> {
        // `PX 0` is invalid; a zero TTL means "expires immediately" in the
        // memory store, the closest Redis equivalent is 1 ms.
        let ttl_ms = duration_ms(ttl).max(1);
        let mut conn = self.conn.clone();
        let granted: i64 = self
            .try_lease
            .key(self.lease_key(job_id))
            .arg(owner)
            .arg(ttl_ms.to_string())
            .invoke_async(&mut conn)
            .await
            .map_err(storage_err)?;
        Ok(granted == 1)
    }
}
