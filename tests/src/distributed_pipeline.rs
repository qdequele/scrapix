//! Distributed-path integration tests (spec R10).
//!
//! Unlike the other suites (which wire library objects together by hand),
//! these run the real frontier service, crawler worker and content worker
//! over the in-process bus, against a wiremock site and a wiremock
//! Meilisearch, and check the job's exact work accounting. They cover R1
//! (status honored), R2 (at-least-once, no double count), R3 (job context),
//! R5 (lifecycle: balance, max_pages, cancel) and R7 (politeness).

mod support {
    pub mod pipeline;
}

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use scrapix_queue::CrawlEvent;
use support::pipeline::*;
use wiremock::matchers::{header_exists, method, path_regex};
use wiremock::{Mock, ResponseTemplate};

fn links(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

/// Serve `/p0` … `/p{n-1}`, each linking to every other page, plus `/`
/// linking to all of them. Returns the page paths (including `/`).
async fn serve_mesh(site: &Site, n: usize, delay: Option<Duration>) -> Vec<String> {
    let pages: Vec<String> = (0..n).map(|i| format!("/p{i}")).collect();
    let mut all = vec!["/".to_string()];
    all.extend(pages.iter().cloned());
    for p in &all {
        let mut r = html_response(page_html(p, &pages));
        if let Some(d) = delay {
            r = r.set_delay(d);
        }
        site.respond(p, r).await;
    }
    all
}

// 1 -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_page_site_indexes_three_documents_and_balances() {
    let site = Site::start().await;
    site.page("/", page_html("Home", &links(&["/a", "/b"])))
        .await;
    site.page("/a", page_html("A", &links(&["/", "/b"]))).await;
    site.page("/b", page_html("B", &links(&["/", "/a"]))).await;

    let p = Pipeline::start(PipelineConfig::default()).await;
    p.submit(&[p.seed(&site.url("/"), "job-three")]).await;

    let acc = p.wait_balanced(|a, _| a.documents_indexed == 3).await;
    assert_eq!(acc.pages_crawled_ok, 3, "{acc:#?}");
    assert_eq!(acc.pages_failed, 0, "{acc:#?}");
    assert_eq!(acc.documents_indexed, 3, "{acc:#?}");
    assert_eq!(p.queued("job-three").await, 0);

    let urls = p.meili.document_urls().await;
    let expected: BTreeSet<String> = ["/", "/a", "/b"].iter().map(|x| site.url(x)).collect();
    assert_eq!(urls, expected);
    for path in ["/", "/a", "/b"] {
        assert_eq!(site.hits(path).await, 1, "{path} fetched exactly once");
    }
}

// 2 -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_pages_are_not_indexed() {
    let site = Site::start().await;
    site.page("/", page_html("Home", &links(&["/missing", "/flaky"])))
        .await;
    site.respond(
        "/missing",
        ResponseTemplate::new(404).set_body_raw(page_html("Not found", &[]), "text/html"),
    )
    .await;
    // Always 503; `Retry-After: 0` so the re-queue delay is short (the
    // hint is honored instead of the 30s exponential backoff).
    site.respond(
        "/flaky",
        ResponseTemplate::new(503)
            .insert_header("retry-after", "0")
            .set_body_raw(page_html("Unavailable", &[]), "text/html"),
    )
    .await;

    let config = PipelineConfig::default();
    let max_retries = config.max_retries as usize;
    let p = Pipeline::start(config).await;
    p.submit(&[p.seed(&site.url("/"), "job-errors")]).await;

    let flaky = site.url("/flaky");
    let acc = p
        .wait_balanced(|a, e| {
            a.pages_failed == 2 && a.documents_indexed == 1 && {
                failures(e).iter().any(|(u, _)| *u == flaky)
            }
        })
        .await;
    let events = p.events();

    assert_eq!(acc.pages_crawled_ok, 1, "{acc:#?}");
    assert_eq!(acc.documents_indexed, 1, "{acc:#?}");
    assert_eq!(
        p.meili.document_urls().await,
        BTreeSet::from([site.url("/")])
    );

    let failed = failures(&events);
    assert!(
        failed.contains(&(site.url("/missing"), Some(404))),
        "{failed:?}"
    );
    assert!(failed.contains(&(flaky.clone(), Some(503))), "{failed:?}");
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert_eq!(
        retries_of(&events, &flaky),
        max_retries,
        "/flaky re-queued MAX_RETRIES times before failing"
    );
    // Neither error page reached the content worker.
    assert!(!crawled_urls(&events).contains(&flaky));
    assert!(!crawled_urls(&events).contains(&site.url("/missing")));
    // A 404 is terminal: fetched once, never retried.
    assert_eq!(retries_of(&events, &site.url("/missing")), 0);
    assert_eq!(site.hits("/missing").await, 1);
}

