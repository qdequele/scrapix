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
use std::time::Duration;

use scrapix_core::CrawlUrl;

use super::{Admission, FrontierStore, JobCounters, JobRunState};

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
    assert_eq!(s.counters("j").await.unwrap().dispatched, 2);
    s.requeue("j", popped).await.unwrap(); // politeness bounce must not consume budget
    let c = s.counters("j").await.unwrap();
    assert_eq!(c.admitted, 2);
    // requeue undoes the pop: `dispatched` counts URLs that actually left.
    assert_eq!(c.dispatched, 0);
    assert_eq!(s.pop_ready("j", 10, i64::MAX).await.unwrap().len(), 2);
    assert_eq!(s.counters("j").await.unwrap().dispatched, 2);
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

/// Regression guard for the two-queue design: 1000 delayed URLs that would
/// otherwise sort ahead of a single due URL (by outranking it on priority)
/// must not stop `pop_ready` from finding and returning the due one. This is
/// a behavioral check, not a timing benchmark — it proves `later` entries
/// never enter `queue`'s scan path until they're promoted.
pub async fn many_delayed_urls_do_not_block_pop_ready(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();

    for i in 0..1000 {
        let mut delayed = CrawlUrl::seed(format!("https://a.test/delayed{i}"));
        delayed.priority = 100; // outranks the due URL below
        delayed.not_before_ms = Some(i64::MAX);
        s.admit("j", &delayed, 2_000).await.unwrap();
    }
    let due = CrawlUrl::seed("https://a.test/due");
    assert_eq!(
        s.admit("j", &due, 2_000).await.unwrap(),
        Admission::Admitted
    );

    let popped = s.pop_ready("j", 1, 1_000).await.unwrap();
    assert_eq!(popped.len(), 1);
    assert_eq!(popped[0].url, "https://a.test/due");
}

/// Pins the missing-job contract documented on the trait: `admit` and
/// `set_state` error for a job that was never `ensure_job`-ed, every other
/// method is a lenient no-op/default. A future Redis-backed store must
/// match this exactly.
pub async fn unknown_job_is_handled_uniformly(s: &dyn FrontierStore) {
    assert!(s
        .admit("ghost", &CrawlUrl::seed("https://a.test/"), 100)
        .await
        .is_err());
    assert!(s.set_state("ghost", JobRunState::Running).await.is_err());

    assert!(s.pop_ready("ghost", 10, i64::MAX).await.unwrap().is_empty());
    assert!(s
        .requeue("ghost", vec![CrawlUrl::seed("https://a.test/")])
        .await
        .is_ok());
    assert_eq!(s.queued("ghost").await.unwrap(), 0);
    assert_eq!(s.counters("ghost").await.unwrap(), JobCounters::default());
    assert_eq!(s.state("ghost").await.unwrap(), None);
    assert_eq!(s.job_template("ghost").await.unwrap(), None);
    assert!(s.release("ghost", Duration::from_secs(1)).await.is_ok());
    assert!(!s
        .active_jobs()
        .await
        .unwrap()
        .contains(&"ghost".to_string()));
}

/// `ensure_job` is first-writer-wins for the template; `release` clears it
/// even though the job entry itself sticks around for `counters`/`state`.
pub async fn job_template_first_writer_wins_and_release_clears(s: &dyn FrontierStore) {
    s.ensure_job("j", "{\"v\":1}", None, None).await.unwrap();
    assert_eq!(
        s.job_template("j").await.unwrap(),
        Some("{\"v\":1}".to_string())
    );

    // A second ensure_job with a different template is a no-op.
    s.ensure_job("j", "{\"v\":2}", None, None).await.unwrap();
    assert_eq!(
        s.job_template("j").await.unwrap(),
        Some("{\"v\":1}".to_string())
    );

    s.set_state("j", JobRunState::Running).await.unwrap();
    s.release("j", Duration::from_secs(60)).await.unwrap();
    assert_eq!(s.job_template("j").await.unwrap(), None);
}

