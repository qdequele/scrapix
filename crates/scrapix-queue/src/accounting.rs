//! Exact work-accounting model for a crawl job.
//!
//! Today a job is declared complete after 10 seconds of pipeline silence.
//! This module replaces that heuristic with an exact model: it folds every
//! relevant [`CrawlEvent`] into running counters and reports [`is_balanced`]
//! once all the work derivable from the job's seeds and discovered links has
//! a terminal outcome recorded for it, and the frontier queue for the job is
//! empty everywhere.
//!
//! This struct is pure and does no I/O: it only observes events handed to it
//! by `apply`. Wiring it to a job's event stream, running it continuously,
//! and gating completion on it (with a grace period — see below) is done by
//! the caller (Task 14).
//!
//! [`is_balanced`]: JobAccounting::is_balanced
//!
//! ## Grace period
//!
//! Between a frontier worker popping a URL (which bumps `dispatched`) and a
//! `PageRetried`/requeue undoing that pop, a `FrontierProgress` snapshot can
//! transiently show `queued` too low and `dispatched` too high relative to
//! what has actually been dispatched and durably queued. `is_balanced` does
//! not smooth over this by itself — a snapshot caught mid-transition can, in
//! principle, still satisfy the inequalities below if the crawl-side
//! outcomes happen to match it, but in general callers MUST NOT treat a
//! single balanced observation as completion. The caller (Task 14) must
//! require `is_balanced()` to hold continuously across a short grace period
//! (e.g. re-checked every second for N seconds) before declaring the job
//! done, so a transient snapshot that briefly looks balanced does not cause
//! premature completion.
//!
//! ## Memory
//!
//! `seen_crawl` and `seen_content` grow with the number of distinct
//! `url_message_id`s observed for the job (one entry per crawled/retried
//! page and one per content outcome). They are unbounded for the lifetime
//! of a single `JobAccounting` instance, which is why they are `#[serde(skip)]`:
//! the persisted (Task 14) representation only keeps the aggregate counters,
//! and the caller is expected to drop the whole `JobAccounting` (seen-sets
//! included) once a job completes, rather than keep accumulating across jobs.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::topics::CrawlEvent;

/// Latest cumulative frontier snapshot for one frontier instance.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct FrontierSnapshot {
    pub received: u64,
    pub admitted: u64,
    pub dispatched: u64,
    pub queued: u64,
}

/// Exact, pure work-accounting state for a single crawl job.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobAccounting {
    /// Number of seed URLs the job was started with.
    pub seeds_published: u64,
    /// Sum of `PageCrawled.links_published` + `SitemapPublished.count` +
    /// `PageRetried` (1 each, since a retry re-publishes a frontier message).
    pub links_published: u64,
    /// Latest cumulative `FrontierProgress` per frontier instance.
    pub frontier: BTreeMap<String, FrontierSnapshot>,
    /// Crawler terminal outcomes per dispatched message (deduped by
    /// `url_message_id`).
    pub crawl_outcomes: u64,
    pub pages_crawled_ok: u64,
    pub pages_failed: u64,
    pub pages_browser: u64,
    pub bytes_downloaded: u64,
    /// Content outcomes per crawled page (deduped by `url_message_id`).
    pub content_outcomes: u64,
    pub documents_indexed: u64,
    pub pages_ai: u64,

    #[serde(skip)]
    seen_crawl: HashSet<String>,
    #[serde(skip)]
    seen_content: HashSet<String>,
    #[serde(skip)]
    seen_sitemap: HashSet<String>,
}