// 3 -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_context_reaches_every_document() {
    let site = Site::start().await;
    serve_mesh(&site, 4, None).await;

    let p = Pipeline::start(PipelineConfig::default()).await;
    p.submit(&[p.seed(&site.url("/"), "job-context")]).await;

    let acc = p.wait_balanced(|a, _| a.documents_indexed == 5).await;
    assert_eq!(acc.pages_crawled_ok, 5, "{acc:#?}");

    // Every document (seed and discovered pages alike) carries the job's
    // source and job id.
    let docs = p.meili.documents().await;
    assert!(docs.len() >= 5, "{docs:#?}");
    for d in &docs {
        assert_eq!(d["source"], "src-test", "{d:#}");
        assert_eq!(d["_crawl_job_id"], "job-context", "{d:#}");
    }
    // Every page event carries the job's account (billing attribution).
    let events = p.events();
    let mut crawled = 0;
    let mut indexed = 0;
    for e in &events {
        match e {
            CrawlEvent::PageCrawled {
                account_id, job_id, ..
            } => {
                crawled += 1;
                assert_eq!(account_id.as_deref(), Some("acct-test"), "{e:?}");
                assert_eq!(job_id, "job-context");
            }
            CrawlEvent::DocumentIndexed { account_id, .. } => {
                indexed += 1;
                assert_eq!(account_id.as_deref(), Some("acct-test"), "{e:?}");
            }
            _ => {}
        }
    }
    assert_eq!(crawled, 5);
    assert_eq!(indexed, 5);
}

// 4 -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_pages_is_exact() {
    let site = Site::start().await;
    serve_mesh(&site, 19, None).await; // 20 pages with `/`

    let p = Pipeline::start(PipelineConfig::default()).await;
    let seed = p.seed(&site.url("/"), "job-max").with_limits(None, Some(5));
    p.submit(&[seed]).await;

    let acc = p.wait_balanced(|a, _| a.documents_indexed == 5).await;
    assert_eq!(acc.pages_crawled_ok, 5, "{acc:#?}");
    assert_eq!(acc.frontier.dispatched, 5, "{acc:#?}");
    let events = p.events();
    assert_eq!(count_crawled(&events), 5, "{:?}", crawled_urls(&events));
    assert_eq!(
        site.page_requests().await.len(),
        5,
        "exactly 5 pages fetched"
    );
    assert_eq!(p.meili.document_urls().await.len(), 5);
    assert_eq!(p.queued("job-max").await, 0);
}

// 5 -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replace_job_never_sends_conditional_headers() {
    let site = Site::start().await;
    // Trap: any conditional request gets a 304 (and would not be indexed).
    for h in ["if-none-match", "if-modified-since"] {
        Mock::given(method("GET"))
            .and(path_regex(r"^/.*$"))
            .and(header_exists(h))
            .respond_with(ResponseTemplate::new(304))
            .with_priority(1)
            .mount(&site.server)
            .await;
    }
    serve_mesh(&site, 3, None).await;

    let p = Pipeline::start(PipelineConfig::default()).await;
    // Replace strategy: the API sends `incremental: false`. The seed carries
    // validators from a previous crawl, which an incremental job would send.
    let mut seed = p
        .seed(&site.url("/"), "job-replace")
        .with_incremental(false);
    seed.url.etag = Some("\"v1\"".into());
    seed.url.last_modified = Some("Wed, 01 Jan 2025 00:00:00 GMT".into());
    p.submit(&[seed]).await;

    let acc = p.wait_balanced(|a, _| a.documents_indexed == 4).await;
    assert_eq!(acc.pages_crawled_ok, 4, "{acc:#?}");
    assert_eq!(acc.pages_failed, 0, "{acc:#?}");
    assert_eq!(p.meili.document_urls().await.len(), 4);

    let requests = site.page_requests().await;
    assert_eq!(requests.len(), 4);
    for r in &requests {
        for h in ["if-none-match", "if-modified-since"] {
            assert!(
                !r.headers.contains_key(h),
                "{} sent {h} on a replace job",
                r.url
            );
        }
    }
}

// 6 -------------------------------------------------------------------------

