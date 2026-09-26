//! Per-message handler: fetch one `UrlMessage`, classify the result, publish
//! everything that follows from it, and ack only once every publish
//! succeeded.
//!
//! Delivery contract (spec R2, ruling R-5): the handler always ends with an
//! ack once its publishes succeed — retries, dead-lettering and terminal
//! failures are all handled here, so a bad URL never blocks its partition.
//! The single non-ack path is "a publish to the bus failed": the message is
//! left uncommitted and redelivered later.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use scrapix_core::{Ack, CrawlUrl, CrawlerType, RawPage, ScrapixError};
use scrapix_crawler::{
    ConditionalRequestHeaders, ExtractorConfig, FetchOptions, FetchResult, UrlExtractor,
};
use scrapix_queue::{
    topic_names, CrawlEvent, DlqMessage, LinksMessage, RawPageMessage, UrlMessage,
};
use tracing::{debug, info, warn};

use crate::outcome::{classify, is_retryable, Outcome};
use crate::CrawlerWorker;

/// Reason used when a job needs JS rendering and this worker has no browser.
pub(crate) const BROWSER_UNAVAILABLE: &str = "browser rendering unavailable on this worker";

/// Result of one fetch attempt plus the proxy it went through (if any), so
/// the proxy pool can be told whether the proxy worked.
struct FetchAttempt {
    result: scrapix_core::Result<FetchResult>,
    proxy: Option<String>,
}

impl CrawlerWorker {
    /// Handle one URL message end to end. Acks exactly once, after every
    /// publish for the message succeeded; on a publish error the ack is
    /// dropped (message left for redelivery).
    pub(crate) async fn handle_message(self: &Arc<Self>, msg: UrlMessage, ack: Ack) {
        // Task 15: ack messages of cancelled jobs here without doing any work.

        let start = Instant::now();
        self.metrics.fetch_started();
        let (outcome, result) = if self.browser_required_but_unavailable(&msg) {
            (
                Outcome::Failed {
                    reason: BROWSER_UNAVAILABLE.to_string(),
                    status: None,
                },
                None,
            )
        } else {
            let attempt = self.fetch(&msg).await;
            if let Some(ref proxy) = attempt.proxy {
                self.report_proxy(&msg.job_id, proxy, &attempt.result);
            }
            let outcome = classify(&attempt.result, msg.url.retry_count, self.max_retries);
            (outcome, Some(attempt.result))
        };
        self.metrics.fetch_completed();
        let elapsed = start.elapsed();

        let published = match outcome {
            Outcome::Crawled => match result {
                Some(Ok(FetchResult::Fetched(page))) => self.on_crawled(&msg, page, elapsed).await,
                // `classify` only returns `Crawled` for `Ok(Fetched(2xx))`.
                _ => unreachable!("Outcome::Crawled without a fetched page"),
            },
            Outcome::NotModified => self.on_not_modified(&msg).await,
            Outcome::Retry { reason, delay } => self.on_retry(&msg, &reason, delay).await,
            Outcome::Failed { reason, status } => {
                let exhausted = result.as_ref().is_some_and(is_retryable);
                self.on_failed(&msg, &reason, status, exhausted).await
            }
        };

        match published {
            Ok(()) => ack.ack(),
            Err(e) => {
                // Not acked: the offset stays uncommitted and the message is
                // redelivered (after a rebalance/restart).
                warn!(
                    url = %msg.url.url,
                    job_id = %msg.job_id,
                    error = %e,
                    "Failed to publish crawl results; leaving message unacked for redelivery"
                );
            }
        }
    }

    /// The job (or URL) requires JS rendering and this worker cannot do it.
    /// Such URLs fail instead of being silently fetched over plain HTTP.
    fn browser_required_but_unavailable(&self, msg: &UrlMessage) -> bool {
        let required = msg.url.requires_js
            || msg
                .job
                .as_ref()
                .is_some_and(|j| j.crawler_type == CrawlerType::Browser);
        required && !self.has_browser()
    }

    fn has_browser(&self) -> bool {
        #[cfg(feature = "browser")]
        {
            self.browser_renderer.is_some()
        }
        #[cfg(not(feature = "browser"))]
        {
            false
        }
    }

