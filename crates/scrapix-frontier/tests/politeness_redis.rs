//! `RedisPoliteness` behaves like the in-memory `PolitenessScheduler` and
//! shares its state between instances.
//!
//! Needs a live Redis (or DragonflyDB): set `SCRAPIX_TEST_REDIS_URL`, e.g.
//! `SCRAPIX_TEST_REDIS_URL=redis://localhost:6379`. Without it each test
//! prints a skip note and passes. Every test uses its own random key prefix
//! and deletes its keys at the end.

#![cfg(feature = "redis-store")]

use std::time::Duration;

use scrapix_frontier::{
    Acquire, FetchReport, FetchSignal, JobLimits, PolitenessConfig, PolitenessStore,
    RedisPoliteness, SlotRequest,
};

fn redis_url() -> Option<String> {
    match std::env::var("SCRAPIX_TEST_REDIS_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("SCRAPIX_TEST_REDIS_URL not set; skipping Redis politeness test");
            None
        }
    }
}

fn config(concurrent: usize) -> PolitenessConfig {
    PolitenessConfig {
        concurrent_per_domain: concurrent,
        default_delay_ms: 0,
        min_delay_ms: 0,
        ..Default::default()
    }
}

async fn pair(url: &str, prefix: &str, c: PolitenessConfig) -> (RedisPoliteness, RedisPoliteness) {
    (
        RedisPoliteness::new(url, prefix, c.clone()).await.unwrap(),
        RedisPoliteness::new(url, prefix, c).await.unwrap(),
    )
}

async fn cleanup(url: &str, prefix: &str) {
    let client = redis::Client::open(url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{prefix}:*"))
        .query_async(&mut conn)
        .await
        .unwrap();
    if !keys.is_empty() {
        let _: () = redis::cmd("DEL")
            .arg(&keys)
            .query_async(&mut conn)
            .await
            .unwrap();
    }
}

fn req<'a>(domain: &'a str, job: &'a str, token: &'a str, limits: JobLimits) -> SlotRequest<'a> {
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

#[tokio::test]
async fn success_delay_follows_fetch_time_like_the_memory_store() {
    let Some(url) = redis_url() else { return };
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let c = PolitenessConfig {
        concurrent_per_domain: 8,
        default_delay_ms: 250,
        min_delay_ms: 25,
        ..Default::default()
    };
    let (a, _b) = pair(&url, &prefix, c.clone()).await;
    let l = JobLimits::default();
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let key = format!("{prefix}:pol:d:f.test");
    let mut delay = 250;
    for (i, fetch_ms) in [200u64, 200, 8_000].into_iter().enumerate() {
        let token = format!("f{i}");
        let _: () = redis::cmd("HDEL")
            .arg(&key)
            .arg("last_ms")
            .query_async(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            a.try_acquire(&req("f.test", "j", &token, l)).await.unwrap(),
            Acquire::Granted
        );
        a.report(&FetchReport {
            fetch_ms: Some(fetch_ms),
            ..report("f.test", "j", &token, FetchSignal::Success)
        })
        .await
        .unwrap();
        delay = scrapix_frontier::adapt_on_success(&c, delay, Some(fetch_ms));
        let stored: u64 = redis::cmd("HGET")
            .arg(&key)
            .arg("delay_ms")
            .query_async(&mut conn)
            .await
            .unwrap();
        assert_eq!(stored, delay, "after a {fetch_ms} ms fetch");
    }
    // 250 → 225 → 202 (at most ×0.9 down) → 601 (an 8 s fetch: target 1 s)
    assert_eq!(delay, 601);

    cleanup(&url, &prefix).await;
}

