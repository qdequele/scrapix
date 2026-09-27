//! Job completion decision (R5).
//!
//! A Running job is finalized from exact work accounting
//! ([`JobAccounting`]), not from an idle timer:
//!
//! - once `is_balanced()` has held **continuously** for the completion grace
//!   period (absorbs the transient pop/requeue frontier snapshot and late
//!   sitemap events — see the `scrapix_queue::accounting` module docs), the
//!   job completes, or fails with "No page could be crawled" when not a
//!   single page was crawled successfully;
//! - when no event arrived for the job for the stall timeout while it is not
//!   balanced, the job fails as stalled.
//!
//! This module is pure; the API's completion loop feeds it the state.

use std::time::{Duration, Instant};

use scrapix_queue::JobAccounting;

/// What the completion loop should do with a Running job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finalize {
    /// Keep waiting.
    Wait,
    /// All work is accounted for and at least one page was crawled.
    Complete,
    /// All work is accounted for but no page was crawled successfully.
    FailNoPages,
    /// Not balanced and no event for the stall timeout.
    FailStalled,
}

/// Decide whether a Running job should be finalized.
///
/// `balanced_since` is when the caller first observed `acc.is_balanced()` in
/// the current uninterrupted balanced streak (`None` when the last
/// observation was unbalanced). `last_event` is when the last pipeline event
/// for the job was applied.
pub fn finalize_decision(
    acc: &JobAccounting,
    balanced_since: Option<Instant>,
    last_event: Instant,
    now: Instant,
    grace: Duration,
    stall: Duration,
) -> Finalize {
    if acc.is_balanced() {
        // A stale/absent `balanced_since` never finalizes: the streak must
        // have lasted the whole grace period.
        return match balanced_since {
            Some(since) if now.saturating_duration_since(since) >= grace => {
                if acc.pages_crawled_ok == 0 {
                    Finalize::FailNoPages
                } else {
                    Finalize::Complete
                }
            }
            _ => Finalize::Wait,
        };
    }
    if now.saturating_duration_since(last_event) >= stall {
        return Finalize::FailStalled;
    }
    Finalize::Wait
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrapix_queue::CrawlEvent;

    const GRACE: Duration = Duration::from_secs(3);
    const STALL: Duration = Duration::from_secs(1800);

    fn progress(received: u64, dispatched: u64, queued: u64) -> CrawlEvent {
        CrawlEvent::FrontierProgress {
            job_id: "j".into(),
            instance_id: "f1".into(),
            received,
            admitted: received,
            dispatched,
            rejected: 0,
            dropped: 0,
            queued,
            timestamp: received as i64,
        }
    }

    fn crawled(id: &str) -> CrawlEvent {
        CrawlEvent::PageCrawled {
            job_id: "j".into(),
            account_id: None,
            url: "https://example.com".into(),
            status: 200,
            content_length: 10,
            duration_ms: 1,
            timestamp: 0,
            links_published: 0,
            url_message_id: id.into(),
            js_rendered: false,
            sitemap_pending: false,
        }
    }

    fn indexed(id: &str) -> CrawlEvent {
        CrawlEvent::DocumentIndexed {
            job_id: "j".into(),
            account_id: None,
            url: "https://example.com".into(),
            document_id: "d".into(),
            timestamp: 0,
            url_message_id: id.into(),
            ai_enriched: false,
            ocr_pages: 0,
        }
    }

    fn failed(id: &str) -> CrawlEvent {
        CrawlEvent::PageFailed {
            job_id: "j".into(),
            account_id: None,
            url: "https://example.com".into(),
            error: "404 Not Found".into(),
            retry_count: 0,
            timestamp: 0,
            status: Some(404),
            url_message_id: id.into(),
        }
    }

    fn one_page_ok() -> JobAccounting {
        let mut a = JobAccounting::default();
        a.seeds_published = 1;
        a.apply(&progress(1, 1, 0));
        a.apply(&crawled("m1"));
        a.apply(&indexed("m1"));
        assert!(a.is_balanced());
        a
    }

    fn all_404() -> JobAccounting {
        let mut a = JobAccounting::default();
        a.seeds_published = 2;
        a.apply(&progress(2, 2, 0));
        a.apply(&failed("m1"));
        a.apply(&failed("m2"));
        assert!(a.is_balanced());
        a
    }

    #[test]
    fn decides() {
        let now = Instant::now();
        let ok = one_page_ok();
        assert_eq!(
            finalize_decision(&ok, Some(now), now, now, GRACE, STALL),
            Finalize::Wait
        );
        assert_eq!(
            finalize_decision(
                &ok,
                Some(now - Duration::from_secs(4)),
                now,
                now,
                GRACE,
                STALL
            ),
            Finalize::Complete
        );
    }

    #[test]
    fn zero_pages_balanced_past_grace_fails_no_pages() {
        let now = Instant::now();
        let a = all_404();
        assert_eq!(
            finalize_decision(
                &a,
                Some(now - Duration::from_secs(4)),
                now,
                now,
                GRACE,
                STALL
            ),
            Finalize::FailNoPages
        );
        // Within grace it still waits.
        assert_eq!(
            finalize_decision(&a, Some(now), now, now, GRACE, STALL),
            Finalize::Wait
        );
    }

    #[test]
    fn unbalanced_and_silent_past_stall_timeout_fails_stalled() {
        let now = Instant::now();
        let mut a = JobAccounting::default();
        a.seeds_published = 1;
        a.apply(&progress(1, 1, 0)); // dispatched, no outcome yet
        assert!(!a.is_balanced());
        let last = now - Duration::from_secs(31 * 60);
        assert_eq!(
            finalize_decision(&a, None, last, now, GRACE, STALL),
            Finalize::FailStalled
        );
        // Recent activity: keep waiting.
        assert_eq!(
            finalize_decision(&a, None, now - Duration::from_secs(60), now, GRACE, STALL),
            Finalize::Wait
        );
    }

    #[test]
    fn never_started_job_stalls_eventually() {
        // The frontier never reported (e.g. seeds lost): the job must not
        // stay Running forever.
        let now = Instant::now();
        let mut a = JobAccounting::default();
        a.seeds_published = 1;
        assert_eq!(
            finalize_decision(&a, None, now - Duration::from_secs(1801), now, GRACE, STALL),
            Finalize::FailStalled
        );
    }

    #[test]
    fn stale_balanced_since_is_ignored_when_no_longer_balanced() {
        // The caller should clear balanced_since, but a stale value must
        // never complete an unbalanced job.
        let now = Instant::now();
        let mut a = one_page_ok();
        a.apply(&progress(3, 1, 2)); // new work queued
        assert!(!a.is_balanced());
        assert_eq!(
            finalize_decision(
                &a,
                Some(now - Duration::from_secs(10)),
                now,
                now,
                GRACE,
                STALL
            ),
            Finalize::Wait
        );
    }

    #[test]
    fn balanced_job_in_grace_is_never_stalled() {
        let now = Instant::now();
        let ok = one_page_ok();
        let last = now - Duration::from_secs(3600);
        assert_eq!(
            finalize_decision(&ok, Some(now), last, now, GRACE, STALL),
            Finalize::Wait
        );
    }
}
