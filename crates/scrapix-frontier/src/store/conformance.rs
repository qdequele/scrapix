//! Shared behavioral conformance suite for [`super::FrontierStore`]
//! implementations.
//!
//! Every case is a free `pub async fn` taking `&dyn FrontierStore`, plus
//! [`run_all`] which drives all of them against a fresh store each (built by
//! the caller-supplied `make` factory). A future Redis-backed store reuses
//! this same suite via `run_all(|| ...)` to prove it behaves identically to
//! [`super::MemoryFrontierStore`].

use std::future::Future;
use std::sync::Arc;

use scrapix_core::CrawlUrl;

use super::{Admission, FrontierStore, JobRunState};

pub async fn admits_then_dedups(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    let u = CrawlUrl::seed("https://a.test/");
    assert_eq!(s.admit("j", &u, 100).await.unwrap(), Admission::Admitted);
    assert_eq!(s.admit("j", &u, 100).await.unwrap(), Admission::Duplicate);
    let c = s.counters("j").await.unwrap();
    assert_eq!((c.received, c.admitted), (2, 1));
}

pub async fn max_pages_counts_each_url_once(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", Some(2), None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    for i in 0..5 {
        let _ = s
            .admit("j", &CrawlUrl::seed(format!("https://a.test/{i}")), 100)
            .await
            .unwrap();
    }
    let popped = s.pop_ready("j", 10, i64::MAX).await.unwrap();
    assert_eq!(popped.len(), 2);
    s.requeue("j", popped).await.unwrap(); // politeness bounce must not consume budget
    assert_eq!(s.counters("j").await.unwrap().admitted, 2);
    assert_eq!(s.pop_ready("j", 10, i64::MAX).await.unwrap().len(), 2);
}

pub async fn full_queue_rejects_before_marking_seen(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    let a = CrawlUrl::seed("https://a.test/a");
    let b = CrawlUrl::seed("https://a.test/b");
    assert_eq!(s.admit("j", &a, 1).await.unwrap(), Admission::Admitted);
    assert_eq!(s.admit("j", &b, 1).await.unwrap(), Admission::QueueFull);
    s.pop_ready("j", 1, i64::MAX).await.unwrap();
    assert_eq!(
        s.admit("j", &b, 1).await.unwrap(),
        Admission::Admitted,
        "b was not marked seen"
    );
}

pub async fn priority_then_fifo(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    let mut low = CrawlUrl::seed("https://a.test/low");
    low.priority = 1;
    let mut hi = CrawlUrl::seed("https://a.test/hi");
    hi.priority = 9;
    let mut low2 = CrawlUrl::seed("https://a.test/low2");
    low2.priority = 1;
    for u in [&low, &hi, &low2] {
        s.admit("j", u, 100).await.unwrap();
    }
    let order: Vec<String> = s
        .pop_ready("j", 3, i64::MAX)
        .await
        .unwrap()
        .into_iter()
        .map(|u| u.url)
        .collect();
    assert_eq!(
        order,
        vec![
            "https://a.test/hi",
            "https://a.test/low",
            "https://a.test/low2"
        ]
    );
}

pub async fn not_before_is_respected(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    let mut u = CrawlUrl::seed("https://a.test/later");
    u.not_before_ms = Some(1_000);
    s.admit("j", &u, 100).await.unwrap();
    assert!(s.pop_ready("j", 1, 999).await.unwrap().is_empty());
    assert_eq!(s.pop_ready("j", 1, 1_000).await.unwrap().len(), 1);
}

/// A delayed high-priority URL must not block a due lower-priority URL
/// sitting behind it in the queue: `pop_ready` has to skip over the
/// not-yet-ready head rather than stopping at it.
pub async fn delayed_high_priority_does_not_block_due_low_priority(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();

    let mut hi = CrawlUrl::seed("https://a.test/hi");
    hi.priority = 9;
    hi.not_before_ms = Some(5_000);
    let mut lo = CrawlUrl::seed("https://a.test/lo");
    lo.priority = 1;

    s.admit("j", &hi, 100).await.unwrap();
    s.admit("j", &lo, 100).await.unwrap();

    // At t=1_000 the high-priority URL isn't ready yet; the low-priority one
    // behind it is, and must still be popped.
    let popped = s.pop_ready("j", 5, 1_000).await.unwrap();
    assert_eq!(popped.len(), 1);
    assert_eq!(popped[0].url, "https://a.test/lo");

    // The high-priority URL stayed in the queue and becomes poppable once
    // due.
    assert_eq!(s.queued("j").await.unwrap(), 1);
    let popped2 = s.pop_ready("j", 5, 5_000).await.unwrap();
    assert_eq!(popped2.len(), 1);
    assert_eq!(popped2[0].url, "https://a.test/hi");
}

pub async fn retries_bypass_dedup_and_budget(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", Some(1), None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    let u = CrawlUrl::seed("https://a.test/");
    s.admit("j", &u, 100).await.unwrap();
    s.pop_ready("j", 1, i64::MAX).await.unwrap();
    let mut retry = u.clone();
    retry.retry_count = 1;
    assert_eq!(
        s.admit("j", &retry, 100).await.unwrap(),
        Admission::Admitted
    );
}

pub async fn cancelled_job_admits_nothing_and_release_frees(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    s.admit("j", &CrawlUrl::seed("https://a.test/1"), 100)
        .await
        .unwrap();
    s.set_state("j", JobRunState::Cancelled).await.unwrap();
    assert_eq!(
        s.admit("j", &CrawlUrl::seed("https://a.test/2"), 100)
            .await
            .unwrap(),
        Admission::JobNotRunning
    );
    s.release("j", std::time::Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(s.queued("j").await.unwrap(), 0);
}

pub async fn lease_is_exclusive(s: &dyn FrontierStore) {
    let ttl = std::time::Duration::from_secs(5);
    assert!(s.try_lease("j", "a", ttl).await.unwrap());
    assert!(!s.try_lease("j", "b", ttl).await.unwrap());
    assert!(s.try_lease("j", "a", ttl).await.unwrap(), "owner can renew");
}

/// Async-factory form: `make` builds a fresh store for each case,
/// asynchronously. A future Redis-backed store (whose constructor needs to
/// `.await` a connection) uses this directly.
pub async fn run_all_async<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn FrontierStore>>,
{
    admits_then_dedups(&*make().await).await;
    max_pages_counts_each_url_once(&*make().await).await;
    full_queue_rejects_before_marking_seen(&*make().await).await;
    priority_then_fifo(&*make().await).await;
    not_before_is_respected(&*make().await).await;
    delayed_high_priority_does_not_block_due_low_priority(&*make().await).await;
    retries_bypass_dedup_and_budget(&*make().await).await;
    cancelled_job_admits_nothing_and_release_frees(&*make().await).await;
    lease_is_exclusive(&*make().await).await;
}

/// Sync-factory convenience form for stores (like [`super::MemoryFrontierStore`])
/// whose constructor needs no `.await`. Delegates to [`run_all_async`].
pub async fn run_all<F>(make: F)
where
    F: Fn() -> Arc<dyn FrontierStore>,
{
    run_all_async(|| std::future::ready(make())).await;
}