/// Same as `grant_reports_the_effective_delay` on the memory scheduler.
#[tokio::test]
async fn grant_reports_the_effective_delay_like_the_memory_store() {
    let Some(url) = redis_url() else { return };
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let c = PolitenessConfig {
        concurrent_per_domain: 8,
        default_delay_ms: 250,
        min_delay_ms: 25,
        ..Default::default()
    };
    let (a, b) = pair(&url, &prefix, c).await;
    let l = JobLimits::default();
    assert_eq!(
        a.try_acquire_spaced(&req("a.test", "j", "t1", l))
            .await
            .unwrap(),
        (Acquire::Granted, Duration::from_millis(250))
    );
    // Free slots do not help: the delay has not elapsed.
    let (again, spacing) = b
        .try_acquire_spaced(&req("a.test", "j", "t2", l))
        .await
        .unwrap();
    assert!(matches!(again, Acquire::Wait(_)), "{again:?}");
    assert_eq!(spacing, Duration::ZERO);
    // A job delay above the adaptive one is the one reported.
    let slow = JobLimits {
        min_delay_ms: 2_000,
        ..JobLimits::default()
    };
    assert_eq!(
        a.try_acquire_spaced(&req("b.test", "j", "t3", slow))
            .await
            .unwrap(),
        (Acquire::Granted, Duration::from_millis(2_000))
    );
    cleanup(&url, &prefix).await;

    // Zero delay: the domain can be granted again at once.
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let (a, b) = pair(&url, &prefix, config(2)).await;
    assert_eq!(
        a.try_acquire_spaced(&req("a.test", "j", "t1", l))
            .await
            .unwrap(),
        (Acquire::Granted, Duration::ZERO)
    );
    assert_eq!(
        b.try_acquire_spaced(&req("a.test", "j", "t2", l))
            .await
            .unwrap(),
        (Acquire::Granted, Duration::ZERO)
    );
    assert_eq!(
        a.try_acquire_spaced(&req("a.test", "j", "t3", l))
            .await
            .unwrap(),
        (Acquire::DomainBusy, Duration::ZERO)
    );
    cleanup(&url, &prefix).await;
}

#[tokio::test]
async fn domain_slot_is_shared_and_released_by_feedback() {
    let Some(url) = redis_url() else { return };
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let (a, b) = pair(&url, &prefix, config(1)).await;
    let l = JobLimits::default();

    assert_eq!(
        a.try_acquire(&req("a.test", "j", "t1", l)).await.unwrap(),
        Acquire::Granted
    );
    // The other instance sees the slot taken.
    assert_eq!(
        b.try_acquire(&req("a.test", "j", "t2", l)).await.unwrap(),
        Acquire::DomainBusy
    );
    // Feedback processed by either instance frees it.
    b.report(&report("a.test", "j", "t1", FetchSignal::Success))
        .await
        .unwrap();
    assert_eq!(
        b.try_acquire(&req("a.test", "j", "t2", l)).await.unwrap(),
        Acquire::Granted
    );
    b.release("a.test", "j", "t2").await.unwrap();
    assert_eq!(
        a.try_acquire(&req("a.test", "j", "t3", l)).await.unwrap(),
        Acquire::Granted
    );

    cleanup(&url, &prefix).await;
}

