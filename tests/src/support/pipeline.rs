//! Harness for the distributed-path integration tests (R10).
//!
//! Runs the REAL frontier service, crawler worker and content worker over
//! one in-process [`ChannelBus`] (the same `with_bus`/`run_with_bus`
//! constructors `scrapix all` uses), with a [`MemoryFrontierStore`], a
//! wiremock "site" to crawl and a wiremock "Meilisearch" to index into.
//! Every event on `EVENTS` is recorded and folded into a [`JobAccounting`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use parking_lot::Mutex;
use scrapix_core::{CrawlUrl, JobSpec, SitemapConfig};
use scrapix_frontier::{FrontierStore, MemoryFrontierStore};
use scrapix_queue::{
    topic_names, AnyConsumer, AnyProducer, ChannelBus, ChannelConsumer, CrawlEvent, JobAccounting,
    JobAction, JobControl, UrlMessage,
};
use tokio::task::JoinHandle;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Index every test job writes to.
pub const INDEX: &str = "idx";

/// Hard timeout for any wait in these tests.
pub const TIMEOUT: Duration = Duration::from_secs(60);

// ============================================================================
// Site
// ============================================================================

/// An HTML page with enough main content to pass the content worker's
/// `min_content_length`, linking to `links` (site-relative paths).
pub fn page_html(title: &str, links: &[String]) -> String {
    let anchors: String = links
        .iter()
        .map(|l| format!("<a href=\"{l}\">{l}</a> "))
        .collect();
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><title>{title}</title></head><body>\
         <nav>{anchors}</nav><main><h1>{title}</h1>\
         <p>This is the page called {title}. It explains in detail how the frontier, \
         the crawler worker and the content worker cooperate to index documents.</p>\
         <p>Every page ends in exactly one terminal outcome and offsets commit only \
         after Meilisearch accepted the batch that contains the page document.</p>\
         </main></body></html>"
    )
}

/// The wiremock site. Addressed as `http://localhost:<port>` (raw-IP seeds
/// are refused by the crawler).
pub struct Site {
    pub server: MockServer,
}

impl Site {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Self { server }
    }

    pub fn base(&self) -> String {
        format!("http://localhost:{}", self.server.address().port())
    }

    pub fn url(&self, p: &str) -> String {
        format!("{}{}", self.base(), p)
    }

    /// Serve `html` at `p` (200, text/html).
    pub async fn page(&self, p: &str, html: String) {
        self.respond(p, html_response(html)).await;
    }

    pub async fn respond(&self, p: &str, response: impl Respond + 'static) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    /// Every page request received (robots.txt excluded).
    pub async fn page_requests(&self) -> Vec<Request> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.url.path() != "/robots.txt")
            .collect()
    }

    pub async fn hits(&self, p: &str) -> usize {
        self.page_requests()
            .await
            .iter()
            .filter(|r| r.url.path() == p)
            .count()
    }
}

pub fn html_response(html: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(html, "text/html; charset=utf-8")
}

/// A responder that records when each request arrived, then answers with
/// `response` (which may carry a delay).
pub struct Timed {
    pub arrivals: Arc<Mutex<Vec<Instant>>>,
    pub response: ResponseTemplate,
}

impl Respond for Timed {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.arrivals.lock().push(Instant::now());
        self.response.clone()
    }
}

// ============================================================================
// Meilisearch
// ============================================================================

fn task_accepted() -> ResponseTemplate {
    ResponseTemplate::new(202).set_body_json(serde_json::json!({
        "taskUid": 1, "indexUid": INDEX, "status": "enqueued",
        "type": "documentAdditionOrUpdate", "enqueuedAt": "2026-01-01T00:00:00Z"}))
}

/// The wiremock Meilisearch: indexes don't exist yet (404), index creation
/// and settings are accepted, document additions return a 202 task and
/// their bodies are recorded.
pub struct Meili {
    pub server: MockServer,
}