impl JobAccounting {
    /// Fold one pipeline event into the accounting state.
    ///
    /// Dedup is by `url_message_id`: an empty id (old workers that don't set
    /// it) is counted without dedup, since there's nothing to dedup against.
    /// Crawl outcomes and content outcomes are deduped in separate sets, and
    /// `SitemapPublished` is deduped by its own id, since it does not
    /// represent a crawl or content terminal outcome.
    pub fn apply(&mut self, event: &CrawlEvent) {
        match event {
            CrawlEvent::PageCrawled {
                url_message_id,
                links_published,
                content_length,
                js_rendered,
                ..
            } => {
                if Self::first_time(&mut self.seen_crawl, url_message_id) {
                    self.crawl_outcomes += 1;
                    self.pages_crawled_ok += 1;
                    self.links_published += links_published;
                    self.bytes_downloaded += content_length;
                    if *js_rendered {
                        self.pages_browser += 1;
                    }
                }
            }
            CrawlEvent::PageFailed { url_message_id, .. } => {
                if Self::first_time(&mut self.seen_crawl, url_message_id) {
                    self.crawl_outcomes += 1;
                    self.pages_failed += 1;
                }
            }
            CrawlEvent::PageSkipped { url_message_id, .. } => {
                if Self::first_time(&mut self.seen_crawl, url_message_id) {
                    self.crawl_outcomes += 1;
                }
            }
            CrawlEvent::PageRetried { url_message_id, .. } => {
                if Self::first_time(&mut self.seen_crawl, url_message_id) {
                    self.crawl_outcomes += 1;
                    self.links_published += 1;
                }
            }
            CrawlEvent::DocumentIndexed {
                url_message_id,
                ai_enriched,
                ..
            } => {
                if Self::first_time(&mut self.seen_content, url_message_id) {
                    self.content_outcomes += 1;
                    self.documents_indexed += 1;
                    if *ai_enriched {
                        self.pages_ai += 1;
                    }
                }
            }
            CrawlEvent::DocumentSkipped { url_message_id, .. } => {
                if Self::first_time(&mut self.seen_content, url_message_id) {
                    self.content_outcomes += 1;
                }
            }
            CrawlEvent::DocumentFailed { url_message_id, .. } => {
                if Self::first_time(&mut self.seen_content, url_message_id) {
                    self.content_outcomes += 1;
                }
            }
            CrawlEvent::SitemapPublished {
                count,
                url_message_id,
                ..
            } => {
                if Self::first_time(&mut self.seen_sitemap, url_message_id) {
                    self.links_published += *count as u64;
                }
            }
            CrawlEvent::FrontierProgress {
                instance_id,
                received,
                admitted,
                dispatched,
                queued,
                ..
            } => {
                let entry = self.frontier.entry(instance_id.clone()).or_default();
                if *received >= entry.received {
                    entry.received = *received;
                    entry.admitted = *admitted;
                    entry.dispatched = *dispatched;
                    entry.queued = *queued;
                }
            }
            _ => {}
        }
    }

    /// Returns true and records `id` the first time it is seen; an empty id
    /// is never considered seen (old workers that don't set `url_message_id`
    /// are counted without dedup).
    fn first_time(seen: &mut HashSet<String>, id: &str) -> bool {
        if id.is_empty() {
            return true;
        }
        seen.insert(id.to_string())
    }