#[tokio::test]
async fn delay_job_cap_retry_after_and_expiry() {
    let Some(url) = redis_url() else { return };
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let c = PolitenessConfig {
        slot_ttl: Duration::from_millis(300),
        ..config(10)
    };
    let (a, b) = pair(&url, &prefix, c).await;

    // Job rps 0.5 → 2 s between requests to one domain, across instances.
    let slow = JobLimits {
        max_rps: Some(0.5),
        ..JobLimits::default()
    };
    assert_eq!(
        a.try_acquire(&req("d.test", "j", "t1", slow))
            .await
            .unwrap(),
        Acquire::Granted
    );
    match b
        .try_acquire(&req("d.test", "j", "t2", slow))
        .await
        .unwrap()
    {
        Acquire::Wait(d) => assert!(d > Duration::from_millis(1_500), "{d:?}"),
        other => panic!("expected Wait, got {other:?}"),
    }

    // Per-job cap across domains.
    let capped = JobLimits {
        max_in_flight: Some(2),
        ..JobLimits::default()
    };
    assert_eq!(
        a.try_acquire(&req("e1.test", "k", "k1", capped))
            .await
            .unwrap(),
        Acquire::Granted
    );
    assert_eq!(
        b.try_acquire(&req("e2.test", "k", "k2", capped))
            .await
            .unwrap(),
        Acquire::Granted
    );
    assert_eq!(
        a.try_acquire(&req("e3.test", "k", "k3", capped))
            .await
            .unwrap(),
        Acquire::JobBusy
    );
    // Lost feedback: the slots expire after slot_ttl.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        a.try_acquire(&req("e3.test", "k", "k3", capped))
            .await
            .unwrap(),
        Acquire::Granted
    );

    // 429 with Retry-After pauses the domain for every instance; robots
    // Crawl-delay is recorded.
    let l = JobLimits::default();
    assert_eq!(
        a.try_acquire(&req("r.test", "j", "r1", l)).await.unwrap(),
        Acquire::Granted
    );
    b.report(&FetchReport {
        retry_until_ms: Some(chrono::Utc::now().timestamp_millis() + 60_000),
        crawl_delay_ms: Some(5_000),
        ..report("r.test", "j", "r1", FetchSignal::RateLimited)
    })
    .await
    .unwrap();
    match a.try_acquire(&req("r.test", "j", "r2", l)).await.unwrap() {
        Acquire::Wait(d) => assert!(d > Duration::from_secs(50), "{d:?}"),
        other => panic!("expected Wait, got {other:?}"),
    }

    // robots Crawl-delay applies (only) to jobs respecting robots.txt.
    assert_eq!(
        a.try_acquire(&req("c.test", "j", "c1", l)).await.unwrap(),
        Acquire::Granted
    );
    a.report(&FetchReport {
        crawl_delay_ms: Some(5_000),
        ..report("c.test", "j", "c1", FetchSignal::Success)
    })
    .await
    .unwrap();
    match b.try_acquire(&req("c.test", "j", "c2", l)).await.unwrap() {
        Acquire::Wait(d) => assert!(d > Duration::from_millis(4_000), "{d:?}"),
        other => panic!("expected Wait, got {other:?}"),
    }
    let ignore = JobLimits {
        respect_robots: false,
        ..l
    };
    assert_eq!(
        b.try_acquire(&req("c.test", "j", "c3", ignore))
            .await
            .unwrap(),
        Acquire::Granted
    );

    cleanup(&url, &prefix).await;
}

#[tokio::test]
async fn default_crawl_delay_needs_checked_robots_and_accounting_needs_the_slot() {
    let Some(url) = redis_url() else { return };
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let (a, b) = pair(&url, &prefix, config(10)).await;
    let l = JobLimits {
        default_crawl_delay_ms: 1_500,
        ..JobLimits::default()
    };
    let wait = |r: Acquire| match r {
        Acquire::Wait(d) => d,
        Acquire::Granted => Duration::ZERO,
        other => panic!("unexpected {other:?}"),
    };
    let free = JobLimits::default();

    // robots.txt never fetched: no default delay.
    assert_eq!(
        a.try_acquire(&req("u.test", "j", "u1", l)).await.unwrap(),
        Acquire::Granted
    );
    a.report(&report("u.test", "j", "u1", FetchSignal::Success))
        .await
        .unwrap();
    assert_eq!(
        b.try_acquire(&req("u.test", "j", "u2", l)).await.unwrap(),
        Acquire::Granted
    );

    // Fetched without Crawl-delay: the job's default applies...
    assert_eq!(
        a.try_acquire(&req("k.test", "j", "k1", l)).await.unwrap(),
        Acquire::Granted
    );
    a.report(&FetchReport {
        robots_checked: true,
        ..report("k.test", "j", "k1", FetchSignal::Success)
    })
    .await
    .unwrap();
    let d = wait(b.try_acquire(&req("k.test", "j", "k2", l)).await.unwrap());
    assert!(d > Duration::from_millis(1_400), "{d:?}");
    // ...unless the job set an explicit delay.
    let explicit = JobLimits {
        min_delay_ms: 200,
        ..l
    };
    let d = wait(
        b.try_acquire(&req("k.test", "j", "k3", explicit))
            .await
            .unwrap(),
    );
    assert!(d <= Duration::from_millis(200), "{d:?}");

    // Feedback for a slot that is not held (duplicate) does no accounting:
    // a dozen rate-limited duplicates would otherwise pause the domain.
    assert_eq!(
        a.try_acquire(&req("dup.test", "j", "x1", free))
            .await
            .unwrap(),
        Acquire::Granted
    );
    a.report(&report("dup.test", "j", "x1", FetchSignal::Success))
        .await
        .unwrap();
    for _ in 0..12 {
        a.report(&report("dup.test", "j", "x1", FetchSignal::RateLimited))
            .await
            .unwrap();
    }
    assert_eq!(
        a.try_acquire(&req("dup.test", "j", "x2", free))
            .await
            .unwrap(),
        Acquire::Granted
    );

    // A Retry-After deadline already in the past does not pause.
    assert_eq!(
        a.try_acquire(&req("old.test", "j", "o1", free))
            .await
            .unwrap(),
        Acquire::Granted
    );
    a.report(&FetchReport {
        retry_until_ms: Some(chrono::Utc::now().timestamp_millis() - 1_000),
        ..report("old.test", "j", "o1", FetchSignal::RateLimited)
    })
    .await
    .unwrap();
    assert_eq!(
        a.try_acquire(&req("old.test", "j", "o2", free))
            .await
            .unwrap(),
        Acquire::Granted
    );

    assert!(a.tracked_domain_count() >= 4);
    cleanup(&url, &prefix).await;
}

