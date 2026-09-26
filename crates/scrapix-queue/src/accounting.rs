//! Exact work-accounting model for a crawl job.
//!
//! The job lifecycle used to declare a job complete after 10 seconds of
//! pipeline silence — a heuristic that both completed jobs too early (a slow
//! straggler URL after a burst of fast ones) and too late (any lull longer
//! than 10s reset the clock). This module replaces that heuristic with an
//! exact model: it folds every relevant [`CrawlEvent`] into running counters
//! and reports [`is_balanced`] once all the work derivable from the job's
//! seeds and discovered links (including sitemap-seeded links) has a
//! terminal outcome recorded for it, and the frontier queue for the job is
//! empty.
//!
//! This struct is pure and does no I/O: it only observes events handed to it
//! by `apply`. Wiring it to a job's event stream, running it continuously,
//! and gating completion on it (with a grace period — see below) is done by
//! the caller (Task 14).
//!
//! [`is_balanced`]: JobAccounting::is_balanced
//!
//! ## Frontier snapshot is job-global, not per-instance (R-17)
//!
//! `FrontierProgress` carries an `instance_id`, but the counters it reports
//! (`received`/`admitted`/`dispatched`/`queued`) are read from the frontier's
//! shared store and are **global to the job**, not local to the publishing
//! instance — any instance holding the job's dispatch lease publishes the
//! same job-wide counters, just tagged with its own `instance_id`. Summing
//! these across instances (as an earlier version of this module did) double-
//! counts every counter after a lease handover between instances, and the
//! job then never balances. `JobAccounting` therefore keeps exactly **one**
//! frontier snapshot per job: a new `FrontierProgress` replaces it when the
//! new `received` is strictly greater than the stored one, or when `received`
//! is equal and the event's `timestamp` is `>=` the stored one (`dispatched`
//! can legitimately drop between two equal-`received` snapshots, e.g. across
//! a requeue, so `received` alone cannot always order two snapshots).
//! `instance_id` is kept on the stored snapshot for observability but plays
//! no part in `is_balanced`.
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
//! ## Sitemap discovery is asynchronous work too (R-18)
//!
//! Sitemap discovery is spawned in the background after a page fetch and can
//! finish well after the page's own crawl/content outcomes have landed — and
//! after the job would otherwise look balanced. `PageCrawled.sitemap_pending`
//! marks the one message per `(job, domain)` that spawned a first-time
//! discovery; `sitemaps_expected` counts those, and `sitemaps_settled` counts
//! the matching `SitemapPublished` events (which the crawler is required to
//! publish on every path — success, empty, disabled, or error — so this
//! always eventually catches up). `is_balanced` additionally requires
//! `sitemaps_settled >= sitemaps_expected`.
//!
//! ## Empty `url_message_id` and rolling deploys
//!
//! An old worker that hasn't been upgraded yet publishes outcome events with
//! an empty `url_message_id`. Those are never deduped (there is nothing to
//! dedup against), so **every** delivery of such an event counts, including
//! a redelivery of the same underlying outcome. During a rolling deploy this
//! can inflate `crawl_outcomes`/`content_outcomes` beyond the true number of
//! distinct pages, which can make `is_balanced` return true earlier than the
//! job's real work is done (a premature-balance risk), on top of the usual
//! at-least-once redelivery semantics. This is accepted as a bounded,
//! deploy-window-only risk rather than solved here: it goes away once every
//! worker in the fleet is upgraded to set `url_message_id`.
//!
//! ## Memory
//!
//! `seen_crawl`, `seen_content` and `seen_sitemap` grow with the number of
//! distinct `url_message_id`s observed for the job (one entry per
//! crawled/retried page, one per content outcome, and one per sitemap-
//! discovery-triggering page). They are unbounded for the lifetime of a
//! single `JobAccounting` instance, which is why they are `#[serde(skip)]`:
//! the persisted (Task 14) representation only keeps the aggregate counters,
//! and the caller is expected to drop the whole `JobAccounting` (seen-sets
//! included) once a job completes, rather than keep accumulating across jobs.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::topics::CrawlEvent;

/// Latest job-global frontier snapshot (R-17: `FrontierProgress` counters are
/// global to the job, not local to the publishing instance, so only one
/// snapshot is ever kept per job).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct FrontierSnapshot {
    /// Instance that published this snapshot. Kept for observability only —
    /// not used by `is_balanced`.
    pub instance_id: String,
    pub received: u64,
    pub admitted: u64,
    pub dispatched: u64,
    pub queued: u64,
    /// Event timestamp, used to break ties when two snapshots report the
    /// same `received` (see the module-level staleness rule).
    pub timestamp: i64,
}