/// Redelivery is simulated with the channel bus test hooks
/// (`scrapix-queue/test-hooks`): the first crawler is crashed mid-job (its
/// in-flight handlers are aborted, their acks never fire), the un-acked
/// messages are republished (like a Kafka rebalance) and a second crawler
/// picks them up. Some of the crashed handlers may already have published
/// their `PageCrawled`, so the second crawler redoes and re-reports them:
/// accounting must still count one terminal outcome per dispatched URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crawler_redelivery_does_not_double_count() {
    let site = Site::start().await;
    let n = 12;
    let all = serve_mesh(&site, n - 1, Some(Duration::from_millis(300))).await;

    let config = PipelineConfig {
        concurrent_per_domain: 4,
        crawler_concurrency: 8,
        // Slots held by the crashed crawler's aborted fetches expire fast.
        request_timeout_secs: 1,
        ..PipelineConfig::default()
    };
    let p = Pipeline::start(config).await;
    p.submit(&[p.seed(&site.url("/"), "job-redeliver")]).await;

    // Crash once the seed was crawled and several fetches are in flight.
    p.wait_for("first page crawled", |_, e| count_crawled(e) >= 1)
        .await;
    let deadline = Instant::now() + TIMEOUT;
    while p.with_crawler(0, |c| c.consumer.unacked_count()) < 2 {
        assert!(Instant::now() < deadline, "no in-flight fetches to crash");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    p.with_crawler(0, |c| c.crash());
    let redelivered = p
        .with_crawler(0, |c| c.consumer.clone())
        .redeliver_unacked()
        .await;
    assert!(redelivered >= 2, "redelivered {redelivered}");
    p.spawn_crawler().await;

    let acc = p
        .wait_balanced(|a, _| a.documents_indexed == n as u64)
        .await;
    assert_eq!(acc.pages_crawled_ok, n as u64, "{acc:#?}");
    assert_eq!(acc.pages_failed, 0, "{acc:#?}");
    assert_eq!(acc.crawl_outcomes, acc.frontier.dispatched, "{acc:#?}");
    assert_eq!(acc.frontier.dispatched, n as u64, "{acc:#?}");

    // Each URL has exactly one terminal outcome identity (one dispatched
    // message), however many times it was reported.
    let mut ids: HashMap<String, HashSet<String>> = HashMap::new();
    for e in p.events() {
        if let CrawlEvent::PageCrawled {
            url,
            url_message_id,
            ..
        }
        | CrawlEvent::PageFailed {
            url,
            url_message_id,
            ..
        } = e
        {
            ids.entry(url).or_default().insert(url_message_id);
        }
    }
    assert_eq!(ids.len(), n, "{ids:#?}");
    for (url, set) in &ids {
        assert_eq!(set.len(), 1, "{url} has several terminal outcomes: {set:?}");
    }
    let expected: BTreeSet<String> = all.iter().map(|x| site.url(x)).collect();
    assert_eq!(p.meili.document_urls().await, expected);
}

// 7 -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_stops_the_crawl() {
    let site = Site::start().await;
    serve_mesh(&site, 199, Some(Duration::from_millis(50))).await; // 200 pages

    let p = Pipeline::start(PipelineConfig::default()).await;
    p.submit(&[p.seed(&site.url("/"), "job-cancel")]).await;

    p.wait_for("first page crawled", |_, e| count_crawled(e) >= 1)
        .await;
    p.cancel("job-cancel").await;
    let cancelled_at = Instant::now();

    // In-flight fetches may still land during the first 2s…
    let deadline = Instant::now() + TIMEOUT;
    while p.queued("job-cancel").await != 0 {
        assert!(Instant::now() < deadline, "frontier queue never drained");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_secs(2).saturating_sub(cancelled_at.elapsed())).await;
    let after_grace = count_crawled(&p.events());
    let fetched_after_grace = site.page_requests().await.len();

    // …but nothing is crawled after that.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let events = p.events();
    assert_eq!(
        count_crawled(&events),
        after_grace,
        "PageCrawled after cancel + 2s"
    );
    assert_eq!(
        site.page_requests().await.len(),
        fetched_after_grace,
        "site fetched after cancel + 2s"
    );
    assert!(
        after_grace < 100,
        "cancel did not stop the crawl: {after_grace} pages"
    );
    assert_eq!(p.queued("job-cancel").await, 0);
}

// 8 -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn politeness_limits_concurrency_per_domain() {
    let site = Site::start().await;
    let delay = Duration::from_millis(200);
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let pages: Vec<String> = (0..5).map(|i| format!("/p{i}")).collect();
    let mut all = vec!["/".to_string()];
    all.extend(pages.iter().cloned());
    for path in &all {
        site.respond(
            path,
            Timed {
                arrivals: arrivals.clone(),
                response: html_response(page_html(path, &pages)).set_delay(delay),
            },
        )
        .await;
    }

    let config = PipelineConfig {
        concurrent_per_domain: 1,
        crawler_concurrency: 8,
        ..PipelineConfig::default()
    };
    let p = Pipeline::start(config).await;
    p.submit(&[p.seed(&site.url("/"), "job-polite")]).await;

    let acc = p.wait_balanced(|a, _| a.documents_indexed == 6).await;
    assert_eq!(acc.pages_crawled_ok, 6, "{acc:#?}");

    // Each response takes `delay`; with one slot per domain the next
    // request may only start after the previous response was received.
    let mut starts = arrivals.lock().clone();
    starts.sort();
    assert_eq!(starts.len(), 6);
    for w in starts.windows(2) {
        let gap = w[1] - w[0];
        assert!(
            gap >= delay,
            "two requests to the same domain overlapped (started {gap:?} apart)"
        );
    }
}