// ---- Spacing credit (SCR-90): Redis and memory apply the same rule ----

use scrapix_frontier::{Grants, PolitenessScheduler, SlotsRequest};

/// The same requests sent to a `RedisPoliteness` and to a
/// `PolitenessScheduler`, on one simulated clock (`at_ms` after a common
/// start): every answer must be identical.
struct Parity {
    redis: RedisPoliteness,
    memory: PolitenessScheduler,
    start: std::time::Instant,
    start_ms: i64,
    next_token: usize,
    /// Time (ms) of every grant, in order.
    grants: Vec<u64>,
}

impl Parity {
    async fn new(url: &str, prefix: &str, c: PolitenessConfig) -> Self {
        Self {
            redis: RedisPoliteness::new(url, prefix, c.clone()).await.unwrap(),
            memory: PolitenessScheduler::new(c),
            start: std::time::Instant::now(),
            start_ms: chrono::Utc::now().timestamp_millis(),
            next_token: 0,
            grants: Vec::new(),
        }
    }

    /// Ask both stores for `want` slots on `domain` at `at_ms`.
    async fn pass(&mut self, domain: &str, limits: JobLimits, at_ms: u64, want: usize) -> Grants {
        let tokens: Vec<String> = (0..want)
            .map(|i| format!("t{}", self.next_token + i))
            .collect();
        let req = SlotsRequest {
            domain,
            job_id: "j",
            tokens: &tokens,
            limits,
        };
        let memory = self
            .memory
            .acquire_many_at(&req, self.start + Duration::from_millis(at_ms));
        let redis = self
            .redis
            .acquire_many_at(&req, self.start_ms + at_ms as i64)
            .await
            .unwrap();
        assert_eq!(redis, memory, "{domain} at {at_ms} ms, {want} wanted");
        self.next_token += redis.granted;
        self.grants
            .extend(std::iter::repeat_n(at_ms, redis.granted));
        redis
    }

    async fn report(&mut self, r: FetchReport<'_>, at_ms: u64) {
        let now_ms = self.start_ms + at_ms as i64;
        self.memory
            .apply_report_at(&r, self.start + Duration::from_millis(at_ms), now_ms);
        self.redis.report_at(&r, now_ms).await.unwrap();
    }
}

fn spaced(delay_ms: u64, concurrent: usize) -> PolitenessConfig {
    PolitenessConfig {
        default_delay_ms: delay_ms,
        min_delay_ms: delay_ms,
        concurrent_per_domain: concurrent,
        ..Default::default()
    }
}