    /// Fetch the URL over the browser (job/URL requires it, or a worker-level
    /// `BROWSER_RENDER_PATTERNS` match) or over HTTP with per-job options.
    async fn fetch(&self, msg: &UrlMessage) -> FetchAttempt {
        let url = &msg.url;

        #[cfg(feature = "browser")]
        if let Some(ref renderer) = self.browser_renderer {
            let job_wants_browser = url.requires_js
                || msg
                    .job
                    .as_ref()
                    .is_some_and(|j| j.crawler_type == CrawlerType::Browser);
            if job_wants_browser || self.browser_patterns.iter().any(|p| p.is_match(&url.url)) {
                debug!(url = %url.url, "Using browser rendering");
                self.metrics.record_browser_render();
                let respect_robots = msg.job.as_ref().map_or(true, |j| j.respect_robots_txt);
                return FetchAttempt {
                    result: renderer
                        .fetch_with_robots(url, respect_robots)
                        .await
                        .map(FetchResult::Fetched),
                    proxy: None,
                };
            }
        }
        let conditional_headers = self.conditional_headers(msg).await;

        // PDF support travels with the UrlMessage so the fetcher can stay a
        // long-lived per-worker singleton while honoring per-job opt-ins.
        let base = match msg.features {
            Some(ref features) if features.is_pdf_enabled() => {
                FetchOptions::with_pdf(features.pdf_max_size_bytes())
            }
            _ => FetchOptions::default(),
        };
        let options = self.shaper.options_for(&msg.job_id, msg.job.as_ref(), base);
        let proxy = options.proxy.clone();

        self.metrics.record_http_fetch();
        let result = self
            .fetcher
            .fetch_conditional_with_options(url, &conditional_headers, options)
            .await;

        if let Some(dns_stats) = self.fetcher.dns_cache_stats() {
            self.metrics
                .record_dns_stats(dns_stats.hits, dns_stats.misses);
        }

        FetchAttempt { result, proxy }
    }