impl Meili {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/indexes/[^/]+$"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "message": "not found", "code": "index_not_found",
                "type": "invalid_request", "link": "https://docs.meilisearch.com"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/indexes"))
            .respond_with(task_accepted())
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/indexes/[^/]+/settings$"))
            .respond_with(task_accepted())
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/indexes/[^/]+/settings/.*$"))
            .respond_with(task_accepted())
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/indexes/[^/]+/documents$"))
            .respond_with(task_accepted())
            .mount(&server)
            .await;
        Self { server }
    }

    pub fn uri(&self) -> String {
        self.server.uri()
    }

    /// Every document received in a document-addition batch, in order.
    pub async fn documents(&self) -> Vec<serde_json::Value> {
        let requests = self.server.received_requests().await.unwrap_or_default();
        requests
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/documents"))
            .flat_map(
                |r| match serde_json::from_slice::<serde_json::Value>(&r.body) {
                    Ok(serde_json::Value::Array(docs)) => docs,
                    Ok(doc) => vec![doc],
                    Err(_) => Vec::new(),
                },
            )
            .collect()
    }

    /// Distinct document URLs received.
    pub async fn document_urls(&self) -> std::collections::BTreeSet<String> {
        self.documents()
            .await
            .iter()
            .filter_map(|d| d["url"].as_str().map(str::to_string))
            .collect()
    }
}

// ============================================================================
// Pipeline
// ============================================================================

/// Knobs of one pipeline run.
#[derive(Clone)]
pub struct PipelineConfig {
    /// Frontier `DOMAIN_DELAY_MS`
    pub domain_delay_ms: u64,
    /// Frontier `CONCURRENT_PER_DOMAIN`
    pub concurrent_per_domain: usize,
    /// Crawler `CONCURRENCY`
    pub crawler_concurrency: usize,
    /// Crawler `MAX_RETRIES`
    pub max_retries: u32,
    /// Frontier `REQUEST_TIMEOUT` (politeness slots expire after twice this)
    pub request_timeout_secs: u64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            domain_delay_ms: 10,
            concurrent_per_domain: 4,
            crawler_concurrency: 8,
            max_retries: 3,
            request_timeout_secs: 5,
        }
    }
}

/// A running crawler worker whose consumer the test can crash.
pub struct Crawler {
    pub consumer: Arc<ChannelConsumer>,
    task: JoinHandle<()>,
}

impl Crawler {
    /// Kill this crawler as a crashed process would die: in-flight handlers
    /// are aborted and their messages stay un-acked.
    pub fn crash(&self) {
        self.consumer.crash();
        self.task.abort();
    }
}

/// Frontier + crawler + content running over one `ChannelBus`.
pub struct Pipeline {
    pub bus: ChannelBus,
    pub config: PipelineConfig,
    pub store: Arc<MemoryFrontierStore>,
    pub meili: Meili,
    producer: AnyProducer,
    events: Arc<Mutex<Vec<CrawlEvent>>>,
    accounting: Arc<Mutex<JobAccounting>>,
    crawlers: Mutex<Vec<Crawler>>,
    crawler_seq: AtomicU64,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        for c in self.crawlers.lock().iter() {
            c.crash();
        }
    }
}

impl Pipeline {
    /// Start the frontier, one crawler and the content worker.
    pub async fn start(config: PipelineConfig) -> Self {
        let p = Self::start_without_crawler(config).await;
        p.spawn_crawler().await;
        p
    }

    /// Start the frontier and the content worker only.
    pub async fn start_without_crawler(config: PipelineConfig) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .try_init();