/// Over a simulated second, Redis grants exactly what the memory scheduler
/// grants, pass by pass, and that is what the delay allows whatever the
/// pass period.
#[tokio::test]
async fn spacing_credit_matches_the_memory_store_pass_by_pass() {
    let Some(url) = redis_url() else { return };
    let l = JobLimits::default();
    for delay in [10u64, 25, 40, 100] {
        for period in [20u64, 25, 33] {
            let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
            let mut p = Parity::new(&url, &prefix, spaced(delay, 100_000)).await;
            let mut t = 0;
            while t < 1_000 {
                p.pass("a.test", l, t, 50).await;
                t += period;
            }
            let last_pass = (999 / period) * period;
            assert_eq!(
                p.grants.len() as u64,
                1 + last_pass / delay,
                "delay {delay} ms, pass every {period} ms"
            );
            // n + 1 consecutive grants span at least n × delay − credit.
            for (i, a) in p.grants.iter().enumerate() {
                for (n, b) in p.grants[i..].iter().enumerate() {
                    assert!(b - a + 100 >= n as u64 * delay, "delay {delay}: burst");
                }
            }
            cleanup(&url, &prefix).await;
        }
    }
}

/// Burst bound, strict long delays, pauses, backoff and the in-flight caps:
/// the same answers from both stores.
#[tokio::test]
async fn spacing_credit_bounds_match_the_memory_store() {
    let Some(url) = redis_url() else { return };
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let l = JobLimits::default();
    let wait = |ms| Acquire::Wait(Duration::from_millis(ms));

    // Burst after an idle period: 1 + credit / delay, then the delay.
    let mut p = Parity::new(&url, &prefix, spaced(25, 100_000)).await;
    assert_eq!(p.pass("burst.test", l, 0, 50).await.granted, 1);
    assert_eq!(
        p.pass("burst.test", l, 10_000, 50).await,
        Grants {
            granted: 5,
            next: wait(25)
        }
    );
    assert_eq!(p.pass("burst.test", l, 10_010, 50).await.granted, 0);
    assert_eq!(p.pass("burst.test", l, 10_025, 50).await.granted, 1);
    // A pass 60 ms late is owed two; asking for fewer leaves the rest.
    assert_eq!(p.pass("late.test", l, 0, 50).await.granted, 1);
    assert_eq!(
        p.pass("late.test", l, 60, 1).await,
        Grants {
            granted: 1,
            next: Acquire::Granted
        }
    );
    assert_eq!(
        p.pass("late.test", l, 60, 50).await,
        Grants {
            granted: 1,
            next: wait(15)
        }
    );

    // A 2 s delay (job rate, or robots Crawl-delay) is strict: one grant
    // per delay however late the pass, never two at once.
    let slow = JobLimits {
        max_rps: Some(0.5),
        ..l
    };
    let mut at = Vec::new();
    for t in (0..9_000).step_by(330).chain((30_000..39_000).step_by(330)) {
        let got = p.pass("slow.test", slow, t, 50).await;
        assert!(got.granted <= 1, "{got:?}");
        if got.granted == 1 {
            at.push(t);
        }
    }
    assert_eq!(at.len(), 8);
    for w in at.windows(2) {
        assert!(w[1] - w[0] >= 2_000, "{} ms apart", w[1] - w[0]);
    }
    assert_eq!(p.pass("robots.test", l, 0, 50).await.granted, 1);
    let robots_at = p.grants.len() - 1;
    p.report(
        FetchReport {
            crawl_delay_ms: Some(2_000),
            ..report(
                "robots.test",
                "j",
                &format!("t{robots_at}"),
                FetchSignal::Success,
            )
        },
        100,
    )
    .await;
    assert_eq!(
        p.pass("robots.test", l, 1_000, 50).await,
        Grants {
            granted: 0,
            next: wait(1_000)
        }
    );
    assert_eq!(
        p.pass("robots.test", l, 60_000, 50).await,
        Grants {
            granted: 1,
            next: wait(2_000)
        }
    );

    // A Retry-After pause banks no credit: 60 ms after its end, the three
    // requests due since then.
    assert_eq!(p.pass("pause.test", l, 0, 50).await.granted, 1);
    let token = format!("t{}", p.grants.len() - 1);
    let until = p.start_ms + 1_000;
    p.report(
        FetchReport {
            retry_until_ms: Some(until),
            ..report("pause.test", "j", &token, FetchSignal::NoRequest)
        },
        0,
    )
    .await;
    assert_eq!(
        p.pass("pause.test", l, 400, 50).await,
        Grants {
            granted: 0,
            next: wait(600)
        }
    );
    assert_eq!(
        p.pass("pause.test", l, 1_060, 50).await,
        Grants {
            granted: 3,
            next: wait(15)
        }
    );

    // A 429 raises the delay and drops the banked credit.
    assert_eq!(p.pass("rate.test", l, 0, 50).await.granted, 1);
    let token = format!("t{}", p.grants.len() - 1);
    p.report(
        report("rate.test", "j", &token, FetchSignal::RateLimited),
        1_000,
    )
    .await;
    assert_eq!(
        p.pass("rate.test", l, 1_000, 50).await,
        Grants {
            granted: 1,
            next: wait(37)
        }
    );
    assert_eq!(p.pass("rate.test", l, 1_036, 50).await.granted, 0);
    assert_eq!(p.pass("rate.test", l, 1_080, 50).await.granted, 2);
    cleanup(&url, &prefix).await;

    // The domain's free slots cap the burst (3 slots, 1 held)...
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let mut p = Parity::new(&url, &prefix, spaced(10, 3)).await;
    assert_eq!(p.pass("cap.test", l, 0, 50).await.granted, 1);
    assert_eq!(
        p.pass("cap.test", l, 5_000, 50).await,
        Grants {
            granted: 2,
            next: Acquire::DomainBusy
        }
    );
    // ...a freed slot is granted at once while credit is left...
    p.report(report("cap.test", "j", "t1", FetchSignal::Success), 5_040)
        .await;
    assert_eq!(
        p.pass("cap.test", l, 5_040, 50).await,
        Grants {
            granted: 1,
            next: Acquire::DomainBusy
        }
    );
    // ...and so does the job's cap, across domains.
    let capped = JobLimits {
        max_in_flight: Some(5),
        ..l
    };
    assert_eq!(p.pass("job1.test", capped, 5_040, 50).await.granted, 1);
    assert_eq!(
        p.pass("job1.test", capped, 9_000, 50).await,
        Grants {
            granted: 1,
            next: Acquire::JobBusy
        }
    );
    assert_eq!(
        p.pass("job2.test", capped, 9_000, 50).await,
        Grants {
            granted: 0,
            next: Acquire::JobBusy
        }
    );
    cleanup(&url, &prefix).await;
}

