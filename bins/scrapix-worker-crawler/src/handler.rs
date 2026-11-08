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

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use scrapix_core::url_normalize::normalize_entry_url;
use scrapix_core::{Ack, CrawlUrl, CrawlerType, RawPage, ScrapixError};
use scrapix_crawler::{
    url_allowed_any_scheme_or_www, ConditionalRequestHeaders, ExtractorConfig, FetchOptions,
    FetchResult, UrlExtractor, MAX_STORED_LINKS,
};
use scrapix_queue::{
    topic_names, CrawlEvent, DlqMessage, FetchFeedback, LinksMessage, RawPageMessage, UrlMessage,
};
use tracing::{debug, info, warn};

use crate::job_fetch::{headers_allowed_for, is_proxy_failure};
use crate::outcome::{classify, is_retryable, Outcome};
use crate::page_history::PageRecord;
use crate::CrawlerWorker;

/// `PageSkipped` reason of a page that answered 304 to an incremental crawl.
pub(crate) const NOT_MODIFIED: &str = "304 Not Modified";

/// Accounting fields of a `PageSkipped` event.
#[derive(Debug, Default, Clone, Copy)]
struct Skip {
    links_published: u64,
    not_modified: bool,
    sitemap_pending: bool,
}

/// Reason used when a job needs JS rendering and this worker has no browser.
pub(crate) const BROWSER_UNAVAILABLE: &str = "browser rendering unavailable on this worker";

/// Reason used when a browser job sets a proxy: the worker's browser is
/// shared by every job and has a single worker-level proxy, so a per-job
/// proxy cannot be honored and the job fails closed instead of connecting
/// directly. (Same text as `scrapix_crawler::BROWSER_PROXY_UNSUPPORTED`,
/// which only exists with the `browser` feature.)
pub(crate) const BROWSER_PROXY_UNSUPPORTED: &str =
    "per-job proxy is not supported for browser rendering";