    /// All work derived from the job is accounted for.
    ///
    /// This alone is not sufficient to declare the job complete: see the
    /// module-level "Grace period" docs. A single balanced observation can
    /// be a transient artifact of a pop/requeue race in the frontier; the
    /// caller must require this to hold continuously for a grace period.
    pub fn is_balanced(&self) -> bool {
        let received: u64 = self.frontier.values().map(|f| f.received).sum();
        let dispatched: u64 = self.frontier.values().map(|f| f.dispatched).sum();
        let queued: u64 = self.frontier.values().map(|f| f.queued).sum();
        self.seeds_published + self.links_published <= received
            && queued == 0
            && self.crawl_outcomes >= dispatched
            && self.content_outcomes >= self.pages_crawled_ok
            && received > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(instance: &str, received: u64, dispatched: u64, queued: u64) -> CrawlEvent {
        CrawlEvent::FrontierProgress {
            job_id: "j".into(),
            instance_id: instance.into(),
            received,
            admitted: received,
            dispatched,
            rejected: 0,
            dropped: 0,
            queued,
            timestamp: 0,
        }
    }

    fn crawled(id: &str, links: u64) -> CrawlEvent {
        CrawlEvent::PageCrawled {
            job_id: "j".into(),
            account_id: None,
            url: "https://example.com".into(),
            status: 200,
            content_length: 0,
            duration_ms: 0,
            timestamp: 0,
            links_published: links,
            url_message_id: id.into(),
            js_rendered: false,
        }
    }

    fn indexed(id: &str) -> CrawlEvent {
        CrawlEvent::DocumentIndexed {
            job_id: "j".into(),
            account_id: None,
            url: "https://example.com".into(),
            document_id: "doc1".into(),
            timestamp: 0,
            url_message_id: id.into(),
            ai_enriched: false,
        }
    }

    fn failed(id: &str) -> CrawlEvent {
        CrawlEvent::PageFailed {
            job_id: "j".into(),
            account_id: None,
            url: "https://example.com".into(),
            error: "not found".into(),
            retry_count: 0,
            timestamp: 0,
            status: Some(404),
            url_message_id: id.into(),
        }
    }

    #[test]
    fn single_page_site_balances_after_content_outcome() {
        let mut a = JobAccounting {
            seeds_published: 1,
            ..Default::default()
        };
        a.apply(&progress("f1", 1, 1, 0));
        a.apply(&crawled("m1", 0));
        assert!(!a.is_balanced(), "content worker has not reported yet");
        a.apply(&indexed("m1"));
        assert!(a.is_balanced());
    }

    #[test]
    fn discovered_links_in_flight_keep_job_open() {
        let mut a = JobAccounting {
            seeds_published: 1,
            ..Default::default()
        };
        a.apply(&progress("f1", 1, 1, 0));
        a.apply(&crawled("m1", 2));
        a.apply(&indexed("m1"));
        assert!(!a.is_balanced(), "2 links not yet received by the frontier");
        a.apply(&progress("f1", 3, 3, 0));
        a.apply(&failed("m2"));
        a.apply(&failed("m3"));
        assert!(a.is_balanced());
    }

    #[test]
    fn all_seeds_404_balances() {
        let mut a = JobAccounting {
            seeds_published: 2,
            ..Default::default()
        };
        a.apply(&progress("f1", 2, 2, 0));
        a.apply(&failed("m1"));
        a.apply(&failed("m2"));
        assert!(a.is_balanced());
    }

    #[test]
    fn duplicate_outcome_events_do_not_overcount() {
        let mut a = JobAccounting {
            seeds_published: 2,
            ..Default::default()
        };
        a.apply(&progress("f1", 2, 2, 0));
        a.apply(&failed("m1"));
        a.apply(&failed("m1")); // redelivered
        assert!(!a.is_balanced());
    }

    #[test]
    fn stale_progress_is_ignored() {
        let mut a = JobAccounting::default();
        a.apply(&progress("f1", 5, 5, 0));
        a.apply(&progress("f1", 3, 3, 2));
        assert_eq!(a.frontier["f1"].received, 5);
    }

    /// Extra test (not in the brief): a transient pop/requeue snapshot must
    /// not be reported as balanced just because crawl-side outcomes happen
    /// to match the "actual" dispatched count. Here the frontier reports
    /// `dispatched` inflated by `k` beyond what was truly dispatched (a
    /// pop that hasn't been undone by its requeue yet) with `queued == 0`,
    /// while only the true count of outcomes has arrived — `is_balanced`
    /// must be false because `crawl_outcomes < dispatched`.
    #[test]
    fn transient_pop_requeue_snapshot_is_not_balanced() {
        let mut a = JobAccounting {
            seeds_published: 2,
            ..Default::default()
        };
        // Frontier popped both seeds (dispatched=2) but one pop's requeue
        // hasn't landed yet, so this snapshot over-reports dispatched by
        // k=1 relative to what has truly been handed off, while queued
        // already reads 0.
        let k = 1;
        a.apply(&progress("f1", 2, 2 + k, 0));
        // Only the actual (non-transient) outcomes have arrived so far.
        a.apply(&failed("m1"));
        a.apply(&failed("m2"));
        assert!(
            !a.is_balanced(),
            "dispatched is inflated by the in-flight requeue; outcomes haven't caught up"
        );
    }
}