/// The script every instance ran before SCR-90 (one slot per call, spacing
/// from `last_ms = now`), for the mixed-version test.
const OLD_ACQUIRE_LUA: &str = r#"
local now = tonumber(ARGV[1])
redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now)
redis.call('ZREMRANGEBYSCORE', KEYS[3], '-inf', now)
local h = redis.call('HMGET', KEYS[1], 'last_ms', 'delay_ms', 'robots_ms', 'pause_until', 'robots_checked')
local pause = tonumber(h[4]) or 0
if pause > now then return {1, pause - now} end
local jobcap = tonumber(ARGV[3])
if jobcap > 0 and redis.call('ZCARD', KEYS[3]) >= jobcap then return {-2, 0} end
if redis.call('ZCARD', KEYS[2]) >= tonumber(ARGV[2]) then return {-1, 0} end
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
if last and now - last < eff then return {1, eff - (now - last)} end
local ttl = tonumber(ARGV[13])
redis.call('ZADD', KEYS[2], now + ttl, ARGV[12])
redis.call('ZADD', KEYS[3], now + ttl, ARGV[12])
redis.call('HSET', KEYS[1], 'last_ms', now)
redis.call('PEXPIRE', KEYS[1], ARGV[14])
redis.call('PEXPIRE', KEYS[2], 2 * ttl)
redis.call('PEXPIRE', KEYS[3], 2 * ttl)
return {0, eff}
"#;

