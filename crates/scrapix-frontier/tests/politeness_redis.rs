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
    }
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