/// Exact, pure work-accounting state for a single crawl job.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobAccounting {
    /// Number of seed URLs the job was started with.
    pub seeds_published: u64,
    /// Sum of `PageCrawled.links_published` + `SitemapPublished.count` +
    /// `PageRetried` (1 each, since a retry re-publishes a frontier message).
    pub links_published: u64,
    /// Latest job-global `FrontierProgress` snapshot (R-17).
    pub frontier: FrontierSnapshot,
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
    /// Number of messages that spawned a first-time sitemap discovery for
    /// their `(job, domain)` (`PageCrawled.sitemap_pending`, R-18).
    pub sitemaps_expected: u64,
    /// Number of matching `SitemapPublished` events observed (deduped by
    /// `url_message_id`).
    pub sitemaps_settled: u64,

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
    /// Dedup is by `url_message_id`: an empty id (old workers) is counted
    /// without dedup — see the module-level "Empty `url_message_id`" docs
    /// for the premature-balance risk this carries during a rolling deploy.
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
                sitemap_pending,
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
                    if *sitemap_pending {
                        self.sitemaps_expected += 1;
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
                    self.sitemaps_settled += 1;
                }
            }
            CrawlEvent::FrontierProgress {
                instance_id,
                received,
                admitted,
                dispatched,
                queued,
                timestamp,
                ..
            } => {
                // R-17: these counters are job-global (read from the shared
                // frontier store), not local to `instance_id` — keep only
                // the single latest snapshot for the whole job. `received`
                // is cumulative, so a strictly greater value is always
                // newer; on a tie, `dispatched` can have dropped (e.g. a
                // requeue), so break the tie on the event timestamp instead.
                let replace = *received > self.frontier.received
                    || (*received == self.frontier.received
                        && *timestamp >= self.frontier.timestamp);
                if replace {
                    self.frontier = FrontierSnapshot {
                        instance_id: instance_id.clone(),
                        received: *received,
                        admitted: *admitted,
                        dispatched: *dispatched,
                        queued: *queued,
                        timestamp: *timestamp,
                    };
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
        self.seeds_published + self.links_published <= self.frontier.received
            && self.frontier.queued == 0
            && self.crawl_outcomes >= self.frontier.dispatched
            && self.content_outcomes >= self.pages_crawled_ok
            && self.sitemaps_settled >= self.sitemaps_expected
            && self.frontier.received > 0
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
            timestamp: received as i64,
        }
    }

    fn progress_at(
        instance: &str,
        received: u64,
        dispatched: u64,
        queued: u64,
        timestamp: i64,
    ) -> CrawlEvent {
        CrawlEvent::FrontierProgress {
            job_id: "j".into(),
            instance_id: instance.into(),
            received,
            admitted: received,
            dispatched,
            rejected: 0,
            dropped: 0,
            queued,
            timestamp,
        }
    }

    fn crawled(id: &str, links: u64) -> CrawlEvent {
        crawled_with(id, links, false)
    }

    fn crawled_with(id: &str, links: u64, sitemap_pending: bool) -> CrawlEvent {
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
            sitemap_pending,
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

    fn skipped(id: &str) -> CrawlEvent {
        CrawlEvent::PageSkipped {
            job_id: "j".into(),
            url: "https://example.com".into(),
            reason: "duplicate".into(),
            timestamp: 0,
            url_message_id: id.into(),
        }
    }

    fn retried(id: &str) -> CrawlEvent {
        CrawlEvent::PageRetried {
            job_id: "j".into(),
            url: "https://example.com".into(),
            url_message_id: id.into(),
            retry_count: 1,
            error: "timeout".into(),
            timestamp: 0,
        }
    }

    fn sitemap_published(id: &str, count: usize) -> CrawlEvent {
        CrawlEvent::SitemapPublished {
            job_id: "j".into(),
            count,
            url_message_id: id.into(),
            timestamp: 0,
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
        a.apply(&progress_at("f1", 5, 5, 0, 10));
        a.apply(&progress_at("f1", 3, 3, 2, 20));
        assert_eq!(a.frontier.received, 5);
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

    /// R-17: two frontier instances (e.g. across a dispatch-lease handover)
    /// reporting the *same* job-global counters must not be double-counted.
    /// A then B with identical numbers, plus a later B, must land on exactly
    /// the same accounting as if only one instance had ever reported.
    #[test]
    fn frontier_counters_are_job_global_not_summed_per_instance() {
        let mut multi = JobAccounting {
            seeds_published: 2,
            ..Default::default()
        };
        multi.apply(&progress_at("instance-a", 2, 2, 0, 10));
        multi.apply(&progress_at("instance-b", 2, 2, 0, 20));
        multi.apply(&progress_at("instance-b", 2, 2, 0, 30));
        multi.apply(&failed("m1"));
        multi.apply(&failed("m2"));

        let mut single = JobAccounting {
            seeds_published: 2,
            ..Default::default()
        };
        single.apply(&progress_at("instance-a", 2, 2, 0, 10));
        single.apply(&failed("m1"));
        single.apply(&failed("m2"));

        // `instance_id` legitimately differs (it's whichever instance
        // published last); every counter that `is_balanced` actually reads
        // must match exactly, matching the single-instance result.
        assert_eq!(multi.frontier.received, single.frontier.received);
        assert_eq!(multi.frontier.dispatched, single.frontier.dispatched);
        assert_eq!(multi.frontier.queued, single.frontier.queued);
        assert_eq!(multi.is_balanced(), single.is_balanced());
        assert!(multi.is_balanced());
    }

    /// A snapshot with the same `received` but a lower `dispatched` (e.g.
    /// after a requeue undid a pop) must still be accepted when its
    /// timestamp is `>=` the stored one — `received` alone cannot order two
    /// equal-`received` snapshots.
    #[test]
    fn equal_received_lower_dispatched_snapshot_is_accepted_when_not_older() {
        let mut a = JobAccounting::default();
        a.apply(&progress_at("f1", 2, 2, 0, 10));
        a.apply(&progress_at("f1", 2, 1, 1, 20));
        assert_eq!(a.frontier.dispatched, 1);
        assert_eq!(a.frontier.queued, 1);
    }

    /// `PageRetried` is a crawl outcome for the message it retries (it's
    /// terminal for that dispatch attempt) and also re-publishes a frontier
    /// message, so it adds exactly one to `links_published`.
    #[test]
    fn page_retried_counts_as_outcome_and_adds_one_link() {
        let mut a = JobAccounting::default();
        a.apply(&retried("m1"));
        assert_eq!(a.crawl_outcomes, 1);
        assert_eq!(a.links_published, 1);
        // Deduped like every other crawl outcome.
        a.apply(&retried("m1"));
        assert_eq!(a.crawl_outcomes, 1);
        assert_eq!(a.links_published, 1);
    }

    /// `PageSkipped` is a terminal crawl outcome, deduped by id, that does
    /// not itself publish any links.
    #[test]
    fn page_skipped_counts_as_outcome_without_links() {
        let mut a = JobAccounting::default();
        a.apply(&skipped("m1"));
        assert_eq!(a.crawl_outcomes, 1);
        assert_eq!(a.links_published, 0);
        a.apply(&skipped("m1")); // redelivered
        assert_eq!(a.crawl_outcomes, 1);
    }

    /// `SitemapPublished` is deduped by its own `url_message_id`, separate
    /// from crawl/content outcome dedup.
    #[test]
    fn sitemap_published_is_deduped_by_its_own_id() {
        let mut a = JobAccounting::default();
        a.apply(&sitemap_published("m1", 3));
        assert_eq!(a.links_published, 3);
        assert_eq!(a.sitemaps_settled, 1);
        a.apply(&sitemap_published("m1", 3)); // redelivered
        assert_eq!(a.links_published, 3);
        assert_eq!(a.sitemaps_settled, 1);
    }

    /// Events with an empty `url_message_id` (old workers, pre-upgrade) are
    /// never deduped: every delivery counts.
    #[test]
    fn empty_url_message_id_is_never_deduped() {
        let mut a = JobAccounting::default();
        a.apply(&failed(""));
        a.apply(&failed("")); // "redelivered" old-worker event: still counts
        assert_eq!(a.crawl_outcomes, 2);
    }

    /// R-18: a page whose fetch spawned a first-time sitemap discovery must
    /// keep the job open until the matching `SitemapPublished` lands, even
    /// with zero sitemap-seeded links (the discovery found nothing / was
    /// disabled / errored — the crawler always still publishes the event).
    #[test]
    fn sitemap_pending_page_waits_for_sitemap_published_with_zero_links() {
        let mut a = JobAccounting {
            seeds_published: 1,
            ..Default::default()
        };
        a.apply(&progress("f1", 1, 1, 0));
        a.apply(&crawled_with("m1", 0, true));
        a.apply(&indexed("m1"));
        assert!(
            !a.is_balanced(),
            "sitemap discovery for m1's domain hasn't settled yet"
        );
        a.apply(&sitemap_published("m1", 0));
        assert!(a.is_balanced());
    }

    /// R-18: when sitemap discovery does find links, the job stays open
    /// until the frontier has received them too, in addition to the
    /// `SitemapPublished` settling.
    #[test]
    fn sitemap_pending_page_waits_for_discovered_links_to_be_received() {
        let mut a = JobAccounting {
            seeds_published: 1,
            ..Default::default()
        };
        a.apply(&progress("f1", 1, 1, 0));
        a.apply(&crawled_with("m1", 0, true));
        a.apply(&indexed("m1"));
        a.apply(&sitemap_published("m1", 3));
        assert!(
            !a.is_balanced(),
            "3 sitemap-seeded links not yet received by the frontier"
        );
        a.apply(&progress("f1", 4, 4, 0));
        a.apply(&failed("s1"));
        a.apply(&failed("s2"));
        a.apply(&failed("s3"));
        assert!(a.is_balanced());
    }
}