/// Rolling upgrade: an instance running the previous script shares a
/// domain's keys with one running the new script. Each honours the other's
/// grants, and together they never exceed one request per delay (plus the
/// new script's credit).
#[tokio::test]
async fn old_and_new_scripts_share_a_domain() {
    let Some(url) = redis_url() else { return };
    let prefix = format!("test-pol-{}", uuid::Uuid::new_v4());
    let c = spaced(25, 100_000);
    let new = RedisPoliteness::new(&url, &prefix, c.clone())
        .await
        .unwrap();
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let old_script = redis::Script::new(OLD_ACQUIRE_LUA);
    let start_ms = chrono::Utc::now().timestamp_millis();
    let l = JobLimits::default();
    let mut n = 0;
    // The old instance asks for one slot at `at_ms`: `(status, ms)`.
    let mut old = |at_ms: i64| {
        n += 1;
        let mut call = old_script.prepare_invoke();
        call.key(format!("{prefix}:pol:d:mix.test"))
            .key(format!("{prefix}:pol:f:mix.test"))
            .key(format!("{prefix}:pol:j:j"))
            .arg(start_ms + at_ms)
            .arg(c.concurrent_per_domain)
            .arg(0)
            .arg(c.default_delay_ms)
            .arg(c.min_delay_ms)
            .arg(c.max_delay_ms)
            .arg(0)
            .arg(0)
            .arg("1")
            .arg(0)
            .arg(c.robots_delay_multiplier)
            .arg(format!("old{n}"))
            .arg(c.slot_ttl.as_millis() as u64)
            .arg(86_400_000u64);
        let mut conn = conn.clone();
        async move {
            let got: (i64, i64) = call.invoke_async(&mut conn).await.unwrap();
            got
        }
    };
    let mut m = 0;
    let mut new_pass = |at_ms: i64, want: usize| {
        let tokens: Vec<String> = (0..want).map(|i| format!("new{}", m + i)).collect();
        m += want;
        let new = &new;
        async move {
            let req = SlotsRequest {
                domain: "mix.test",
                job_id: "j",
                tokens: &tokens,
                limits: l,
            };
            new.acquire_many_at(&req, start_ms + at_ms).await.unwrap()
        }
    };
    let wait = |ms| Acquire::Wait(Duration::from_millis(ms));

    // A hash written by the old script only (`last_ms`): the new script
    // spaces from it, and banks credit against it.
    assert_eq!(old(0).await, (0, 25));
    assert_eq!(
        new_pass(10, 5).await,
        Grants {
            granted: 0,
            next: wait(15)
        }
    );
    assert_eq!(
        new_pass(25, 5).await,
        Grants {
            granted: 1,
            next: wait(25)
        }
    );
    // The old script honours the new one's grant.
    assert_eq!(old(40).await, (1, 10));
    assert_eq!(old(50).await, (0, 25));
    assert_eq!(new_pass(60, 5).await.granted, 0);

    // After an idle second the new script uses its credit (5 grants at
    // 1050: the schedule is then at 1050)...
    assert_eq!(
        new_pass(1_050, 50).await,
        Grants {
            granted: 5,
            next: wait(25)
        }
    );
    // ...which the old script waits out like any delay...
    assert_eq!(old(1_060).await, (1, 15));
    assert_eq!(old(1_075).await, (0, 25));
    // ...and an old grant leaves no credit behind for the new script.
    assert_eq!(
        new_pass(1_090, 50).await,
        Grants {
            granted: 0,
            next: wait(10)
        }
    );
    // A new grant that came late (schedule at 1100, granted at 1140) lets
    // the old script grant up to the credit early, as the new one would:
    // 1125 is the next slot of the schedule.
    assert_eq!(
        new_pass(1_140, 1).await,
        Grants {
            granted: 1,
            next: Acquire::Granted
        }
    );
    assert_eq!(old(1_141).await, (0, 25));
    assert_eq!(new_pass(1_150, 50).await.granted, 0);

    let last: i64 = redis::cmd("HGET")
        .arg(format!("{prefix}:pol:d:mix.test"))
        .arg("last_ms")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(last - start_ms, 1_141, "the old script wrote its own clock");
    cleanup(&url, &prefix).await;
}