    /// Conditional headers for incremental crawling — only when both the
    /// worker flag and the job's `incremental` flag are on (the Replace
    /// index strategy sets `incremental = false` to force a full re-crawl).
    async fn conditional_headers(&self, msg: &UrlMessage) -> ConditionalRequestHeaders {
        let url = &msg.url;
        if !(self.incremental_crawl && msg.incremental) {
            return ConditionalRequestHeaders::new();
        }
        let mut headers = ConditionalRequestHeaders::new();
        if let Some(ref etag) = url.etag {
            headers = headers.with_etag(etag);
        }
        if let Some(ref last_modified) = url.last_modified {
            headers = headers.with_last_modified(last_modified);
        }
        // No in-message headers: look up the Redis crawl history.
        if !headers.has_headers() {
            if let Some(ref history) = self.crawl_history {
                match history.get(&msg.index_uid, &url.url).await {
                    Ok(Some(record)) => {
                        if let Some(ref etag) = record.etag {
                            headers = headers.with_etag(etag);
                        }
                        if let Some(ref lm) = record.last_modified {
                            headers = headers.with_last_modified(lm);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!(url = %url.url, error = %e, "Failed to look up crawl history");
                    }
                }
            }
        }
        headers
    }

    /// Tell the job's proxy pool whether the proxy worked: any HTTP response
    /// is a success; a transport-level error counts against the proxy.
    fn report_proxy(&self, job_id: &str, proxy: &str, result: &scrapix_core::Result<FetchResult>) {
        match result {
            Ok(_) => self.shaper.report_proxy(job_id, proxy, true),
            Err(
                ScrapixError::Connection(_) | ScrapixError::Timeout(_) | ScrapixError::Network(_),
            ) => self.shaper.report_proxy(job_id, proxy, false),
            Err(_) => {}
        }
    }

    /// 2xx: publish the page, the discovered links and `PageCrawled`.
    async fn on_crawled(
        self: &Arc<Self>,
        msg: &UrlMessage,
        page: RawPage,
        elapsed: Duration,
    ) -> scrapix_core::Result<()> {
        let url = &msg.url;
        let page_size = page.html.len() as u64;
        let status = page.status;
        let js_rendered = page.js_rendered;
        self.metrics.record_success(page_size);

        info!(
            url = %url.url,
            status,
            size_kb = page_size / 1024,
            duration_ms = elapsed.as_millis(),
            js_rendered,
            "Page fetched successfully"
        );

        // Sitemap discovery runs in the background so it never delays the
        // first fetch (Task 7 moves it to a per-job flow).
        if self.sitemap_parser.is_some() {
            if let Some(domain) = url::Url::parse(&url.url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
            {
                let worker = self.clone();
                let parent = msg.clone();
                tokio::spawn(async move {
                    if let Err(e) = worker.maybe_discover_sitemaps(&domain, &parent).await {
                        debug!(domain, error = %e, "Sitemap discovery failed");
                    }
                });
            }
        }

        let discovered_urls = self.extract_links(msg, &page);
        let discovered_count = discovered_urls.len();
        self.metrics.record_discovered(discovered_count as u64);

        let target_urls: Vec<String> = discovered_urls.iter().map(|u| u.url.clone()).collect();
        if let Some(ref graph) = self.link_graph {
            let target_refs: Vec<&str> = target_urls.iter().map(|s| s.as_str()).collect();
            graph.record_links(&url.url, target_refs);
            let processed = self.metrics.urls_processed.load(Ordering::Relaxed);
            if processed > 0 && processed % self.link_graph_interval == 0 {
                graph.compute_scores_if_dirty();
                debug!(processed, "Recomputed link graph scores");
            }
        }

        // Best effort: centralized PageRank input, not part of the page's
        // delivery guarantee.
        if self.publish_links && !target_urls.is_empty() {
            let links_msg = LinksMessage::new(&url.url, target_urls, &msg.job_id);
            if let Err(e) = self
                .producer
                .send(topic_names::LINKS, Some(&msg.job_id), &links_msg)
                .await
            {
                debug!(error = %e, "Failed to publish links to frontier");
            }
        }

        let etag = page.headers.get("etag").cloned();
        let last_modified = page.headers.get("last-modified").cloned();

        // Save crawl history for future incremental crawls (Update strategy only).
        if msg.incremental && (etag.is_some() || last_modified.is_some()) {
            if let Some(ref history) = self.crawl_history {
                if let Err(e) = history
                    .save(
                        &msg.index_uid,
                        &url.url,
                        etag.clone(),
                        last_modified.clone(),
                    )
                    .await
                {
                    debug!(url = %url.url, error = %e, "Failed to save crawl history to Redis");
                }
            }
        }

        let raw_page_msg = RawPageMessage::from_url_message(msg, page, etag, last_modified);
        self.producer
            .send(topic_names::PAGES_RAW, Some(&msg.job_id), &raw_page_msg)
            .await?;

        for mut discovered in discovered_urls {
            if let Some(ref graph) = self.link_graph {
                discovered.priority += graph.get_priority_boost(&discovered.url);
            }
            let child = msg.child(discovered);
            self.producer
                .send(
                    topic_names::URL_FRONTIER,
                    Some(&child.partition_key()),
                    &child,
                )
                .await?;
        }

        if discovered_count > 0 {
            let event = CrawlEvent::UrlsDiscovered {
                job_id: msg.job_id.clone(),
                source_url: url.url.clone(),
                count: discovered_count,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            self.publish_event(&msg.job_id, &event).await?;
        }

        let event = CrawlEvent::PageCrawled {
            job_id: msg.job_id.clone(),
            account_id: msg.account_id.clone(),
            url: url.url.clone(),
            status,
            content_length: page_size,
            duration_ms: elapsed.as_millis() as u64,
            timestamp: chrono::Utc::now().timestamp_millis(),
            links_published: discovered_count as u64,
            url_message_id: msg.message_id.clone(),
            js_rendered,
        };
        self.publish_event(&msg.job_id, &event).await?;

        // Task 12: publish FetchFeedback (politeness slot release) here.
        Ok(())
    }

    /// Extract links with the job's URL patterns and per-job `max_depth`.
    fn extract_links(&self, msg: &UrlMessage, page: &RawPage) -> Vec<CrawlUrl> {
        let depth = msg.url.depth;
        let effective_max_depth = msg.max_depth.unwrap_or(self.extractor.config().max_depth);
        if let Some(ref patterns) = msg.url_patterns {
            // With an allowed_domains whitelist, use strict domain filtering.
            let extractor = UrlExtractor::new(ExtractorConfig {
                patterns: Some(patterns.clone()),
                max_depth: effective_max_depth,
                follow_external: false,
                follow_subdomains: patterns.allowed_domains.is_empty(),
                extract_from_data_attrs: false,
                allowed_domains: patterns.allowed_domains.clone(),
            });
            extractor.extract(page, depth)
        } else if msg.max_depth.is_some() {
            let mut config = self.extractor.config().clone();
            config.max_depth = effective_max_depth;
            UrlExtractor::new(config).extract(page, depth)
        } else {
            self.extractor.extract(page, depth)
        }
    }

    /// 304 under incremental crawling.
    async fn on_not_modified(&self, msg: &UrlMessage) -> scrapix_core::Result<()> {
        self.metrics.record_not_modified();
        debug!(url = %msg.url.url, "Page not modified (304), skipping processing");
        let event = CrawlEvent::PageSkipped {
            job_id: msg.job_id.clone(),
            url: msg.url.url.clone(),
            reason: "304 Not Modified".to_string(),
            timestamp: chrono::Utc::now().timestamp_millis(),
            url_message_id: msg.message_id.clone(),
        };
        self.publish_event(&msg.job_id, &event).await?;
        // Task 12: publish FetchFeedback (politeness slot release) here.
        Ok(())
    }

    /// Transient failure: re-queue with `retry_count + 1` and a
    /// `not_before_ms` backoff, then publish `PageRetried`.
    async fn on_retry(
        &self,
        msg: &UrlMessage,
        reason: &str,
        delay: Duration,
    ) -> scrapix_core::Result<()> {
        self.metrics.record_retry();
        let mut url = msg.url.clone();
        url.retry_count += 1;
        url.not_before_ms = Some(chrono::Utc::now().timestamp_millis() + delay.as_millis() as i64);
        let retry_count = url.retry_count;
        let retry = msg.child(url);

        info!(
            url = %msg.url.url,
            job_id = %msg.job_id,
            retry_count,
            delay_secs = delay.as_secs(),
            reason,
            "Re-queueing URL for retry"
        );

        self.producer
            .send(
                topic_names::URL_FRONTIER,
                Some(&retry.partition_key()),
                &retry,
            )
            .await?;

        let event = CrawlEvent::PageRetried {
            job_id: msg.job_id.clone(),
            url: msg.url.url.clone(),
            url_message_id: msg.message_id.clone(),
            retry_count,
            error: reason.to_string(),
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        self.publish_event(&msg.job_id, &event).await?;
        // Task 12: publish FetchFeedback (politeness slot release) here.
        Ok(())
    }

    /// Terminal failure: dead-letter it when the retry budget was exhausted,
    /// then publish `PageFailed` (carrying the status, if any).
    async fn on_failed(
        &self,
        msg: &UrlMessage,
        reason: &str,
        status: Option<u16>,
        retries_exhausted: bool,
    ) -> scrapix_core::Result<()> {
        self.metrics.record_failure();
        warn!(
            url = %msg.url.url,
            job_id = %msg.job_id,
            status,
            retry_count = msg.url.retry_count,
            reason,
            "URL failed"
        );

        if retries_exhausted {
            let mut dlq = DlqMessage::new(
                serde_json::to_string(msg)?,
                topic_names::URL_PROCESSING,
                reason,
            )
            .with_job_id(&msg.job_id);
            dlq.retry_count = msg.url.retry_count;
            self.producer
                .send(topic_names::DLQ_URLS, Some(&msg.job_id), &dlq)
                .await?;
        }

        let event = CrawlEvent::PageFailed {
            job_id: msg.job_id.clone(),
            account_id: msg.account_id.clone(),
            url: msg.url.url.clone(),
            error: reason.to_string(),
            retry_count: msg.url.retry_count,
            timestamp: chrono::Utc::now().timestamp_millis(),
            status,
            url_message_id: msg.message_id.clone(),
        };
        self.publish_event(&msg.job_id, &event).await?;
        // Task 12: publish FetchFeedback (politeness slot release) here.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Args;
    use clap::Parser;
    use scrapix_core::JobSpec;
    use scrapix_queue::{AnyConsumer, AnyProducer, ChannelBus};
    use serde::de::DeserializeOwned;
    use std::sync::atomic::AtomicBool;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MAX_RETRIES: u32 = 2;

    async fn worker(bus: &ChannelBus) -> Arc<CrawlerWorker> {
        let mut args = Args::parse_from(["scrapix-worker-crawler"]);
        args.allow_private_ips = true; // wiremock listens on 127.0.0.1
        args.respect_robots = false;
        args.sitemap_discovery = false;
        args.redis_url = None;
        args.browser_render = false;
        args.max_retries = MAX_RETRIES;
        Arc::new(
            CrawlerWorker::build(
                &args,
                "test".into(),
                AnyConsumer::from(bus.consumer()),
                AnyProducer::from(bus.producer()),
            )
            .await
            .unwrap(),
        )
    }

    fn reader(bus: &ChannelBus, topic: &str) -> AnyConsumer {
        let c = AnyConsumer::from(bus.consumer());
        c.subscribe(&[topic]).unwrap();
        c
    }

    async fn drain<T: DeserializeOwned + Send>(c: &AnyConsumer) -> Vec<T> {
        let mut out = Vec::new();
        while let Some(m) = c.poll_one::<T>(Duration::from_millis(50)).await.unwrap() {
            out.push(m);
        }
        out
    }

    fn tracked_ack() -> (Ack, Arc<AtomicBool>) {
        let acked = Arc::new(AtomicBool::new(false));
        let flag = acked.clone();
        (
            Ack::from_fn(move || flag.store(true, Ordering::SeqCst)),
            acked,
        )
    }

    fn url(server: &MockServer, p: &str) -> String {
        format!("{}{}", server.uri().replace("127.0.0.1", "localhost"), p)
    }

    fn message(target: String, job: Option<JobSpec>) -> UrlMessage {
        UrlMessage::new(CrawlUrl::seed(target), "job-1", "idx")
            .account("acct")
            .with_job(job)
    }

    struct Topics {
        pages: AnyConsumer,
        frontier: AnyConsumer,
        events: AnyConsumer,
        dlq: AnyConsumer,
    }

    fn topics(bus: &ChannelBus) -> Topics {
        Topics {
            pages: reader(bus, topic_names::PAGES_RAW),
            frontier: reader(bus, topic_names::URL_FRONTIER),
            events: reader(bus, topic_names::EVENTS),
            dlq: reader(bus, topic_names::DLQ_URLS),
        }
    }

    #[tokio::test]
    async fn success_publishes_page_links_and_event_then_acks() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"<html><body><a href="/next">next</a></body></html>"#,
                "text/html",
            ))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let job = JobSpec {
            user_agents: vec!["UA".into()],
            ..Default::default()
        };
        let msg = message(url(&server, "/"), Some(job.clone()));
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;

        assert!(acked.load(Ordering::SeqCst));
        let pages: Vec<RawPageMessage> = drain(&t.pages).await;
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].status, 200);
        assert_eq!(pages[0].url_message_id, msg.message_id);
        assert_eq!(pages[0].job.as_ref(), Some(&job));

        let children: Vec<UrlMessage> = drain(&t.frontier).await;
        assert_eq!(children.len(), 1);
        assert!(children[0].url.url.ends_with("/next"));
        assert_eq!(children[0].job.as_ref(), Some(&job));
        assert_eq!(children[0].account_id.as_deref(), Some("acct"));
        assert_ne!(children[0].message_id, msg.message_id);

        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(events.iter().any(|e| matches!(
            e,
            CrawlEvent::PageCrawled { links_published: 1, js_rendered: false, url_message_id, status: 200, .. }
                if *url_message_id == msg.message_id
        )));
        assert!(drain::<DlqMessage>(&t.dlq).await.is_empty());
    }