/// Upper bound on the best-effort `FetchFeedback` publish.
const FEEDBACK_SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Build the `FetchFeedback` for one handled message (`crawl_delay_ms` is
/// filled in by the caller). `result` is `None` when no fetch was attempted
/// (fail-closed precheck). Transport errors through a job proxy are the
/// proxy's, not the domain's, so they are not flagged as `transport_error`.
fn fetch_feedback(
    msg: &UrlMessage,
    result: Option<&scrapix_core::Result<FetchResult>>,
    via_proxy: bool,
) -> FetchFeedback {
    let mut fb = FetchFeedback::new(
        scrapix_frontier::extract_domain(&msg.url.url),
        msg.job_id.clone(),
        msg.message_id.clone(),
        msg.url.url.clone(),
    );
    match result {
        None => {}
        Some(Ok(FetchResult::Fetched(page))) => {
            fb.status = Some(page.status);
            if matches!(page.status, 429 | 503) {
                fb.retry_after_ms = page
                    .headers
                    .get("retry-after")
                    .and_then(|v| scrapix_crawler::parse_retry_after(v, chrono::Utc::now()))
                    .map(|d| d.as_millis() as u64);
            }
        }
        Some(Ok(FetchResult::NotModified { .. })) => fb.status = Some(304),
        Some(Err(ScrapixError::Http { status, .. })) => fb.status = Some(*status),
        Some(Err(ScrapixError::RateLimited { retry_after_secs })) => {
            fb.status = Some(429);
            fb.retry_after_ms = Some(retry_after_secs.saturating_mul(1000));
        }
        Some(Err(
            ScrapixError::Timeout(_) | ScrapixError::Connection(_) | ScrapixError::Network(_),
        )) => fb.transport_error = !via_proxy,
        // Robots disallow, SSRF refusal, invalid URL, ...: no page request.
        Some(Err(_)) => {}
    }
    fb
}

/// Result of one fetch attempt plus the proxy it went through (if any), so
/// the proxy pool can be told whether the proxy worked.
struct FetchAttempt {
    result: scrapix_core::Result<FetchResult>,
    proxy: Option<String>,
}

/// The job (`crawler_type: browser`) or the URL (`requires_js`) needs JS
/// rendering.
fn requires_browser(msg: &UrlMessage) -> bool {
    msg.url.requires_js
        || msg
            .job
            .as_ref()
            .is_some_and(|j| j.crawler_type == CrawlerType::Browser)
}

impl CrawlerWorker {
    /// Handle one URL message end to end. Acks exactly once, after every
    /// publish for the message succeeded; on a publish error the ack is
    /// dropped (message left for redelivery).
    pub(crate) async fn handle_message(self: &Arc<Self>, msg: UrlMessage, ack: Ack) {
        // R5: a cancelled/finished job's message is acked without any work,
        // but its dispatch still holds a politeness slot in the frontier:
        // free it (status None, no request made) before acking.
        if self.cancelled.contains(&msg.job_id) {
            debug!(url = %msg.url.url, job_id = %msg.job_id, "Skipping message of a cancelled job");
            self.publish_feedback(&msg, None).await;
            ack.ack();
            return;
        }

        let start = Instant::now();
        self.metrics.fetch_started();
        let (outcome, result) = if let Some(reason) = self.precheck(&msg) {
            (
                Outcome::Failed {
                    reason: reason.to_string(),
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
        self.metrics.observe_fetch_duration(elapsed);

        // Release the frontier's politeness slot for this dispatch (every
        // handled message, including fail-closed ones that made no request).
        self.publish_feedback(&msg, result.as_ref()).await;

        let published = match outcome {
            Outcome::Crawled => match result {
                Some(Ok(FetchResult::Fetched(page))) => self.on_fetched(&msg, page, elapsed).await,
                // `classify` only returns `Crawled` for `Ok(Fetched(2xx))`;
                // never panic here (a panicking handler would lose the ack).
                _ => {
                    self.on_failed(
                        &msg,
                        "internal error: crawled outcome without a fetched page",
                        None,
                        false,
                    )
                    .await
                }
            },
            Outcome::NotModified => self.on_not_modified(&msg).await,
            Outcome::Retry { reason, delay } => self.on_retry(&msg, &reason, delay).await,
            Outcome::Failed { reason, status } => {
                let exhausted = result.as_ref().is_some_and(is_retryable);
                self.on_failed(&msg, &reason, status, exhausted).await
            }
            Outcome::Skipped { reason } => {
                self.metrics.record_skipped();
                info!(url = %msg.url.url, job_id = %msg.job_id, reason, "URL skipped");
                self.on_skipped(&msg, &reason, Skip::default()).await
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

    /// Publish the `FetchFeedback` for `msg` (keyed by domain). Best-effort:
    /// a failed or slow publish is logged and never blocks the ack — the
    /// frontier's slot expiry covers lost feedback.
    async fn publish_feedback(
        &self,
        msg: &UrlMessage,
        result: Option<&scrapix_core::Result<FetchResult>>,
    ) {
        let via_proxy = msg.job.as_ref().is_some_and(|j| j.proxy.is_some());
        let mut feedback = fetch_feedback(msg, result, via_proxy);
        let respects_robots = !matches!(msg.job, Some(ref j) if !j.respect_robots_txt);
        if respects_robots && result.is_some() {
            if let Some(delay) = self.fetcher.cached_crawl_delay(&msg.url.url) {
                feedback.robots_checked = true;
                feedback.crawl_delay_ms = delay;
            }
        }
        let send = self.producer.send(
            topic_names::FETCH_FEEDBACK,
            Some(&feedback.domain),
            &feedback,
        );
        match tokio::time::timeout(FEEDBACK_SEND_TIMEOUT, send).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!(url = %msg.url.url, error = %e, "Failed to publish fetch feedback")
            }
            Err(_) => warn!(url = %msg.url.url, "Timed out publishing fetch feedback"),
        }
    }

    /// Terminal failures decided before fetching. A job (or URL) that
    /// requires JS rendering fails instead of being silently fetched over
    /// plain HTTP when:
    /// - it also sets a per-job proxy (the shared browser cannot use it), or
    /// - this worker has no browser renderer.
    fn precheck(&self, msg: &UrlMessage) -> Option<&'static str> {
        if !requires_browser(msg) {
            return None;
        }
        if msg.job.as_ref().is_some_and(|j| j.proxy.is_some()) {
            return Some(BROWSER_PROXY_UNSUPPORTED);
        }
        if !self.has_browser() {
            return Some(BROWSER_UNAVAILABLE);
        }
        None
    }

    fn has_browser(&self) -> bool {
        #[cfg(feature = "browser")]
        {
            self.browser_renderer.is_some() || self.lightpanda.is_some()
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

        // PDF/document support travels with the UrlMessage so the fetcher
        // can stay a long-lived per-worker singleton while honoring per-job
        // opt-ins.
        let base = match msg.features {
            Some(ref features) => FetchOptions::for_features(features),
            None => FetchOptions::default(),
        };
        let mut options = self.shaper.options_for(&msg.job_id, msg.job.as_ref(), base);
        let allowed_domains = msg
            .url_patterns
            .as_ref()
            .map(|p| p.allowed_domains.as_slice())
            .unwrap_or_default();
        if !options.extra_headers.is_empty() && !headers_allowed_for(&url.url, allowed_domains) {
            debug!(url = %url.url, "Host not in allowed_domains; not sending job headers");
            options.extra_headers.clear();
        }

        #[cfg(feature = "browser")]
        if self.has_browser() {
            // A worker-level pattern match only routes to the browser when the
            // job has no proxy (the browser cannot honor it; HTTP can).
            let job_has_proxy = msg.job.as_ref().is_some_and(|j| j.proxy.is_some());
            let pattern_match =
                !job_has_proxy && self.browser_patterns.iter().any(|p| p.is_match(&url.url));
            if requires_browser(msg) || pattern_match {
                return FetchAttempt {
                    result: self.browser_fetch(url, &options).await,
                    proxy: None,
                };
            }
        }

        // The job configured proxies but all of them are in cooldown: never
        // fall back to a direct connection. Transient → re-queued.
        if msg.job.as_ref().is_some_and(|j| j.proxy.is_some()) && options.proxy.is_none() {
            return FetchAttempt {
                result: Err(ScrapixError::Connection(
                    "no proxy available for this job (all proxies in cooldown)".to_string(),
                )),
                proxy: None,
            };
        }

        let conditional_headers = self.conditional_headers(msg).await;
        let proxy = options.proxy.clone();

        self.metrics.record_http_fetch();
        let mut result = self
            .fetcher
            .fetch_conditional_with_options(url, &conditional_headers, options)
            .await;

        // A proxy-generated error response (407, or 5xx with Proxy-Status)
        // is the proxy's failure, not the origin's: make it a transient error
        // so the URL is retried (through another proxy) instead of failing.
        if proxy.is_some() {
            if let Ok(FetchResult::Fetched(ref page)) = result {
                if is_proxy_failure(page) {
                    result = Err(ScrapixError::Connection(format!(
                        "proxy error: HTTP {}",
                        page.status
                    )));
                }
            }
        }

        if let Some(dns_stats) = self.fetcher.dns_cache_stats() {
            self.metrics
                .record_dns_stats(dns_stats.hits, dns_stats.misses);
        }

        FetchAttempt { result, proxy }
    }

    /// Render `url` in a browser: Lightpanda first when configured, Chrome
    /// when Lightpanda fails (except on a refusal, which is final) or the
    /// page looks unrendered. Without Chrome, Lightpanda's answer stands.
    #[cfg(feature = "browser")]
    async fn browser_fetch(
        &self,
        url: &scrapix_core::CrawlUrl,
        options: &scrapix_crawler::FetchOptions,
    ) -> scrapix_core::Result<FetchResult> {
        let chrome = self.browser_renderer.as_ref();
        if let Some(lightpanda) = &self.lightpanda {
            self.metrics.record_lightpanda_render();
            match lightpanda.fetch_with_options(url, options).await {
                Ok(render) => match render.fallback_reason() {
                    Some(reason) if chrome.is_some() => {
                        debug!(url = %url.url, reason, "Lightpanda render rejected; re-rendering in Chrome");
                        self.metrics.record_lightpanda_fallback();
                    }
                    _ => {
                        debug!(url = %url.url, "Rendered with Lightpanda");
                        return Ok(FetchResult::Fetched(render.raw_page(url)));
                    }
                },
                Err(e) if chrome.is_none() || !scrapix_crawler::should_retry_in_chrome(&e) => {
                    return Err(e);
                }
                Err(e) => {
                    debug!(url = %url.url, error = %e, "Lightpanda render failed; re-rendering in Chrome");
                    self.metrics.record_lightpanda_fallback();
                }
            }
        }
        let Some(chrome) = chrome else {
            return Err(ScrapixError::Config(BROWSER_UNAVAILABLE.to_string()));
        };
        debug!(url = %url.url, "Using browser rendering");
        self.metrics.record_browser_render();
        chrome
            .fetch_with_options(url, options)
            .await
            .map(FetchResult::Fetched)
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
        // No in-message headers: look up the page history. A record without
        // stored links, or stored under other link filters (another job, a
        // widened config, a record older than scope keys), is not used: a
        // 304 would replay the wrong links. The full fetch rewrites it.
        if !headers.has_headers() {
            if let Some(ref history) = self.crawl_history {
                match history.get(&msg.index_uid, &url.url).await {
                    Ok(Some(record))
                        if record.usable_for_conditional_request()
                            && record.link_scope.as_deref()
                                == Some(self.link_scope_key(msg).as_str()) =>
                    {
                        if let Some(ref etag) = record.etag {
                            headers = headers.with_etag(etag);
                        }
                        if let Some(ref lm) = record.last_modified {
                            headers = headers.with_last_modified(lm);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        debug!(url = %url.url, error = %e, "Failed to look up crawl history");
                    }
                }
            }
        }
        headers
    }

    /// Tell the job's proxy pool whether the proxy worked: an origin HTTP
    /// response is a success; a transport-level error, or a proxy-generated
    /// error response (turned into `Connection` in [`Self::fetch`]), counts
    /// against the proxy so it enters cooldown.
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

        // Whether *this* message is the one that owns (job, domain)'s
        // one-time sitemap discovery (R-18). This must be decided
        // synchronously, before `PageCrawled` is published below, because
        // `PageCrawled.sitemap_pending` tells job-completion accounting
        // whether to wait for a matching `SitemapPublished` — and because
        // deciding is itself the (one-shot) dedup side effect, so it must
        // happen at most once per page.
        let sitemap_pending = self.start_sitemap_discovery(msg, &page.final_url);

        let (discovered_urls, outlinks) = self.extract_links(msg, &page);
        let page_final_url = page.final_url.clone();
        let discovered_count = discovered_urls.len();
        self.metrics.record_discovered(discovered_count as u64);

        let target_urls: Vec<String> = discovered_urls.iter().map(|u| u.url.clone()).collect();
        if let Some(ref graph) = self.link_graph {
            let target_refs: Vec<&str> = target_urls.iter().map(|s| s.as_str()).collect();
            graph.record_links(&url.url, target_refs);
            let processed = self.metrics.urls_processed.load(Ordering::Relaxed);
            if processed > 0 && processed.is_multiple_of(self.link_graph_interval) {
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

        // Save the page history for future incremental crawls (Update
        // strategy only), with the page's in-scope links and final URL so a
        // later 304 can still follow them.
        if msg.incremental && (etag.is_some() || last_modified.is_some()) {
            if let Some(ref history) = self.crawl_history {
                let record = PageRecord {
                    etag: etag.clone(),
                    last_modified: last_modified.clone(),
                    crawled_at: chrono::Utc::now().timestamp_millis(),
                    final_url: Some(page_final_url),
                    link_scope: Some(self.link_scope_key(msg)),
                    outlinks: Some(outlinks),
                };
                if let Err(e) = history.save(&msg.index_uid, &url.url, &record).await {
                    debug!(url = %url.url, error = %e, "Failed to save crawl history to Redis");
                }
            }
        }

        let raw_page_msg = RawPageMessage::from_url_message(msg, page, etag, last_modified);
        self.producer
            .send(topic_names::PAGES_RAW, Some(&msg.job_id), &raw_page_msg)
            .await?;

        self.publish_links(msg, discovered_urls).await?;

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
            sitemap_pending,
        };
        self.publish_event(&msg.job_id, &event).await?;

        Ok(())
    }

    /// Extract links with the job's URL patterns and per-job `max_depth`.
    /// Links to documents the job parses (`features.pdf`,
    /// `features.documents`) are followed; a PDF's own hyperlinks are
    /// followed when the job sets `features.pdf.extract_links`.
    ///
    /// Also returns the links to store in the page history: the in-scope
    /// links (the job's domain, include/exclude and non-page filters),
    /// whatever the depth, at most [`MAX_STORED_LINKS`].
    fn extract_links(&self, msg: &UrlMessage, page: &RawPage) -> (Vec<CrawlUrl>, Vec<String>) {
        let depth = msg.url.depth;
        let extractor = self.job_extractor(msg);

        let content_type = page.content_type.as_deref().unwrap_or_default();
        if !scrapix_core::content_types::is_binary_document(content_type) {
            return extractor.extract_with_outlinks(page, depth);
        }
        // A binary document: its body is base64, not HTML. Only PDFs carry
        // followable links, and only when the job asks for them.
        if !msg.features.as_ref().is_some_and(|f| f.pdf_extract_links()) {
            return (Vec::new(), Vec::new());
        }
        let Ok(bytes) = BASE64.decode(page.html.as_bytes()) else {
            return (Vec::new(), Vec::new());
        };
        if scrapix_parser::detect_kind(Some(content_type), &bytes)
            != Some(scrapix_parser::DocumentKind::Pdf)
        {
            return (Vec::new(), Vec::new());
        }
        match scrapix_parser::pdf::extract_links(&bytes, &page.final_url) {
            Ok(links) => {
                let discovered = extractor.extract_from_links(&page.final_url, links, depth);
                let stored = discovered
                    .iter()
                    .take(MAX_STORED_LINKS)
                    .map(|u| u.url.clone())
                    .collect();
                (discovered, stored)
            }
            Err(e) => {
                debug!(url = %page.final_url, error = %e, "PDF link extraction failed");
                (Vec::new(), Vec::new())
            }
        }
    }

    /// The link extractor for `msg`'s job: its URL patterns, `max_depth`
    /// and document opt-ins.
    fn job_extractor(&self, msg: &UrlMessage) -> UrlExtractor {
        let (config, follow_pdf, follow_documents) = self.job_extractor_config(msg);
        UrlExtractor::new(config).follow_document_links(follow_pdf, follow_documents)
    }

    /// The extractor configuration of `msg`'s job, with its PDF and office
    /// document link opt-ins.
    fn job_extractor_config(&self, msg: &UrlMessage) -> (ExtractorConfig, bool, bool) {
        let effective_max_depth = msg.max_depth.unwrap_or(self.extractor.config().max_depth);
        let (follow_pdf, follow_documents) = msg.features.as_ref().map_or((false, false), |f| {
            (f.is_pdf_enabled(), f.is_documents_enabled())
        });
        let config = if let Some(ref patterns) = msg.url_patterns {
            // With an allowed_domains whitelist, use strict domain filtering.
            ExtractorConfig {
                patterns: Some(patterns.clone()),
                max_depth: effective_max_depth,
                follow_external: false,
                follow_subdomains: patterns.allowed_domains.is_empty(),
                extract_from_data_attrs: false,
                allowed_domains: patterns.allowed_domains.clone(),
            }
        } else {
            let mut config = self.extractor.config().clone();
            config.max_depth = effective_max_depth;
            config
        };
        (config, follow_pdf, follow_documents)
    }

    /// Stable key of the filters `msg`'s job applies to a page's links
    /// (allowed domains, include/exclude patterns, external/subdomain
    /// following, document link opt-ins, PDF link extraction), not of
    /// `max_depth` or other per-run settings. A page's stored links are only
    /// replayed on a 304 for a job with the same key: a job with other
    /// filters (a widened saved config) fetches the page in full instead.
    fn link_scope_key(&self, msg: &UrlMessage) -> String {
        let (config, follow_pdf, follow_documents) = self.job_extractor_config(msg);
        let sorted = |v: &[String]| {
            let mut v = v.to_vec();
            v.sort();
            v.dedup();
            v
        };
        let (include, exclude) = config
            .patterns
            .as_ref()
            .map(|p| (sorted(&p.include), sorted(&p.exclude)))
            .unwrap_or_default();
        let pdf_links = msg.features.as_ref().is_some_and(|f| f.pdf_extract_links());
        let canonical = format!(
            "links-v1|include={include:?}|exclude={exclude:?}|domains={:?}|external={}|subdomains={}|data_attrs={}|pdf={follow_pdf}|documents={follow_documents}|pdf_links={pdf_links}",
            sorted(&config.allowed_domains),
            config.follow_external,
            config.follow_subdomains,
            config.extract_from_data_attrs,
        );
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, canonical.as_bytes())
            .simple()
            .to_string()
    }

    /// 304 under incremental crawling: the page is unchanged and already
    /// indexed, but its links are followed again from the page history
    /// (filtered by this job's rules), so the crawl still reaches the pages
    /// below it. Also starts the domain's sitemap discovery, like a crawled
    /// page would.
    async fn on_not_modified(self: &Arc<Self>, msg: &UrlMessage) -> scrapix_core::Result<()> {
        self.metrics.record_not_modified();
        let record = match self.crawl_history {
            Some(ref history) => match history.get(&msg.index_uid, &msg.url.url).await {
                Ok(record) => record,
                Err(e) => {
                    warn!(url = %msg.url.url, error = %e, "Failed to read the stored links of a 304 page");
                    None
                }
            },
            None => None,
        };
        let page_url = record
            .as_ref()
            .and_then(|r| r.final_url.clone())
            .unwrap_or_else(|| msg.url.url.clone());
        let links = self.stored_links(msg, record);
        debug!(url = %msg.url.url, links = links.len(), "Page not modified (304), following its stored links");
        let sitemap_pending = self.start_sitemap_discovery(msg, &page_url);
        let links_published = links.len() as u64;
        self.publish_links(msg, links).await?;
        self.on_skipped(
            msg,
            NOT_MODIFIED,
            Skip {
                links_published,
                not_modified: true,
                sitemap_pending,
            },
        )
        .await
    }

    /// The links to follow from a 304 page's history `record`, re-filtered by
    /// `msg`'s job. They are judged from the page's final URL (where the
    /// first fetch landed, possibly another host after a redirect), not from
    /// the requested URL.
    fn stored_links(&self, msg: &UrlMessage, record: Option<PageRecord>) -> Vec<CrawlUrl> {
        let Some(record) = record else {
            return Vec::new();
        };
        let page_url = record.final_url.unwrap_or_else(|| msg.url.url.clone());
        self.job_extractor(msg).extract_from_links(
            &page_url,
            record.outlinks.unwrap_or_default(),
            msg.url.depth,
        )
    }

    /// A fetched 2xx page: skipped when a redirect took it out of the
    /// crawl's scope, else crawled. The final URL of an in-scope redirect is
    /// marked seen in the frontier, so a direct link to it is not fetched a
    /// second time.
    async fn on_fetched(
        self: &Arc<Self>,
        msg: &UrlMessage,
        page: RawPage,
        elapsed: Duration,
    ) -> scrapix_core::Result<()> {
        let requested = normalize_entry_url(&msg.url.url);
        let target = normalize_entry_url(&page.final_url);
        if let Some(target) = target.filter(|t| Some(t) != requested.as_ref()) {
            if let Some(reason) = self.redirect_out_of_scope(msg, &target).await {
                self.metrics.record_skipped();
                info!(url = %msg.url.url, final_url = %page.final_url, reason, "Redirect target skipped");
                return self.on_skipped(msg, &reason, Skip::default()).await;
            }
            let seen = msg.seen_only_child(target);
            self.producer
                .send(
                    topic_names::URL_FRONTIER,
                    Some(&seen.partition_key()),
                    &seen,
                )
                .await?;
        }
        self.on_crawled(msg, page, elapsed).await
    }

    /// Why the redirect target `target` of `msg` must not be crawled, if it
    /// must not: robots.txt disallows it (unless the job ignores robots.txt),
    /// or, for a discovered URL (not a start URL, which may legitimately
    /// redirect anywhere), it is outside the job's allowed domains or
    /// include/exclude patterns. `http`/`https` and a leading `www.` count
    /// as the same site there: a `https://example.com/docs/*` include keeps
    /// a redirect to `https://www.example.com/docs/a`.
    async fn redirect_out_of_scope(&self, msg: &UrlMessage, target: &str) -> Option<String> {
        let respects_robots = !matches!(msg.job, Some(ref j) if !j.respect_robots_txt);
        if respects_robots {
            match self.fetcher.is_allowed(target).await {
                Ok(false) => {
                    return Some(format!(
                        "Redirect target disallowed by robots.txt: {target}"
                    ));
                }
                Ok(true) => {}
                Err(e) => {
                    debug!(target, error = %e, "robots.txt check of a redirect target failed")
                }
            }
        }
        let discovered = msg.url.parent_url.is_some();
        match msg.url_patterns {
            Some(ref patterns)
                if discovered && !url_allowed_any_scheme_or_www(patterns, target) =>
            {
                Some(format!(
                    "Redirect target outside the crawl's scope: {target}"
                ))
            }
            _ => None,
        }
    }

    /// Publish `PageSkipped` for `msg`.
    async fn on_skipped(
        &self,
        msg: &UrlMessage,
        reason: &str,
        skip: Skip,
    ) -> scrapix_core::Result<()> {
        let event = CrawlEvent::PageSkipped {
            job_id: msg.job_id.clone(),
            url: msg.url.url.clone(),
            reason: reason.to_string(),
            timestamp: chrono::Utc::now().timestamp_millis(),
            url_message_id: msg.message_id.clone(),
            links_published: skip.links_published,
            not_modified: skip.not_modified,
            sitemap_pending: skip.sitemap_pending,
        };
        self.publish_event(&msg.job_id, &event).await
    }

    /// Publish `links` to the frontier as children of `msg` (with the link
    /// graph's priority boost), then `UrlsDiscovered` when there are any.
    async fn publish_links(
        &self,
        msg: &UrlMessage,
        links: Vec<CrawlUrl>,
    ) -> scrapix_core::Result<()> {
        let count = links.len();
        for mut discovered in links {
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
        if count > 0 {
            let event = CrawlEvent::UrlsDiscovered {
                job_id: msg.job_id.clone(),
                source_url: msg.url.url.clone(),
                count,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            self.publish_event(&msg.job_id, &event).await?;
        }
        Ok(())
    }

    /// Start `(job, host)`'s one-time sitemap discovery for a page served
    /// at `page_url`, on that page's scheme, host and port. Returns whether
    /// this message owns it (the `sitemap_pending` its event must carry).
    ///
    /// Sitemap discovery runs in the background so it never delays the
    /// fetch. Once `sitemap_pending` is true, `maybe_discover_sitemaps`
    /// guarantees a `SitemapPublished` for `msg.message_id` on every path
    /// (disabled parser, no sitemaps, fetch error) and cannot itself fail,
    /// so accounting never waits forever for it.
    fn start_sitemap_discovery(self: &Arc<Self>, msg: &UrlMessage, page_url: &str) -> bool {
        let Some(parsed) = url::Url::parse(page_url).ok() else {
            return false;
        };
        let Some(domain) = parsed.host_str().map(str::to_string) else {
            return false;
        };
        let origin = parsed.origin().ascii_serialization();
        if !self.sitemap_discovery_should_run(&msg.job_id, &domain, msg) {
            return false;
        }
        let worker = self.clone();
        let parent = msg.clone();
        tokio::spawn(async move {
            worker
                .maybe_discover_sitemaps(&domain, &origin, &parent)
                .await;
        });
        true
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
        // Known gap (deferred): if a later publish for this message fails, the
        // redelivered message re-queues the retry again (duplicate re-queue).
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

        // Known gap (deferred): if the PageFailed publish below fails, the
        // redelivered message publishes this DLQ entry again (duplicate).
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Args;
    use clap::Parser;
    use scrapix_core::JobSpec;
    use scrapix_queue::{AnyConsumer, AnyProducer, ChannelBus, FetchFeedback};
    use serde::de::DeserializeOwned;
    use std::sync::atomic::AtomicBool;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MAX_RETRIES: u32 = 2;

    async fn worker(bus: &ChannelBus) -> Arc<CrawlerWorker> {
        worker_with_robots(bus, false).await
    }

    async fn worker_with_robots(bus: &ChannelBus, respect_robots: bool) -> Arc<CrawlerWorker> {
        let mut args = Args::parse_from(["scrapix-worker-crawler"]);
        args.allow_private_ips = true; // wiremock listens on 127.0.0.1
        args.respect_robots = respect_robots;
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

    /// A worker with an in-memory page history (incremental crawling).
    async fn worker_with_history(bus: &ChannelBus) -> Arc<CrawlerWorker> {
        let mut args = Args::parse_from(["scrapix-worker-crawler"]);
        args.allow_private_ips = true; // wiremock listens on 127.0.0.1
        args.respect_robots = false;
        args.sitemap_discovery = false;
        args.redis_url = None;
        args.browser_render = false;
        args.max_retries = MAX_RETRIES;
        let mut w = CrawlerWorker::build(
            &args,
            "test".into(),
            AnyConsumer::from(bus.consumer()),
            AnyProducer::from(bus.producer()),
        )
        .await
        .unwrap();
        w.crawl_history = Some(Arc::new(crate::page_history::PageHistory::memory()));
        Arc::new(w)
    }

    async fn worker_with_sitemaps(bus: &ChannelBus) -> Arc<CrawlerWorker> {
        let mut args = Args::parse_from(["scrapix-worker-crawler"]);
        args.allow_private_ips = true; // wiremock listens on 127.0.0.1
        args.sitemap_discovery = true;
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

    /// Poll `c` until an item matching `pred` is seen, or `overall` elapses,
    /// returning every item observed along the way (matched or not) for use
    /// in assertion failure messages. Needed for events published from a
    /// task the handler spawns and detaches (e.g. background sitemap
    /// discovery, R-18): `handle_message` can return, and this function's
    /// caller can start draining, before that task's publish lands.
    async fn wait_for_event<T, F>(c: &AnyConsumer, overall: Duration, mut pred: F) -> Vec<T>
    where
        T: DeserializeOwned + Send,
        F: FnMut(&T) -> bool,
    {
        let deadline = Instant::now() + overall;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return seen;
            }
            match c
                .poll_one::<T>(remaining.min(Duration::from_millis(100)))
                .await
                .unwrap()
            {
                Some(item) => {
                    let matched = pred(&item);
                    seen.push(item);
                    if matched {
                        return seen;
                    }
                }
                None if Instant::now() >= deadline => return seen,
                None => {}
            }
        }
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

    /// An unchanged page (304 to the conditional request built from the
    /// page history) is not downloaded again, but its stored links are
    /// followed, so an incremental re-crawl still reaches the pages below
    /// it; its `PageSkipped` counts those links and marks it unchanged.
    #[tokio::test]
    async fn not_modified_page_still_follows_its_stored_links() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_raw(
                        r#"<a href="/next/">n</a><a href="https://elsewhere.test/x">x</a>"#,
                        "text/html",
                    ),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker_with_history(&bus).await;

        // First crawl: full fetch, stores the validator, the final URL and
        // the in-scope links only (the external link is not stored).
        let (ack, _) = tracked_ack();
        w.handle_message(message(url(&server, "/"), None), ack)
            .await;
        assert_eq!(drain::<RawPageMessage>(&t.pages).await.len(), 1);
        let record = w
            .crawl_history
            .as_ref()
            .unwrap()
            .get("idx", &url(&server, "/"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.outlinks, Some(vec![url(&server, "/next")]));
        assert_eq!(record.final_url, Some(url(&server, "/")));
        drain::<UrlMessage>(&t.frontier).await;
        drain::<CrawlEvent>(&t.events).await;

        // Re-crawl: 304, no page, but the link is followed again.
        let msg = message(url(&server, "/"), None);
        let (ack, acked) = tracked_ack();
        w.handle_message(msg.clone(), ack).await;
        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        let children: Vec<UrlMessage> = drain(&t.frontier).await;
        let urls: Vec<_> = children.iter().map(|c| c.url.url.clone()).collect();
        assert_eq!(urls, vec![url(&server, "/next")]);
        assert_eq!(children[0].url.depth, 1);
        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                CrawlEvent::PageSkipped { not_modified: true, links_published: 1, url_message_id, .. }
                    if *url_message_id == msg.message_id
            )),
            "{events:?}"
        );
    }

    /// A 304 page's stored links are judged from the page's final URL: a
    /// first fetch that redirected to another host stored that host's
    /// links, and they are followed (not rejected as external).
    #[tokio::test]
    async fn stored_links_are_judged_from_the_final_url() {
        let bus = ChannelBus::new();
        let w = worker(&bus).await;
        let msg = message("https://a.test/".into(), None);
        let record = PageRecord {
            etag: Some("\"e\"".into()),
            final_url: Some("https://b.test/".into()),
            outlinks: Some(vec!["https://b.test/x".into()]),
            ..Default::default()
        };
        let links: Vec<_> = w
            .stored_links(&msg, Some(record.clone()))
            .into_iter()
            .map(|u| u.url)
            .collect();
        assert_eq!(links, vec!["https://b.test/x"]);
        // Without a final URL (older record) the requested URL is used.
        let old = PageRecord {
            final_url: None,
            ..record
        };
        assert!(w.stored_links(&msg, Some(old)).is_empty());
        assert!(w.stored_links(&msg, None).is_empty());
    }

    /// Conditional headers are sent only for a record stored under the
    /// same link filters: a widened config fetches the page in full (so its
    /// new links are found and the record rewritten); a record without a
    /// scope key (older) counts as a mismatch.
    #[tokio::test]
    async fn conditional_request_only_for_the_same_link_scope() {
        let bus = ChannelBus::new();
        let w = worker_with_history(&bus).await;
        let target = "https://a.test/docs".to_string();
        let scoped = |include: &str| {
            UrlMessage::with_patterns(
                CrawlUrl::seed(target.clone()),
                "job-1",
                "idx",
                scrapix_core::UrlPatterns {
                    include: vec![include.into()],
                    allowed_domains: vec!["a.test".into()],
                    ..Default::default()
                },
            )
        };
        let narrow = scoped("https://a.test/docs/*");
        let wide = scoped("https://a.test/*");
        let history = w.crawl_history.as_ref().unwrap();
        let mut record = PageRecord {
            etag: Some("\"v1\"".into()),
            outlinks: Some(vec![]),
            ..Default::default()
        };
        // No scope key: a mismatch.
        history.save("idx", &target, &record).await.unwrap();
        assert!(!w.conditional_headers(&narrow).await.has_headers());

        record.link_scope = Some(w.link_scope_key(&narrow));
        history.save("idx", &target, &record).await.unwrap();
        assert!(w.conditional_headers(&narrow).await.has_headers());
        // Same filters, another max_depth: still the same scope.
        let deeper = scoped("https://a.test/docs/*").with_limits(Some(9), None);
        assert!(w.conditional_headers(&deeper).await.has_headers());
        // Widened include patterns: full fetch.
        assert!(!w.conditional_headers(&wide).await.has_headers());
    }

    /// A redirect target is matched against the user's include/exclude
    /// patterns with `http`/`https` and `www.` treated as the same site.
    #[tokio::test]
    async fn redirect_scope_check_ignores_scheme_and_www() {
        let bus = ChannelBus::new();
        let w = worker(&bus).await;
        let patterns = scrapix_core::UrlPatterns {
            include: vec!["https://example.com/docs/*".into()],
            ..Default::default()
        };
        let msg = UrlMessage::with_patterns(
            CrawlUrl::new("https://example.com/docs/old", 1)
                .with_parent("https://example.com/docs"),
            "job-1",
            "idx",
            patterns,
        );
        assert_eq!(
            w.redirect_out_of_scope(&msg, "https://www.example.com/docs/a")
                .await,
            None
        );
        assert!(w
            .redirect_out_of_scope(&msg, "https://www.example.com/blog/a")
            .await
            .is_some());
    }

    fn scoped_message(server: &MockServer, target: &str) -> UrlMessage {
        let patterns = scrapix_core::UrlPatterns {
            include: vec![format!("{}/*", url(server, "/docs"))],
            ..Default::default()
        };
        UrlMessage::with_patterns(
            CrawlUrl::new(url(server, target), 1).with_parent(url(server, "/docs")),
            "job-1",
            "idx",
            patterns,
        )
    }

    /// A discovered URL that redirects out of the job's include patterns is
    /// skipped, not indexed; one that redirects inside them is crawled and
    /// its final URL marked seen in the frontier (so a direct link to it is
    /// not fetched again).
    #[tokio::test]
    async fn redirect_targets_are_scope_checked_and_marked_seen() {
        let server = MockServer::start().await;
        for (from, to) in [("/docs/old", "/blog/new"), ("/docs/a", "/docs/b/")] {
            Mock::given(method("GET"))
                .and(path(from))
                .respond_with(ResponseTemplate::new(301).insert_header("location", to))
                .mount(&server)
                .await;
        }
        for p in ["/blog/new", "/docs/b/"] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).set_body_raw("<p>x</p>", "text/html"))
                .mount(&server)
                .await;
        }
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;

        let out = scoped_message(&server, "/docs/old");
        let (ack, acked) = tracked_ack();
        w.handle_message(out.clone(), ack).await;
        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                CrawlEvent::PageSkipped { reason, url_message_id, .. }
                    if *url_message_id == out.message_id && reason.contains("outside the crawl's scope")
            )),
            "{events:?}"
        );
        assert!(!events.iter().any(|e| matches!(
            e,
            CrawlEvent::PageCrawled { .. } | CrawlEvent::PageFailed { .. }
        )));

        let inside = scoped_message(&server, "/docs/a");
        let (ack, _) = tracked_ack();
        w.handle_message(inside, ack).await;
        assert_eq!(drain::<RawPageMessage>(&t.pages).await.len(), 1);
        let frontier: Vec<UrlMessage> = drain(&t.frontier).await;
        assert!(
            frontier
                .iter()
                .any(|m| m.seen_only && m.url.url == url(&server, "/docs/b")),
            "{frontier:?}"
        );
    }

    /// robots.txt disallows and unsupported content types are skipped with
    /// their reason, not reported as failed pages.
    #[tokio::test]
    async fn robots_disallowed_and_unsupported_content_are_skipped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("User-agent: *\nDisallow: /private\n", "text/plain"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/llms.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("# llms", "text/plain"))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker_with_robots(&bus, true).await;

        for (target, expected) in [
            ("/private/x", crate::outcome::ROBOTS_DISALLOWED),
            ("/llms.txt", "Unsupported content type: text/plain"),
        ] {
            let msg = message(url(&server, target), None);
            let (ack, acked) = tracked_ack();
            w.handle_message(msg.clone(), ack).await;
            assert!(acked.load(Ordering::SeqCst));
            let events: Vec<CrawlEvent> = drain(&t.events).await;
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    CrawlEvent::PageSkipped { reason, url_message_id, .. }
                        if *url_message_id == msg.message_id && reason.starts_with(expected)
                )),
                "{target}: {events:?}"
            );
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, CrawlEvent::PageFailed { .. })),
                "{target}: {events:?}"
            );
        }
        assert!(drain::<DlqMessage>(&t.dlq).await.is_empty());
    }

    /// Sitemap discovery uses the crawled page's scheme and port, and its
    /// entries are normalized like links (`/post/` -> `/post`).
    #[tokio::test]
    async fn sitemap_discovery_keeps_the_origin_and_normalizes_entries() {
        let server = MockServer::start().await;
        let base = url(&server, "");
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("User-agent: *\nSitemap: {base}/sitemap.xml\n"),
                "text/plain",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sitemap.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!(
                    r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>{base}/post/</loc></url></urlset>"#
                ),
                "application/xml",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>home</p>", "text/html"))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker_with_sitemaps(&bus).await;
        let msg = message(url(&server, "/"), None);
        let (ack, _) = tracked_ack();
        w.handle_message(msg.clone(), ack).await;

        let events: Vec<CrawlEvent> = wait_for_event(&t.events, Duration::from_secs(10), |e| {
            matches!(e, CrawlEvent::SitemapPublished { url_message_id, .. } if *url_message_id == msg.message_id)
        })
        .await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CrawlEvent::SitemapPublished { count: 1, .. })),
            "{events:?}"
        );
        let frontier: Vec<UrlMessage> = drain(&t.frontier).await;
        let urls: Vec<_> = frontier.iter().map(|m| m.url.url.clone()).collect();
        assert_eq!(urls, vec![format!("{base}/post")]);
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

    /// R-18: even when the domain's sitemap discovery finds nothing (here,
    /// neither `robots.txt` nor `sitemap.xml` are mounted, so both fetches
    /// 404), the crawler still publishes `SitemapPublished { count: 0 }` for
    /// the page that spawned it, and that page's `PageCrawled` carries
    /// `sitemap_pending: true` — the two events job-completion accounting
    /// needs to ever stop waiting on this domain's discovery.
    #[tokio::test]
    async fn sitemap_discovery_publishes_settled_event_even_when_it_finds_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<html><body>no links here</body></html>", "text/html"),
            )
            .mount(&server)
            .await;
        // Discovery targets the crawled page's own origin (scheme, host and
        // port): neither robots.txt nor sitemap.xml is mounted there, so
        // both 404 and discovery finds nothing.
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker_with_sitemaps(&bus).await;
        let msg = message(url(&server, "/"), None);
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;
        assert!(acked.load(Ordering::SeqCst));

        // `SitemapPublished` comes from a task `on_crawled` spawns and
        // detaches, so it can still be in flight after `handle_message`
        // returns — poll with a bounded timeout instead of draining once.
        let events: Vec<CrawlEvent> = wait_for_event(&t.events, Duration::from_secs(10), |e| {
            matches!(
                e,
                CrawlEvent::SitemapPublished { url_message_id, .. }
                    if *url_message_id == msg.message_id
            )
        })
        .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                CrawlEvent::PageCrawled { sitemap_pending: true, url_message_id, .. }
                    if *url_message_id == msg.message_id
            )),
            "expected PageCrawled.sitemap_pending=true for {msg:?}: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                CrawlEvent::SitemapPublished { count: 0, url_message_id, .. }
                    if *url_message_id == msg.message_id
            )),
            "expected a settling SitemapPublished{{count:0}} for {msg:?}: {events:?}"
        );
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

    fn proxy_spec(urls: Vec<String>) -> scrapix_core::ProxyConfig {
        scrapix_core::ProxyConfig {
            urls,
            rotation: scrapix_core::ProxyRotation::RoundRobin,
            tiered: None,
        }
    }

    fn events_have_retry_with(events: &[CrawlEvent], needle: &str) -> bool {
        events
            .iter()
            .any(|e| matches!(e, CrawlEvent::PageRetried { error, .. } if error.contains(needle)))
    }

    #[tokio::test]
    async fn browser_job_with_proxy_fails_closed() {
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let job = JobSpec {
            crawler_type: CrawlerType::Browser,
            proxy: Some(proxy_spec(vec!["http://proxy.test:8080".into()])),
            ..Default::default()
        };
        let (ack, acked) = tracked_ack();

        w.handle_message(message("https://spa.test/".into(), Some(job)), ack)
            .await;

        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        assert!(drain::<UrlMessage>(&t.frontier).await.is_empty());
        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(events.iter().any(|e| matches!(
            e,
            CrawlEvent::PageFailed { error, status: None, .. } if error == BROWSER_PROXY_UNSUPPORTED
        )));
    }

    #[tokio::test]
    async fn all_proxies_in_cooldown_retries_instead_of_direct_fetch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>ok</p>", "text/html"))
            .expect(0) // never fetched directly
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let proxy = "http://proxy.test:8080".to_string();
        let job = JobSpec {
            proxy: Some(proxy_spec(vec![proxy.clone()])),
            ..Default::default()
        };
        // Populate the job's pool, then put its only proxy in cooldown.
        w.shaper
            .options_for("job-1", Some(&job), FetchOptions::default());
        for _ in 0..crate::job_fetch::PROXY_MAX_FAILURES {
            w.shaper.report_proxy("job-1", &proxy, false);
        }
        let (ack, acked) = tracked_ack();

        w.handle_message(message(url(&server, "/p"), Some(job)), ack)
            .await;

        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        let requeued: Vec<UrlMessage> = drain(&t.frontier).await;
        assert_eq!(requeued.len(), 1);
        assert_eq!(requeued[0].url.retry_count, 1);
        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(
            events_have_retry_with(&events, "no proxy available"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn proxy_auth_failure_is_retried_not_indexed() {
        // wiremock plays the proxy and answers 407 for everything.
        let proxy = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(407))
            .mount(&proxy)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        let job = JobSpec {
            proxy: Some(proxy_spec(vec![proxy
                .uri()
                .replace("127.0.0.1", "localhost")])),
            ..Default::default()
        };
        let (ack, acked) = tracked_ack();

        w.handle_message(message("http://origin.invalid/x".into(), Some(job)), ack)
            .await;

        assert!(acked.load(Ordering::SeqCst));
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        let events: Vec<CrawlEvent> = drain(&t.events).await;
        assert!(
            events_have_retry_with(&events, "proxy error: HTTP 407"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn job_headers_not_sent_to_hosts_outside_allowed_domains() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>ok</p>", "text/html"))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let w = worker(&bus).await;
        let job = JobSpec {
            headers: [("X-Api-Key".to_string(), "secret".to_string())].into(),
            ..Default::default()
        };
        let send = |allowed: Vec<String>| {
            let mut m = message(url(&server, "/h"), Some(job.clone()));
            m.url_patterns = Some(scrapix_core::UrlPatterns {
                allowed_domains: allowed,
                ..Default::default()
            });
            m
        };

        let (ack, _) = tracked_ack();
        w.handle_message(send(vec!["docs.example.com".into()]), ack)
            .await;
        let (ack, _) = tracked_ack();
        w.handle_message(send(vec!["localhost".into()]), ack).await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].headers.contains_key("x-api-key"));
        assert_eq!(requests[1].headers.get("x-api-key").unwrap(), "secret");
    }

    /// The single `FetchFeedback` published for `msg` (fails on 0 or 2+).
    async fn feedback_for(bus: &ChannelBus, msg: &UrlMessage) -> FetchFeedback {
        let c = reader(bus, topic_names::FETCH_FEEDBACK);
        let all: Vec<FetchFeedback> = drain(&c).await;
        let mine: Vec<_> = all
            .into_iter()
            .filter(|f| f.message_id == msg.message_id)
            .collect();
        assert_eq!(mine.len(), 1, "exactly one feedback per message: {mine:?}");
        let fb = mine.into_iter().next().unwrap();
        assert_eq!(fb.job_id, msg.job_id);
        assert_eq!(fb.url, msg.url.url);
        assert_eq!(fb.domain, "localhost");
        assert!(fb.timestamp > 0);
        fb
    }

    #[tokio::test]
    async fn success_publishes_fetch_feedback_keyed_by_domain() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>ok</p>", "text/html"))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let w = worker(&bus).await;
        let msg = message(url(&server, "/ok"), None);
        let (ack, acked) = tracked_ack();
        w.handle_message(msg.clone(), ack).await;
        assert!(acked.load(Ordering::SeqCst));

        let fb = feedback_for(&bus, &msg).await;
        assert_eq!(fb.status, Some(200));
        assert!(!fb.transport_error);
        assert_eq!(fb.retry_after_ms, None);
        assert_eq!(fb.crawl_delay_ms, None, "robots not consulted");
        assert!(!fb.robots_checked);
    }

    #[tokio::test]
    async fn rate_limited_feedback_carries_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let w = worker(&bus).await;
        let msg = message(url(&server, "/slow"), None);
        let (ack, _) = tracked_ack();
        w.handle_message(msg.clone(), ack).await;

        let fb = feedback_for(&bus, &msg).await;
        assert_eq!(fb.status, Some(429));
        assert_eq!(fb.retry_after_ms, Some(120_000));
    }

    #[tokio::test]
    async fn fail_closed_before_fetch_still_sends_feedback_without_status() {
        let bus = ChannelBus::new();
        let w = worker(&bus).await;
        let job = JobSpec {
            crawler_type: CrawlerType::Browser,
            ..Default::default()
        };
        let msg = message("http://localhost:1/spa".into(), Some(job));
        let (ack, acked) = tracked_ack();
        w.handle_message(msg.clone(), ack).await;
        assert!(acked.load(Ordering::SeqCst));

        let fb = feedback_for(&bus, &msg).await;
        assert_eq!(fb.status, None);
        assert!(!fb.transport_error, "no request was made");
    }

    #[tokio::test]
    async fn transport_error_feedback_is_flagged() {
        let bus = ChannelBus::new();
        let w = worker(&bus).await;
        // Nothing listens on port 1: connection refused.
        let msg = message("http://localhost:1/x".into(), None);
        let (ack, _) = tracked_ack();
        w.handle_message(msg.clone(), ack).await;

        let fb = feedback_for(&bus, &msg).await;
        assert_eq!(fb.status, None);
        assert!(fb.transport_error);
    }

    #[tokio::test]
    async fn robots_crawl_delay_is_reported_only_for_jobs_respecting_robots() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("User-agent: *\nCrawl-delay: 2\n", "text/plain"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>ok</p>", "text/html"))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let w = worker_with_robots(&bus, true).await;

        let respects = message(url(&server, "/page"), Some(JobSpec::default()));
        let (ack, _) = tracked_ack();
        w.handle_message(respects.clone(), ack).await;
        let fb = feedback_for(&bus, &respects).await;
        assert_eq!(fb.status, Some(200));
        assert_eq!(fb.crawl_delay_ms, Some(2_000));
        assert!(fb.robots_checked);

        let ignores = message(
            url(&server, "/page"),
            Some(JobSpec {
                respect_robots_txt: false,
                ..Default::default()
            }),
        );
        let (ack, _) = tracked_ack();
        w.handle_message(ignores.clone(), ack).await;
        let fb = feedback_for(&bus, &ignores).await;
        assert_eq!(fb.crawl_delay_ms, None);
        assert!(!fb.robots_checked, "job ignores robots.txt");
    }

    #[tokio::test]
    async fn robots_without_crawl_delay_is_reported_as_checked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("User-agent: *\nAllow: /\n", "text/plain"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>ok</p>", "text/html"))
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let w = worker_with_robots(&bus, true).await;
        let msg = message(url(&server, "/page"), Some(JobSpec::default()));
        let (ack, _) = tracked_ack();
        w.handle_message(msg.clone(), ack).await;
        let fb = feedback_for(&bus, &msg).await;
        assert!(fb.robots_checked);
        assert_eq!(fb.crawl_delay_ms, None);
    }

    /// R5: a message of a cancelled (or finished) job is acked without any
    /// fetch or publish, except the FetchFeedback that frees the frontier's
    /// politeness slot for this dispatch.
    #[tokio::test]
    async fn cancelled_job_message_is_acked_without_fetching_and_frees_its_slot() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<html></html>", "text/html"))
            .expect(0)
            .mount(&server)
            .await;
        let bus = ChannelBus::new();
        let t = topics(&bus);
        let w = worker(&bus).await;
        w.cancelled.insert("job-1");
        let msg = message(url(&server, "/"), None);
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;

        assert!(acked.load(Ordering::SeqCst), "acked");
        let fb = feedback_for(&bus, &msg).await;
        assert_eq!(fb.status, None, "no request was made");
        assert!(!fb.transport_error);
        assert!(drain::<RawPageMessage>(&t.pages).await.is_empty());
        assert!(drain::<UrlMessage>(&t.frontier).await.is_empty());
        assert!(drain::<CrawlEvent>(&t.events).await.is_empty());
        assert!(drain::<DlqMessage>(&t.dlq).await.is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// A one-page text PDF whose text layer is `text` (Helvetica).
    fn pdf_with_text(text: &str) -> Vec<u8> {
        let content = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn features(pdf: bool, extract_links: bool, documents: bool) -> scrapix_core::FeaturesConfig {
        scrapix_core::FeaturesConfig {
            pdf: pdf.then(|| scrapix_core::PdfConfig {
                enabled: true,
                extract_links,
                ..Default::default()
            }),
            documents: documents.then_some(scrapix_core::DocumentsConfig {
                enabled: true,
                max_size_mb: None,
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn pdf_links_are_followed_only_with_extract_links() {
        let server = MockServer::start().await;
        let next = url(&server, "/next");
        Mock::given(method("GET"))
            .and(path("/doc.pdf"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                pdf_with_text(&format!("Read more at {next} today")),
                "application/pdf",
            ))
            .mount(&server)
            .await;

        for (extract_links, expected) in [(false, 0usize), (true, 1)] {
            let bus = ChannelBus::new();
            let t = topics(&bus);
            let w = worker(&bus).await;
            let mut msg = message(url(&server, "/doc.pdf"), None);
            msg.features = Some(features(true, extract_links, false));
            let (ack, acked) = tracked_ack();

            w.handle_message(msg.clone(), ack).await;

            assert!(acked.load(Ordering::SeqCst));
            let pages: Vec<RawPageMessage> = drain(&t.pages).await;
            assert_eq!(pages.len(), 1, "the PDF is forwarded to the content worker");
            let children: Vec<UrlMessage> = drain(&t.frontier).await;
            assert_eq!(children.len(), expected, "extract_links={extract_links}");
            if expected == 1 {
                assert_eq!(children[0].url.url, next);
            }
        }
    }

    #[tokio::test]
    async fn document_links_are_followed_when_the_job_parses_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"<a href="/a.pdf">a</a><a href="/b.docx">b</a><a href="/c.png">c</a>"#,
                "text/html",
            ))
            .mount(&server)
            .await;

        for (pdf, documents, expected) in [
            (false, false, vec![]),
            (true, false, vec!["/a.pdf"]),
            (true, true, vec!["/a.pdf", "/b.docx"]),
        ] {
            let bus = ChannelBus::new();
            let t = topics(&bus);
            let w = worker(&bus).await;
            let mut msg = message(url(&server, "/"), None);
            msg.features = Some(features(pdf, false, documents));
            let (ack, _) = tracked_ack();
            w.handle_message(msg, ack).await;
            let children: Vec<UrlMessage> = drain(&t.frontier).await;
            let mut got: Vec<String> = children
                .iter()
                .map(|c| url::Url::parse(&c.url.url).unwrap().path().to_string())
                .collect();
            got.sort();
            assert_eq!(got, expected, "pdf={pdf} documents={documents}");
        }
    }
}