/// `active_jobs` includes a running job and stops including it the moment
/// it is `release`d (not only once its retention window elapses).
pub async fn active_jobs_tracks_running_and_release_removes(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    assert!(s.active_jobs().await.unwrap().contains(&"j".to_string()));

    s.release("j", Duration::from_secs(60)).await.unwrap();
    assert!(!s.active_jobs().await.unwrap().contains(&"j".to_string()));
}

/// `rejected` increments for every non-`Admitted` outcome, whatever the
/// reason: depth, capacity, or run state.
pub async fn rejected_counts_non_admitted_outcomes(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, Some(0)).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();

    let mut deep = CrawlUrl::seed("https://a.test/deep");
    deep.depth = 1;
    assert_eq!(
        s.admit("j", &deep, 100).await.unwrap(),
        Admission::OverDepth
    );
    assert_eq!(s.counters("j").await.unwrap().rejected, 1);

    let shallow = CrawlUrl::seed("https://a.test/shallow");
    assert_eq!(
        s.admit("j", &shallow, 0).await.unwrap(),
        Admission::QueueFull
    );
    assert_eq!(s.counters("j").await.unwrap().rejected, 2);

    // A paused job keeps admitting (it only stops dispatching)...
    s.set_state("j", JobRunState::Paused).await.unwrap();
    assert_eq!(
        s.admit("j", &CrawlUrl::seed("https://a.test/paused"), 100)
            .await
            .unwrap(),
        Admission::Admitted
    );
    // ...a finished one does not.
    s.set_state("j", JobRunState::Finished).await.unwrap();
    assert_eq!(
        s.admit("j", &CrawlUrl::seed("https://a.test/finished"), 100)
            .await
            .unwrap(),
        Admission::JobNotRunning
    );
    assert_eq!(s.counters("j").await.unwrap().rejected, 3);
}

/// `dropped` counts URLs still pending (ready or delayed) when `release`
/// discards them.
pub async fn dropped_counts_release_of_queued_urls(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();

    s.admit("j", &CrawlUrl::seed("https://a.test/1"), 100)
        .await
        .unwrap();
    let mut delayed = CrawlUrl::seed("https://a.test/2");
    delayed.not_before_ms = Some(i64::MAX);
    s.admit("j", &delayed, 100).await.unwrap();
    assert_eq!(s.queued("j").await.unwrap(), 2);

    s.release("j", Duration::from_secs(60)).await.unwrap();
    assert_eq!(s.counters("j").await.unwrap().dropped, 2);
    assert_eq!(s.queued("j").await.unwrap(), 0);
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

/// URLs popped before a `release` (a dispatcher mid-batch when the job was
/// cancelled) and requeued after it are dropped, not queued again.
pub async fn requeue_after_release_drops_the_urls(s: &dyn FrontierStore) {
    s.ensure_job("j", "{}", None, None).await.unwrap();
    s.set_state("j", JobRunState::Running).await.unwrap();
    s.admit("j", &CrawlUrl::seed("https://a.test/1"), 100)
        .await
        .unwrap();
    let popped = s.pop_ready("j", 10, i64::MAX).await.unwrap();
    assert_eq!(popped.len(), 1);
    s.set_state("j", JobRunState::Cancelled).await.unwrap();
    s.release("j", std::time::Duration::from_secs(60))
        .await
        .unwrap();
    s.requeue("j", popped).await.unwrap();
    assert_eq!(s.queued("j").await.unwrap(), 0);
    let c = s.counters("j").await.unwrap();
    assert_eq!(c.dispatched, 0, "the pop is undone");
    assert_eq!(c.dropped, 1, "and the URL counted as dropped");
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
    many_delayed_urls_do_not_block_pop_ready(&*make().await).await;
    unknown_job_is_handled_uniformly(&*make().await).await;
    job_template_first_writer_wins_and_release_clears(&*make().await).await;
    active_jobs_tracks_running_and_release_removes(&*make().await).await;
    rejected_counts_non_admitted_outcomes(&*make().await).await;
    dropped_counts_release_of_queued_urls(&*make().await).await;
    retries_bypass_dedup_and_budget(&*make().await).await;
    cancelled_job_admits_nothing_and_release_frees(&*make().await).await;
    requeue_after_release_drops_the_urls(&*make().await).await;
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