    #[tokio::test]
    async fn not_found_fails_with_status_and_is_not_indexed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404).set_body_raw("nope", "text/html"))
            .expect(1)
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let msg = message(url(&server, "/missing"), None);
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;

        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        assert!(drain::<UrlMessage>(&t.frontier).await.is_empty());
        assert!(drain::<DlqMessage>(&t.dlq).await.is_empty());
        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                CrawlEvent::PageFailed { status: Some(404), url_message_id, account_id: Some(_), .. }
                    if *url_message_id == msg.message_id
            )),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn unavailable_is_requeued_with_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/busy"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "120"))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let job = JobSpec {
            headers: [("X-Job".to_string(), "1".to_string())].into(),
            ..Default::default()
        };
        let msg = message(url(&server, "/busy"), Some(job.clone()));
        let (ack, acked) = tracked_ack();
        let before = chrono::Utc::now().timestamp_millis();

        w.handle_message(msg.clone(), ack).await;

        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        let requeued: Vec<UrlMessage> = drain(&t.frontier).await;
        assert_eq!(requeued.len(), 1);
        let r = &requeued[0];
        assert_eq!(r.url.url, msg.url.url);
        assert_eq!(r.url.retry_count, 1);
        assert_eq!(r.job.as_ref(), Some(&job));
        assert_ne!(r.message_id, msg.message_id);
        let not_before = r.url.not_before_ms.expect("not_before_ms set");
        assert!(not_before >= before + 119_000, "{not_before} vs {before}");

        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(events.iter().any(|e| matches!(
            e,
            CrawlEvent::PageRetried { retry_count: 1, url_message_id, .. }
                if *url_message_id == msg.message_id
        )));
        assert!(!events
            .iter()
            .any(|e| matches!(e, CrawlEvent::PageFailed { .. })));
        assert!(drain::<DlqMessage>(&t.dlq).await.is_empty());
    }

    #[tokio::test]
    async fn exhausted_retries_are_dead_lettered() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/down"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let mut msg = message(url(&server, "/down"), None);
        msg.url.retry_count = MAX_RETRIES;
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;

        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<UrlMessage>(&t.frontier).await.is_empty());
        let dlq: Vec<DlqMessage> = drain(&t.dlq).await;
        assert_eq!(dlq.len(), 1);
        assert_eq!(dlq[0].original_topic, topic_names::URL_PROCESSING);
        assert_eq!(dlq[0].job_id.as_deref(), Some("job-1"));
        assert_eq!(dlq[0].retry_count, MAX_RETRIES);
        let original: UrlMessage = serde_json::from_str(&dlq[0].original_message).unwrap();
        assert_eq!(original.message_id, msg.message_id);

        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(events.iter().any(|e| matches!(
            e,
            CrawlEvent::PageFailed {
                status: Some(503),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn job_headers_and_user_agent_reach_the_origin() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/h"))
            .and(header("user-agent", "JobBot/2"))
            .and(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>ok</p>", "text/html"))
            .expect(1)
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let job = JobSpec {
            user_agents: vec!["JobBot/2".into()],
            headers: [("X-Api-Key".to_string(), "k".to_string())].into(),
            ..Default::default()
        };
        let (ack, acked) = tracked_ack();

        w.handle_message(message(url(&server, "/h"), Some(job)), ack)
            .await;

        assert!(acked.load(Ordering::SeqCst));
        assert_eq!(drain::<RawPageMessage>(&t.pages).await.len(), 1);
    }

    #[tokio::test]
    async fn browser_job_without_renderer_fails_instead_of_http() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>ok</p>", "text/html"))
            .expect(0)
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let job = JobSpec {
            crawler_type: CrawlerType::Browser,
            ..Default::default()
        };
        let (ack, acked) = tracked_ack();

        w.handle_message(message(url(&server, "/spa"), Some(job)), ack)
            .await;

        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(events.iter().any(|e| matches!(
            e,
            CrawlEvent::PageFailed { error, status: None, .. } if error == BROWSER_UNAVAILABLE
        )));
        assert!(drain::<DlqMessage>(&t.dlq).await.is_empty());
    }
}