        let bus = ChannelBus::new();
        let meili = Meili::start().await;
        let store = Arc::new(MemoryFrontierStore::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let accounting = Arc::new(Mutex::new(JobAccounting::default()));
        let mut tasks = Vec::new();

        // Event collector (the only reader of EVENTS: there is no API here).
        let events_consumer = AnyConsumer::channel(bus.consumer());
        events_consumer.subscribe(&[topic_names::EVENTS]).unwrap();
        {
            let (events, accounting) = (events.clone(), accounting.clone());
            tasks.push(tokio::spawn(async move {
                loop {
                    match events_consumer
                        .poll_one::<CrawlEvent>(Duration::from_millis(50))
                        .await
                    {
                        Ok(Some(e)) => {
                            accounting.lock().apply(&e);
                            events.lock().push(e);
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "bad event"),
                    }
                }
            }));
        }

        // Frontier
        let mut fargs = scrapix_frontier_service::Args::parse_from(["scrapix-frontier-service"]);
        fargs.redis_url = None;
        fargs.instance_id = Some("test-frontier".into());
        fargs.domain_delay_ms = config.domain_delay_ms;
        fargs.concurrent_per_domain = config.concurrent_per_domain;
        fargs.request_timeout_secs = config.request_timeout_secs;
        fargs.dispatch_interval_ms = 20;
        fargs.enable_linkgraph = false;
        fargs.enable_recrawl = false;
        let fmain = Arc::new(AnyConsumer::channel(bus.consumer()));
        fmain.subscribe(&[topic_names::URL_FRONTIER]).unwrap();
        let ffeedback = Arc::new(AnyConsumer::channel(bus.consumer()));
        ffeedback.subscribe(&[topic_names::FETCH_FEEDBACK]).unwrap();
        let fcontrol = Arc::new(AnyConsumer::channel(
            bus.consumer_in_group("frontier-control"),
        ));
        fcontrol.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let fproducer = Arc::new(AnyProducer::channel(bus.producer()));
        let fstore: Arc<dyn FrontierStore> = store.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = scrapix_frontier_service::run_with_bus(
                fargs,
                fproducer,
                fmain,
                None,
                None,
                Some(ffeedback),
                Some(fcontrol),
                fstore,
            )
            .await
            {
                tracing::error!(error = %e, "frontier failed");
            }
        }));

        // Content worker
        let muri = meili.uri();
        let mut cargs = scrapix_worker_content::Args::parse_from([
            "scrapix-worker-content",
            "--meilisearch-url",
            muri.as_str(),
        ]);
        cargs.meilisearch_key = None;
        cargs.skip_meilisearch = false;
        cargs.default_index = INDEX.into();
        cargs.batch_size = 1;
        cargs.min_content_length = 100;
        cargs.publish_to_kafka = false;
        cargs.publish_history = false;
        cargs.enable_summary = false;
        cargs.enable_extraction = false;
        cargs.enable_block_split = false;
        cargs.enable_dedup = false;
        cargs.worker_id = Some("test-content".into());
        let cconsumer = Arc::new(AnyConsumer::channel(bus.consumer()));
        cconsumer.subscribe(&[topic_names::PAGES_RAW]).unwrap();
        let ccontrol = Arc::new(AnyConsumer::channel(
            bus.consumer_in_group("content-control"),
        ));
        ccontrol.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let cproducer = Arc::new(AnyProducer::channel(bus.producer()));
        tasks.push(tokio::spawn(async move {
            if let Err(e) =
                scrapix_worker_content::run_with_bus(cargs, cconsumer, cproducer, Some(ccontrol))
                    .await
            {
                tracing::error!(error = %e, "content worker failed");
            }
        }));

        Self {
            producer: AnyProducer::channel(bus.producer()),
            bus,
            config,
            store,
            meili,
            events,
            accounting,
            crawlers: Mutex::new(Vec::new()),
            crawler_seq: AtomicU64::new(0),
            tasks,
        }
    }

    /// Start one more crawler worker on the bus (sharing `URL_PROCESSING`).
    pub async fn spawn_crawler(&self) {
        let n = self.crawler_seq.fetch_add(1, Ordering::Relaxed);
        let mut args = scrapix_worker_crawler::Args::parse_from(["scrapix-worker-crawler"]);
        args.redis_url = None;
        args.worker_id = Some(format!("test-crawler-{n}"));
        args.concurrency = self.config.crawler_concurrency;
        args.max_retries = self.config.max_retries;
        args.timeout = 10;
        args.allow_private_ips = true;
        args.sitemap_discovery = false;
        args.respect_robots = true;
        args.follow_external = false;
        args.link_graph = false;
        args.publish_links = false;
        args.incremental_crawl = true;
        args.browser_render = false;
        let consumer = Arc::new(self.bus.consumer());
        let control =
            AnyConsumer::channel(self.bus.consumer_in_group(format!("crawler-control-{n}")));
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let producer = AnyProducer::channel(self.bus.producer());
        let any_consumer = AnyConsumer::Channel(consumer.clone());
        let task = tokio::spawn(async move {
            if let Err(e) =
                scrapix_worker_crawler::run_with_bus(args, producer, any_consumer, Some(control))
                    .await
            {
                tracing::error!(error = %e, "crawler failed");
            }
        });
        self.crawlers.lock().push(Crawler { consumer, task });
    }

    /// Run `f` on the `i`-th crawler started.
    pub fn with_crawler<R>(&self, i: usize, f: impl FnOnce(&Crawler) -> R) -> R {
        f(&self.crawlers.lock()[i])
    }

    /// A seed message the way the API builds one: job context (source,
    /// account, job spec, per-job Meilisearch target) attached.
    pub fn seed(&self, url: &str, job_id: &str) -> UrlMessage {
        let spec = JobSpec {
            sitemap: SitemapConfig {
                enabled: false,
                urls: Vec::new(),
            },
            ..JobSpec::default()
        };
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        let patterns = scrapix_core::UrlPatterns {
            allowed_domains: vec![host],
            ..Default::default()
        };
        UrlMessage::with_patterns(CrawlUrl::seed(url), job_id, INDEX, patterns)
            .with_source(Some("src-test".into()))
            .account("acct-test")
            .with_meilisearch(Some(self.meili.uri()), Some(String::new()))
            .with_features(Some(Default::default()))
            .with_job(Some(spec))
    }

    /// Publish seed messages to the frontier (what `POST /crawl` does) and
    /// count them in the job's accounting.
    pub async fn submit(&self, seeds: &[UrlMessage]) {
        self.accounting.lock().seeds_published += seeds.len() as u64;
        for s in seeds {
            self.producer
                .send(topic_names::URL_FRONTIER, Some(&s.job_id), s)
                .await
                .unwrap();
        }
    }

    /// Cancel a job (what `DELETE /job/{id}` does).
    pub async fn cancel(&self, job_id: &str) {
        self.producer
            .send(
                topic_names::JOB_STATUS,
                Some(job_id),
                &JobControl::new(job_id, JobAction::Cancel),
            )
            .await
            .unwrap();
    }

    pub fn events(&self) -> Vec<CrawlEvent> {
        self.events.lock().clone()
    }

    pub fn accounting(&self) -> JobAccounting {
        self.accounting.lock().clone()
    }

    /// Wait (polling every 50ms, at most [`TIMEOUT`]) until `cond` holds
    /// over the accounting and events. Panics with `what` and the current
    /// state on timeout.
    pub async fn wait_for(
        &self,
        what: &str,
        mut cond: impl FnMut(&JobAccounting, &[CrawlEvent]) -> bool,
    ) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            {
                let acc = self.accounting.lock();
                let events = self.events.lock();
                if cond(&acc, &events) {
                    return;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}\naccounting: {:#?}\nevents: {:#?}",
                    self.accounting(),
                    self.events()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait until the job balances and `extra` holds, then require it to
    /// stay balanced for a short grace period (like the API's completion
    /// check, which never trusts a single balanced observation).
    pub async fn wait_balanced(
        &self,
        mut extra: impl FnMut(&JobAccounting, &[CrawlEvent]) -> bool,
    ) -> JobAccounting {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "accounting never stayed balanced: {:#?}",
                self.accounting()
            );
            self.wait_for("balanced accounting", |a, e| a.is_balanced() && extra(a, e))
                .await;
            let mut stable = true;
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if !self.accounting.lock().is_balanced() {
                    stable = false;
                    break;
                }
            }
            if stable {
                return self.accounting();
            }
        }
    }

    pub async fn queued(&self, job_id: &str) -> u64 {
        self.store.queued(job_id).await.unwrap()
    }
}

// ============================================================================
// Event helpers
// ============================================================================

pub fn crawled_urls(events: &[CrawlEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            CrawlEvent::PageCrawled { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect()
}

pub fn count_crawled(events: &[CrawlEvent]) -> usize {
    crawled_urls(events).len()
}

/// `(url, status)` of every `PageFailed`.
pub fn failures(events: &[CrawlEvent]) -> Vec<(String, Option<u16>)> {
    events
        .iter()
        .filter_map(|e| match e {
            CrawlEvent::PageFailed { url, status, .. } => Some((url.clone(), *status)),
            _ => None,
        })
        .collect()
}

pub fn retries_of(events: &[CrawlEvent], url: &str) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, CrawlEvent::PageRetried { url: u, .. } if u == url))
        .count()
}
