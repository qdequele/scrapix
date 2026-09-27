//! Scrapix Crawler Worker
//!
//! Distributed worker that fetches web pages from the URL frontier queue.
//!
//! ## Responsibilities
//!
//! 1. Consume URLs from the frontier topic
//! 2. Fetch pages (respecting robots.txt and rate limits, per-job headers,
//!    user agents, proxy and browser rendering)
//! 3. Classify the HTTP status: only 2xx pages go to the content worker;
//!    429/5xx and transport errors are re-queued with backoff, then
//!    dead-lettered; other statuses fail with a `PageFailed` event
//! 4. Extract links from fetched pages and publish them back to the frontier
//! 5. Ack the message only after all of the above was published
//!
//! ## Features
//!
//! - DNS caching for improved performance
//! - Link graph analysis for priority boosting
//! - Incremental crawling with conditional HTTP headers

mod handler;
pub mod job_fetch;
pub mod outcome;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use scrapix_lifecycle::{
    idle_minutes_from_env, install_signal_handlers, spawn_idle_watchdog, spawn_wake_listener,
    wake_port_from_env,
};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use scrapix_core::{CrawlUrl, UrlPatterns};
use scrapix_crawler::{
    is_non_page_url_for, url_allowed, ExtractorConfig, HttpFetcher, HttpFetcherBuilder,
    RobotsCache, RobotsConfig, SitemapConfig, SitemapParser, UrlExtractor,
};
#[cfg(feature = "browser")]
use scrapix_crawler::{CdpRenderer, CdpRendererBuilder};
use scrapix_frontier::LinkGraph;
use scrapix_queue::{
    control_group_id, topic_names, AnyConsumer, AnyProducer, CancelledJobs, ConsumerBuilder,
    CrawlEvent, ProducerBuilder, UrlMessage,
};
use scrapix_storage::{RedisCrawlHistory, RedisStorage};

use std::collections::HashSet;

use crate::job_fetch::JobFetchShaper;

/// Crawler worker for fetching web pages from the URL frontier
#[derive(Parser, Debug)]
#[command(name = "scrapix-worker-crawler")]
#[command(version, about = "Crawler worker for fetching web pages")]
pub struct Args {
    /// Kafka/Redpanda broker addresses
    #[arg(short, long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
    pub brokers: String,

    /// Consumer group ID
    #[arg(
        short,
        long,
        env = "KAFKA_GROUP_ID",
        default_value = "scrapix-crawlers"
    )]
    pub group_id: String,

    /// Number of concurrent fetchers
    #[arg(short, long, env = "CONCURRENCY", default_value = "50")]
    pub concurrency: usize,

    /// User agent string
    #[arg(
        long,
        env = "USER_AGENT",
        default_value = "Scrapix/1.0 (compatible; +https://github.com/quentindequelen/scrapix)"
    )]
    pub user_agent: String,

    /// Request timeout in seconds
    #[arg(long, env = "REQUEST_TIMEOUT", default_value = "30")]
    pub timeout: u64,

    /// Maximum re-queues per URL for transient failures (429/5xx, network
    /// errors) before it is dead-lettered. The fetcher itself retries once
    /// in-process on top of this.
    #[arg(long, env = "MAX_RETRIES", default_value = "3")]
    pub max_retries: u32,

    /// Follow external links (different domain)
    #[arg(long, env = "FOLLOW_EXTERNAL")]
    pub follow_external: bool,

    /// Maximum crawl depth
    #[arg(long, env = "MAX_DEPTH", default_value = "100")]
    pub max_depth: u32,

    /// Maximum response body size in MB
    #[arg(long, env = "MAX_BODY_SIZE_MB", default_value = "10")]
    pub max_body_size_mb: usize,

    /// Respect robots.txt
    #[arg(long, env = "RESPECT_ROBOTS", default_value = "true")]
    pub respect_robots: bool,

    /// Worker ID (for logging/metrics)
    #[arg(long, env = "WORKER_ID")]
    pub worker_id: Option<String>,

    /// Enable DNS caching for improved performance
    #[arg(long, env = "DNS_CACHE", default_value = "true")]
    pub dns_cache: bool,

    /// DNS cache TTL in seconds
    #[arg(long, env = "DNS_CACHE_TTL", default_value = "300")]
    pub dns_cache_ttl: u64,

    /// Enable crawler-local link graph tracking for priority boosting (off by
    /// default: the per-worker graph only sees a fraction of the links)
    #[arg(long, env = "LINK_GRAPH", default_value = "false")]
    pub link_graph: bool,

    /// Link graph score computation interval (in URLs processed)
    #[arg(long, env = "LINK_GRAPH_INTERVAL", default_value = "1000")]
    pub link_graph_interval: u64,

    /// Publish link data to frontier service for centralized PageRank
    #[arg(long, env = "PUBLISH_LINKS", default_value = "false")]
    pub publish_links: bool,

    /// Enable incremental crawling (use conditional HTTP headers)
    #[arg(long, env = "INCREMENTAL_CRAWL", default_value = "true")]
    pub incremental_crawl: bool,

    /// Redis URL for crawl history persistence (enables cross-session incremental crawling)
    #[arg(long, env = "REDIS_URL")]
    pub redis_url: Option<String>,

    /// Enable browser rendering for JavaScript-heavy pages (requires --features browser)
    #[arg(long, env = "BROWSER_RENDER")]
    pub browser_render: bool,

    /// URL patterns that require browser rendering (regex, comma-separated)
    /// Example: ".*spa\.example\.com.*,.*react-app.*"
    #[arg(long, env = "BROWSER_RENDER_PATTERNS")]
    pub browser_render_patterns: Option<String>,

    /// Chrome/Chromium executable path for browser rendering
    #[arg(long, env = "CHROME_PATH")]
    pub chrome_path: Option<String>,

    /// Browser rendering timeout in seconds
    #[arg(long, env = "BROWSER_TIMEOUT", default_value = "30")]
    pub browser_timeout: u64,

    /// Maximum concurrent browser pages
    #[arg(long, env = "BROWSER_CONCURRENCY", default_value = "5")]
    pub browser_concurrency: usize,

    /// Run browser in headless mode
    #[arg(long, env = "BROWSER_HEADLESS", default_value = "true")]
    pub browser_headless: bool,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,

    /// Enable sitemap discovery from robots.txt
    #[arg(long, env = "SITEMAP_DISCOVERY", default_value = "true")]
    pub sitemap_discovery: bool,

    /// Maximum sitemap URLs to discover per domain
    #[arg(long, env = "MAX_SITEMAP_URLS", default_value = "10000")]
    pub max_sitemap_urls: usize,

    /// Allow fetching hosts that resolve to private/internal addresses
    /// (SSRF opt-out). Tests and self-hosted private-network crawls only;
    /// never enable in production. Raw-IP URLs stay refused.
    #[arg(long, env = "ALLOW_PRIVATE_IPS")]
    pub allow_private_ips: bool,
}

/// Worker metrics for monitoring
#[derive(Debug, Default)]
struct WorkerMetrics {
    urls_processed: AtomicU64,
    urls_succeeded: AtomicU64,
    urls_failed: AtomicU64,
    urls_discovered: AtomicU64,
    urls_not_modified: AtomicU64,
    urls_retried: AtomicU64,
    bytes_downloaded: AtomicU64,
    active_fetches: AtomicU64,
    dns_cache_hits: AtomicU64,
    dns_cache_misses: AtomicU64,
    browser_renders: AtomicU64,
    http_fetches: AtomicU64,
    sitemap_urls_discovered: AtomicU64,
    domains_with_sitemaps: AtomicU64,
}

impl WorkerMetrics {
    fn new() -> Self {
        Self::default()
    }

    fn record_success(&self, bytes: u64) {
        self.urls_processed.fetch_add(1, Ordering::Relaxed);
        self.urls_succeeded.fetch_add(1, Ordering::Relaxed);
        self.bytes_downloaded.fetch_add(bytes, Ordering::Relaxed);
        scrapix_core::metrics::crawler_fetches_total()
            .with_label_values(&["crawled"])
            .inc();
        scrapix_core::metrics::crawler_bytes_total().inc_by(bytes as f64);
    }

    fn record_failure(&self) {
        self.urls_processed.fetch_add(1, Ordering::Relaxed);
        self.urls_failed.fetch_add(1, Ordering::Relaxed);
        scrapix_core::metrics::crawler_fetches_total()
            .with_label_values(&["failed"])
            .inc();
    }

    fn record_not_modified(&self) {
        self.urls_processed.fetch_add(1, Ordering::Relaxed);
        self.urls_not_modified.fetch_add(1, Ordering::Relaxed);
        scrapix_core::metrics::crawler_fetches_total()
            .with_label_values(&["not_modified"])
            .inc();
    }

    fn record_retry(&self) {
        self.urls_processed.fetch_add(1, Ordering::Relaxed);
        self.urls_retried.fetch_add(1, Ordering::Relaxed);
        scrapix_core::metrics::crawler_fetches_total()
            .with_label_values(&["retry"])
            .inc();
    }

    /// Record the wall-clock duration of one fetch attempt, regardless of
    /// outcome (`scrapix_crawler_fetch_duration_seconds`).
    fn observe_fetch_duration(&self, elapsed: Duration) {
        scrapix_core::metrics::crawler_fetch_duration_seconds().observe(elapsed.as_secs_f64());
    }

    fn record_discovered(&self, count: u64) {
        self.urls_discovered.fetch_add(count, Ordering::Relaxed);
    }

    fn record_dns_stats(&self, hits: u64, misses: u64) {
        self.dns_cache_hits.store(hits, Ordering::Relaxed);
        self.dns_cache_misses.store(misses, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    fn record_browser_render(&self) {
        self.browser_renders.fetch_add(1, Ordering::Relaxed);
    }

    fn record_http_fetch(&self) {
        self.http_fetches.fetch_add(1, Ordering::Relaxed);
    }

    fn record_sitemap_discovery(&self, urls_count: u64) {
        self.sitemap_urls_discovered
            .fetch_add(urls_count, Ordering::Relaxed);
        self.domains_with_sitemaps.fetch_add(1, Ordering::Relaxed);
    }

    fn fetch_started(&self) {
        self.active_fetches.fetch_add(1, Ordering::Relaxed);
    }

    fn fetch_completed(&self) {
        self.active_fetches.fetch_sub(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            urls_processed: self.urls_processed.load(Ordering::Relaxed),
            urls_succeeded: self.urls_succeeded.load(Ordering::Relaxed),
            urls_failed: self.urls_failed.load(Ordering::Relaxed),
            urls_discovered: self.urls_discovered.load(Ordering::Relaxed),
            urls_not_modified: self.urls_not_modified.load(Ordering::Relaxed),
            urls_retried: self.urls_retried.load(Ordering::Relaxed),
            bytes_downloaded: self.bytes_downloaded.load(Ordering::Relaxed),
            active_fetches: self.active_fetches.load(Ordering::Relaxed),
            dns_cache_hits: self.dns_cache_hits.load(Ordering::Relaxed),
            dns_cache_misses: self.dns_cache_misses.load(Ordering::Relaxed),
            browser_renders: self.browser_renders.load(Ordering::Relaxed),
            http_fetches: self.http_fetches.load(Ordering::Relaxed),
            sitemap_urls_discovered: self.sitemap_urls_discovered.load(Ordering::Relaxed),
            domains_with_sitemaps: self.domains_with_sitemaps.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
struct MetricsSnapshot {
    urls_processed: u64,
    urls_succeeded: u64,
    urls_failed: u64,
    urls_discovered: u64,
    urls_not_modified: u64,
    urls_retried: u64,
    bytes_downloaded: u64,
    active_fetches: u64,
    dns_cache_hits: u64,
    dns_cache_misses: u64,
    browser_renders: u64,
    http_fetches: u64,
    sitemap_urls_discovered: u64,
    domains_with_sitemaps: u64,
}

/// The main crawler worker
struct CrawlerWorker {
    consumer: AnyConsumer,
    producer: AnyProducer,
    fetcher: HttpFetcher,
    sitemap_parser: Option<SitemapParser>,
    #[cfg(feature = "browser")]
    browser_renderer: Option<Arc<CdpRenderer>>,
    #[cfg(feature = "browser")]
    browser_patterns: Vec<regex::Regex>,
    extractor: UrlExtractor,
    #[allow(dead_code)]
    semaphore: Arc<Semaphore>,
    concurrency: usize,
    metrics: Arc<WorkerMetrics>,
    shutdown: Arc<AtomicBool>,
    worker_id: String,
    link_graph: Option<Arc<LinkGraph>>,
    link_graph_interval: u64,
    incremental_crawl: bool,
    publish_links: bool,
    /// Re-queue budget for transient failures (`MAX_RETRIES`)
    max_retries: u32,
    /// Per-job request shaping (user agents, headers, proxies, robots opt-out)
    shaper: JobFetchShaper,
    /// Redis-backed crawl history for cross-session incremental crawling
    crawl_history: Option<Arc<RedisCrawlHistory>>,
    /// Tracks which (job_id, domain) pairs have already had sitemap
    /// discovery run, so a second job on the same domain still gets its own
    /// sitemap seeds (Task 7: discovery used to be keyed by worker+domain
    /// only, so a second job on an already-seen domain got nothing).
    sitemap_seen: Arc<SitemapSeen>,
    /// Jobs cancelled or finished (from `JOB_STATUS`): their messages are
    /// acked without work (spec R5).
    cancelled: Arc<CancelledJobs>,
    /// `JOB_STATUS` consumer feeding `cancelled` (per-worker group).
    control_consumer: Option<Arc<AnyConsumer>>,
}

/// Maximum number of `(job_id, domain)` pairs [`SitemapSeen`] remembers
/// before evicting the oldest. Bounds worker memory across long-lived
/// workers that see many jobs and domains.
const SITEMAP_SEEN_CAPACITY: usize = 10_000;

/// Bounded (LRU-ish) set of `(job_id, domain)` pairs sitemap discovery has
/// already run for. `first_time` is the single entry point: it reports
/// whether this is the first time the pair is seen *and* records it,
/// atomically under one lock, so concurrent callers can't both observe
/// "not seen yet" for the same pair.
struct SitemapSeen {
    inner: parking_lot::Mutex<SitemapSeenInner>,
}

struct SitemapSeenInner {
    set: HashSet<(String, String)>,
    order: std::collections::VecDeque<(String, String)>,
    capacity: usize,
}

impl SitemapSeen {
    fn new(capacity: usize) -> Self {
        Self {
            inner: parking_lot::Mutex::new(SitemapSeenInner {
                set: HashSet::new(),
                order: std::collections::VecDeque::new(),
                capacity,
            }),
        }
    }

    /// Returns `true` the first time this `(job_id, domain)` pair is seen,
    /// `false` on every later call. Evicts the oldest pair once `capacity`
    /// is reached (a bounded ring, not a strict LRU: eviction is by
    /// insertion order, not last access).
    fn first_time(&self, job_id: &str, domain: &str) -> bool {
        let key = (job_id.to_string(), domain.to_string());
        let mut inner = self.inner.lock();
        if inner.set.contains(&key) {
            return false;
        }
        if inner.capacity > 0 && inner.order.len() >= inner.capacity {
            if let Some(oldest) = inner.order.pop_front() {
                inner.set.remove(&oldest);
            }
        }
        inner.set.insert(key.clone());
        inner.order.push_back(key);
        true
    }
}

impl CrawlerWorker {
    /// Create a new crawler worker from CLI args (uses Kafka).
    async fn new(args: &Args) -> anyhow::Result<Self> {
        let worker_id = args
            .worker_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string());

        info!(worker_id = %worker_id, "Initializing crawler worker");
        if args.worker_id.is_none() {
            warn!(
                worker_id = %worker_id,
                "WORKER_ID is not set: using a random id, so the job-control consumer group \
                 changes on every restart (controls published while down are missed and old \
                 groups leak). Set a stable WORKER_ID per worker instance."
            );
        }

        // Create Kafka consumer
        let kafka_consumer = ConsumerBuilder::new(&args.brokers, &args.group_id)
            .client_id(format!("scrapix-crawler-{}", worker_id))
            .auto_offset_reset("earliest")
            .build()?;

        // Subscribe to processing topic (URLs ready to crawl after dedup/politeness)
        kafka_consumer.subscribe(&[topic_names::URL_PROCESSING])?;
        info!(
            topic = topic_names::URL_PROCESSING,
            "Subscribed to processing topic"
        );

        // Create Kafka producer
        let kafka_producer = ProducerBuilder::new(&args.brokers)
            .client_id(format!("scrapix-crawler-{}-producer", worker_id))
            .compression("lz4")
            .build()?;

        // Job control: a per-worker group, so every worker sees every
        // cancel; `latest` so a new group does not replay the history.
        let control_group = control_group_id(&args.group_id, &worker_id);
        let control = ConsumerBuilder::new(&args.brokers, &control_group)
            .client_id(format!("scrapix-crawler-{}-control", worker_id))
            .auto_offset_reset("latest")
            .build()?;
        control.subscribe(&[topic_names::JOB_STATUS])?;
        info!(topic = topic_names::JOB_STATUS, group = %control_group, "Subscribed to job control topic");

        let consumer = AnyConsumer::from(kafka_consumer);
        let producer = AnyProducer::from(kafka_producer);

        let mut worker = Self::build(args, worker_id, consumer, producer).await?;
        worker.control_consumer = Some(Arc::new(AnyConsumer::from(control)));
        Ok(worker)
    }

    /// Create a new crawler worker using pre-built `AnyProducer`/`AnyConsumer` (for `scrapix all`).
    /// `control`, when given, must already be subscribed to `JOB_STATUS`.
    pub async fn with_bus(
        args: &Args,
        producer: AnyProducer,
        consumer: AnyConsumer,
        control: Option<AnyConsumer>,
    ) -> anyhow::Result<Self> {
        let worker_id = args
            .worker_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string());

        info!(worker_id = %worker_id, "Initializing crawler worker (in-process bus)");

        consumer.subscribe(&[topic_names::URL_PROCESSING])?;
        info!(
            topic = topic_names::URL_PROCESSING,
            "Subscribed to processing topic"
        );

        let mut worker = Self::build(args, worker_id, consumer, producer).await?;
        worker.control_consumer = control.map(Arc::new);
        Ok(worker)
    }

    /// Shared construction logic (everything except bus creation and subscription).
    async fn build(
        args: &Args,
        worker_id: String,
        consumer: AnyConsumer,
        producer: AnyProducer,
    ) -> anyhow::Result<Self> {
        // Create robots.txt cache configuration
        let robots_config = RobotsConfig {
            user_agent: args.user_agent.clone(),
            cache_ttl: Duration::from_secs(3600), // 1 hour
            fetch_timeout: Duration::from_secs(10),
            respect_robots: args.respect_robots,
            default_crawl_delay_ms: None,
            // Same SSRF policy as the fetcher (ALLOW_PRIVATE_IPS, default off).
            allow_private_ips: args.allow_private_ips,
        };

        // Create simple in-memory robots cache for the HTTP fetcher
        let fetcher_robots_cache = Arc::new(RobotsCache::new(robots_config.clone())?);

        // Create sitemap parser if enabled
        let sitemap_parser = if args.sitemap_discovery {
            info!("Sitemap discovery enabled");
            Some(SitemapParser::new(SitemapConfig {
                max_urls: args.max_sitemap_urls,
                allow_private_ips: args.allow_private_ips,
                ..SitemapConfig::default()
            }))
        } else {
            None
        };

        // Create HTTP fetcher with optional DNS caching.
        //
        // The fetcher retries 429/5xx and transport errors once in-process;
        // `MAX_RETRIES` is the *re-queue* budget handled by the worker
        // (outcome::classify), so the two do not multiply into
        // (MAX_RETRIES + 1)^2 attempts. The in-process wait is capped at 5s
        // so a long `Retry-After` does not pin a fetch slot: the re-queue
        // delay honors the full hint instead.
        let mut fetcher_builder = HttpFetcherBuilder::new()
            .user_agent(&args.user_agent)
            .timeout(Duration::from_secs(args.timeout))
            .max_retries(1)
            .max_backoff(Duration::from_secs(5))
            .max_body_size(args.max_body_size_mb * 1024 * 1024)
            .allow_private_ips(args.allow_private_ips);
        if args.allow_private_ips {
            warn!("ALLOW_PRIVATE_IPS is set: SSRF protection for private addresses is off");
        }

        if args.dns_cache {
            fetcher_builder =
                fetcher_builder.dns_cache_ttl(Duration::from_secs(args.dns_cache_ttl));
            info!(ttl_secs = args.dns_cache_ttl, "DNS caching enabled");
        }

        let fetcher = fetcher_builder.build(fetcher_robots_cache.clone())?;

        // Create URL extractor
        let extractor_config = ExtractorConfig {
            patterns: None,
            max_depth: args.max_depth,
            follow_external: args.follow_external,
            follow_subdomains: true,
            extract_from_data_attrs: false,
            allowed_domains: vec![],
        };
        let extractor = UrlExtractor::new(extractor_config);

        // Create link graph if enabled
        let link_graph = if args.link_graph {
            info!("Link graph tracking enabled");
            Some(Arc::new(LinkGraph::with_defaults()))
        } else {
            None
        };

        // Create browser renderer if enabled
        #[cfg(feature = "browser")]
        let (browser_renderer, browser_patterns) = if args.browser_render {
            // Parse browser render patterns
            let patterns: Vec<regex::Regex> = args
                .browser_render_patterns
                .as_ref()
                .map(|p| {
                    p.split(',')
                        .filter_map(|pattern| {
                            let pattern = pattern.trim();
                            if pattern.is_empty() {
                                return None;
                            }
                            match regex::Regex::new(pattern) {
                                Ok(r) => Some(r),
                                Err(e) => {
                                    warn!(
                                        pattern = pattern,
                                        error = %e,
                                        "Failed to compile browser render pattern"
                                    );
                                    None
                                }
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            info!(
                patterns_count = patterns.len(),
                "Browser render patterns compiled"
            );

            // Create CDP renderer
            // The renderer checks robots.txt (same cache as the HTTP
            // fetcher) and the SSRF rules before every navigation.
            let mut cdp_builder = CdpRendererBuilder::new()
                .robots_cache(fetcher_robots_cache.clone())
                .allow_private_ips(args.allow_private_ips)
                .timeout(Duration::from_secs(args.browser_timeout))
                .max_concurrent_pages(args.browser_concurrency)
                .headless(args.browser_headless);

            if let Some(ref chrome_path) = args.chrome_path {
                cdp_builder = cdp_builder.executable_path(chrome_path);
            }

            match cdp_builder.build().await {
                Ok(renderer) => {
                    info!(
                        headless = args.browser_headless,
                        concurrency = args.browser_concurrency,
                        timeout_secs = args.browser_timeout,
                        "Browser renderer initialized"
                    );
                    (Some(Arc::new(renderer)), patterns)
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "Failed to initialize browser renderer; jobs requiring a browser will fail on this worker"
                    );
                    (None, Vec::new())
                }
            }
        } else {
            (None, Vec::new())
        };

        #[cfg(not(feature = "browser"))]
        if args.browser_render {
            warn!("Browser rendering requested but 'browser' feature is not enabled. Compile with --features browser");
        }

        // Create concurrency limiter
        let semaphore = Arc::new(Semaphore::new(args.concurrency));

        // Create Redis crawl history for incremental crawling if Redis URL is provided
        let crawl_history = if args.incremental_crawl {
            if let Some(ref redis_url) = args.redis_url {
                match RedisStorage::with_url(redis_url.as_str()).await {
                    Ok(storage) => {
                        info!(redis_url = %redis_url, "Redis crawl history enabled for incremental crawling");
                        Some(Arc::new(RedisCrawlHistory::with_defaults(storage)))
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to connect to Redis for crawl history, incremental crawling will use in-message headers only");
                        None
                    }
                }
            } else {
                debug!("No REDIS_URL configured, incremental crawling will use in-message headers only");
                None
            }
        } else {
            None
        };

        Ok(Self {
            consumer,
            producer,
            fetcher,
            sitemap_parser,
            #[cfg(feature = "browser")]
            browser_renderer,
            #[cfg(feature = "browser")]
            browser_patterns,
            extractor,
            semaphore,
            concurrency: args.concurrency,
            metrics: Arc::new(WorkerMetrics::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            worker_id,
            link_graph,
            link_graph_interval: args.link_graph_interval,
            incremental_crawl: args.incremental_crawl,
            publish_links: args.publish_links,
            max_retries: args.max_retries,
            shaper: JobFetchShaper::default(),
            crawl_history,
            sitemap_seen: Arc::new(SitemapSeen::new(SITEMAP_SEEN_CAPACITY)),
            cancelled: Arc::new(CancelledJobs::default()),
            control_consumer: None,
        })
    }

    /// Run the crawler worker
    async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        info!(worker_id = %self.worker_id, "Starting crawler worker main loop");

        // Start metrics reporter
        let metrics = self.metrics.clone();
        let shutdown = self.shutdown.clone();
        let link_graph = self.link_graph.clone();
        let metrics_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            while !shutdown.load(Ordering::Relaxed) {
                interval.tick().await;
                let snapshot = metrics.snapshot();

                // Log link graph stats if enabled
                let (link_graph_pages, link_graph_links) = if let Some(ref graph) = link_graph {
                    let stats = graph.stats();
                    (stats.page_count, stats.link_count)
                } else {
                    (0, 0)
                };

                info!(
                    processed = snapshot.urls_processed,
                    succeeded = snapshot.urls_succeeded,
                    failed = snapshot.urls_failed,
                    not_modified = snapshot.urls_not_modified,
                    retried = snapshot.urls_retried,
                    discovered = snapshot.urls_discovered,
                    sitemap_urls = snapshot.sitemap_urls_discovered,
                    sitemap_domains = snapshot.domains_with_sitemaps,
                    bytes_mb = snapshot.bytes_downloaded / (1024 * 1024),
                    active = snapshot.active_fetches,
                    http_fetches = snapshot.http_fetches,
                    browser_renders = snapshot.browser_renders,
                    dns_hits = snapshot.dns_cache_hits,
                    dns_misses = snapshot.dns_cache_misses,
                    link_graph_pages = link_graph_pages,
                    link_graph_links = link_graph_links,
                    "Worker metrics"
                );
            }
        });

        let control_handle = match self.control_consumer {
            Some(ref c) => Some(scrapix_queue::control::spawn_listener(
                c.clone(),
                self.cancelled.clone(),
                self.shutdown.clone(),
            )),
            None => {
                warn!("No job control consumer: cancelled jobs are not skipped");
                None
            }
        };

        // Process messages using concurrent processing to maintain heartbeats
        let result = self.clone().process_messages().await;

        // Cleanup
        self.shutdown.store(true, Ordering::Relaxed);
        metrics_handle.abort();
        if let Some(h) = control_handle {
            h.abort();
        }

        result
    }

    /// Process messages from the frontier queue.
    ///
    /// Uses ack-based concurrent processing: each message's offset is
    /// committed only after [`CrawlerWorker::handle_message`] acked it, i.e.
    /// after every publish for it succeeded.
    async fn process_messages(self: Arc<Self>) -> anyhow::Result<()> {
        let concurrency = self.concurrency;
        let shutdown = self.shutdown.clone();
        let worker = self.clone();

        info!(
            concurrency = concurrency,
            worker_id = %self.worker_id,
            "Starting message processing loop"
        );

        self.consumer
            .process_with_ack::<UrlMessage, _, _>(
                move |msg, metadata, ack| {
                    let worker = worker.clone();
                    async move {
                        debug!(
                            url = %msg.url.url,
                            job_id = %msg.job_id,
                            partition = metadata.partition,
                            offset = metadata.offset,
                            retry_count = msg.url.retry_count,
                            "Received URL from frontier"
                        );
                        worker.handle_message(msg, ack).await;
                    }
                },
                concurrency,
                shutdown,
            )
            .await?;

        Ok(())
    }

    /// Whether `parent` is the message that owns `(job_id, domain)`'s
    /// one-time sitemap discovery.
    ///
    /// This is a synchronous, side-effecting decision (it consumes
    /// `sitemap_seen`'s one-shot dedup slot for the pair), so the caller
    /// must call it at most once per page, before publishing that page's
    /// `PageCrawled` (R-18): the result is exactly what `PageCrawled`'s
    /// `sitemap_pending` flag must carry, and true here obligates the
    /// caller to spawn [`Self::maybe_discover_sitemaps`], which then
    /// guarantees a matching `SitemapPublished`.
    ///
    /// `parent.job.sitemap.enabled` (when the message carries a job spec)
    /// decides whether discovery runs at all; `enabled == false` skips it
    /// entirely, even if this worker's `SITEMAP_DISCOVERY` default is on.
    /// No job spec at all (legacy/test messages) falls back to this
    /// worker's `SITEMAP_DISCOVERY` default (whether a sitemap parser was
    /// built).
    fn sitemap_discovery_should_run(
        &self,
        job_id: &str,
        domain: &str,
        parent: &UrlMessage,
    ) -> bool {
        let enabled = match parent.job.as_ref() {
            Some(job) => job.sitemap.enabled,
            None => self.sitemap_parser.is_some(),
        };
        if !enabled {
            return false;
        }
        // Dedupe per (job_id, domain): a second job on an already-seen
        // domain still gets its own sitemap seeds.
        self.sitemap_seen.first_time(job_id, domain)
    }

    /// Run `(job_id, domain)`'s sitemap discovery and publish every accepted
    /// URL to the frontier as a child of `parent`.
    ///
    /// Only called after [`Self::sitemap_discovery_should_run`] returned
    /// true for this exact `parent`, so this method's only job is to run
    /// discovery and **unconditionally** publish `SitemapPublished` for
    /// `parent.message_id` — with `count: 0` on every empty/error/disabled
    /// path (no parser built, fetch error, nothing found, everything
    /// filtered out) — so `JobAccounting`'s `pending_sitemaps` is guaranteed
    /// to eventually get its matching `settled_sitemaps` entry and the job
    /// can balance (R-18). This is why the actual discovery work lives in
    /// [`Self::run_sitemap_discovery`], which never returns an `Err`: every
    /// failure mode collapses to a `0` count instead, so there is exactly
    /// one exit path from this function and it always publishes. This
    /// method itself cannot fail either — it always returns the discovered
    /// count, publish failures included (they're logged, not propagated).
    ///
    /// Sitemap URLs are derived from `parent` via `UrlMessage::child`, so
    /// they carry every job-scoped field (job spec, limits, features, ...).
    /// Runs in a background task spawned after a successful fetch, off the
    /// hot path (R9).
    ///
    /// Entries are filtered by [`url_allowed`], the same URL-pattern matcher
    /// link extraction uses, and by [`is_non_page_url_for`] honoring the
    /// job's PDF/document opt-ins, so a PDF (or `.docx`, ...) sitemap entry
    /// is kept when the job enables that format.
    async fn maybe_discover_sitemaps(&self, domain: &str, parent: &UrlMessage) -> usize {
        let job_id = parent.job_id.as_str();

        let discovered_count = self.run_sitemap_discovery(domain, parent).await;

        if discovered_count > 0 {
            self.metrics
                .record_sitemap_discovery(discovered_count as u64);
            info!(
                domain,
                discovered_count, "Published sitemap URLs to frontier"
            );

            let timestamp = chrono::Utc::now().timestamp_millis();
            let event = CrawlEvent::UrlsDiscovered {
                job_id: job_id.to_string(),
                source_url: format!("https://{domain}/sitemap.xml"),
                count: discovered_count,
                timestamp,
            };
            if let Err(e) = self.publish_event(job_id, &event).await {
                debug!(domain, error = %e, "Failed to publish sitemap discovery event");
            }
        }

        // Counted separately from `UrlsDiscovered` so job completion
        // accounting (links_published, sitemaps_settled) can attribute
        // sitemap-seeded URLs to the message that triggered discovery (D1,
        // R-18). Published unconditionally — see the doc comment above.
        let timestamp = chrono::Utc::now().timestamp_millis();
        let sitemap_published = CrawlEvent::SitemapPublished {
            job_id: job_id.to_string(),
            count: discovered_count,
            url_message_id: parent.message_id.clone(),
            timestamp,
        };
        if let Err(e) = self.publish_event(job_id, &sitemap_published).await {
            warn!(
                domain,
                error = %e,
                "Failed to publish SitemapPublished event; this job's work accounting may never balance for this domain"
            );
        }

        discovered_count
    }

    /// Fetch, filter and publish this domain's sitemap URLs to the
    /// frontier. Returns the count actually published to the frontier.
    ///
    /// Never fails outward: a missing parser, a fetch error, an empty
    /// result, or every entry being filtered out all collapse to `0`, so
    /// [`Self::maybe_discover_sitemaps`] can always publish
    /// `SitemapPublished` afterward without an extra branch for "did
    /// discovery even run".
    async fn run_sitemap_discovery(&self, domain: &str, parent: &UrlMessage) -> usize {
        let explicit_urls: &[String] = match parent.job.as_ref() {
            Some(job) => job.sitemap.urls.as_slice(),
            None => &[],
        };

        let sitemap_parser = match &self.sitemap_parser {
            Some(parser) => parser,
            None => return 0,
        };

        let sitemap_entries = if !explicit_urls.is_empty() {
            let mut all_urls = Vec::new();
            for url in explicit_urls {
                match sitemap_parser.fetch_and_parse(url).await {
                    Ok(urls) => all_urls.extend(urls),
                    Err(e) => {
                        debug!(domain, sitemap_url = %url, error = %e, "Failed to fetch job-specified sitemap");
                    }
                }
            }
            all_urls
        } else {
            // Use the same full discovery as /map: fetch robots.txt, follow all sub-sitemaps
            let base_url = format!("https://{domain}");
            match sitemap_parser.discover_all_urls(&base_url).await {
                Ok(urls) => urls,
                Err(e) => {
                    debug!(domain, error = %e, "Sitemap discovery failed");
                    return 0;
                }
            }
        };

        info!(
            domain,
            found = sitemap_entries.len(),
            "Discovered URLs from sitemaps"
        );

        let (pdf_enabled, documents_enabled) =
            parent.features.as_ref().map_or((false, false), |f| {
                (f.is_pdf_enabled(), f.is_documents_enabled())
            });

        let mut discovered_count = 0;
        for sitemap_entry in sitemap_entries {
            if !sitemap_entry_allowed(
                &sitemap_entry.loc,
                parent.url_patterns.as_ref(),
                pdf_enabled,
                documents_enabled,
            ) {
                continue;
            }

            let mut crawl_url = CrawlUrl::seed(&sitemap_entry.loc);
            crawl_url.parent_url = Some(format!("sitemap:{domain}"));

            if let Some(priority) = sitemap_entry.priority {
                crawl_url.priority = (priority * 100.0) as i32;
            }

            let url_msg = parent.child(crawl_url);

            if let Err(e) = self
                .producer
                .send(
                    topic_names::URL_FRONTIER,
                    Some(&url_msg.partition_key()),
                    &url_msg,
                )
                .await
            {
                warn!(url = sitemap_entry.loc, error = %e, "Failed to publish sitemap URL");
            } else {
                discovered_count += 1;
            }
        }

        discovered_count
    }

    /// Publish a crawl event
    async fn publish_event(&self, job_id: &str, event: &CrawlEvent) -> scrapix_core::Result<()> {
        self.producer
            .send(topic_names::EVENTS, Some(job_id), event)
            .await?;
        Ok(())
    }
}

/// Whether a sitemap entry should be published to the frontier: not a
/// non-page resource (respecting the job's PDF/document opt-ins) and allowed by the
/// job's URL patterns — the same [`url_allowed`] matcher link extraction
/// uses, so a sitemap entry and a discovered link are judged identically.
///
/// `patterns` is `None` when the job set no `url_patterns` at all, in which
/// case every non-filtered-extension URL is allowed (matching link
/// extraction's behavior for a job without patterns).
fn sitemap_entry_allowed(
    loc: &str,
    patterns: Option<&UrlPatterns>,
    pdf_enabled: bool,
    documents_enabled: bool,
) -> bool {
    if is_non_page_url_for(loc, pdf_enabled, documents_enabled) {
        return false;
    }
    match patterns {
        Some(patterns) => url_allowed(patterns, loc),
        None => true,
    }
}

/// Run the crawler worker with the given arguments.
pub async fn run(args: Args) -> anyhow::Result<()> {
    info!(
        concurrency = args.concurrency,
        brokers = %args.brokers,
        group_id = %args.group_id,
        dns_cache = args.dns_cache,
        link_graph = args.link_graph,
        incremental_crawl = args.incremental_crawl,
        browser_render = args.browser_render,
        "Starting Scrapix crawler worker"
    );

    // Create and run worker
    let worker = Arc::new(CrawlerWorker::new(&args).await?);

    // Install SIGTERM + Ctrl-C handler — flips worker.shutdown on either signal.
    let signal_handle = install_signal_handlers(worker.shutdown.clone());

    // Bare-TCP wake listener on WAKE_PORT (default 8081) so Fly.io's proxy can
    // autostart this machine from a suspended state when the API fans out
    // wake requests on POST /crawl.
    let wake_handle = spawn_wake_listener(wake_port_from_env(), worker.shutdown.clone());

    // Idle watchdog: if no URLs processed for IDLE_EXIT_MINUTES (default 10),
    // exit cleanly so Fly can suspend this machine to zero cost.
    let idle_metrics = worker.metrics.clone();
    let idle_handle = spawn_idle_watchdog(
        move || idle_metrics.urls_processed.load(Ordering::Relaxed),
        idle_minutes_from_env(10.0),
        worker.shutdown.clone(),
    );

    // Clone for metrics access after run completes
    let worker_for_metrics = worker.clone();

    // Run the worker (consumes the Arc)
    let result = worker.run().await;

    // Cleanup
    signal_handle.abort();
    wake_handle.abort();
    idle_handle.abort();

    // Print final metrics
    let metrics = worker_for_metrics.metrics.snapshot();
    info!(
        processed = metrics.urls_processed,
        succeeded = metrics.urls_succeeded,
        failed = metrics.urls_failed,
        not_modified = metrics.urls_not_modified,
        discovered = metrics.urls_discovered,
        bytes_mb = metrics.bytes_downloaded / (1024 * 1024),
        http_fetches = metrics.http_fetches,
        browser_renders = metrics.browser_renders,
        dns_hits = metrics.dns_cache_hits,
        dns_misses = metrics.dns_cache_misses,
        "Final worker metrics"
    );

    // Print link graph stats if enabled
    if let Some(ref graph) = worker_for_metrics.link_graph {
        let stats = graph.stats();
        info!(
            pages = stats.page_count,
            links = stats.link_count,
            avg_score = format!("{:.4}", stats.avg_score),
            "Final link graph stats"
        );
    }

    result
}

/// Run the crawler worker using pre-built message bus trait objects.
///
/// Used by `scrapix all` to run the crawler in-process alongside other services.
pub async fn run_with_bus(
    args: Args,
    producer: AnyProducer,
    consumer: AnyConsumer,
    control: Option<AnyConsumer>,
) -> anyhow::Result<()> {
    info!(
        concurrency = args.concurrency,
        "Starting Scrapix crawler worker (in-process bus)"
    );

    let worker = Arc::new(CrawlerWorker::with_bus(&args, producer, consumer, control).await?);
    let worker_for_metrics = worker.clone();

    let result = worker.run().await;

    let metrics = worker_for_metrics.metrics.snapshot();
    info!(
        processed = metrics.urls_processed,
        succeeded = metrics.urls_succeeded,
        failed = metrics.urls_failed,
        "Final crawler worker metrics (in-process)"
    );

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sitemap_discovery_is_keyed_by_job_and_domain() {
        let seen = SitemapSeen::new(10);
        assert!(seen.first_time("job1", "a.test"));
        assert!(!seen.first_time("job1", "a.test"));
        assert!(seen.first_time("job2", "a.test"));
    }

    #[test]
    fn sitemap_seen_evicts_oldest_pair_once_capacity_is_reached() {
        let seen = SitemapSeen::new(2);
        assert!(seen.first_time("job1", "a.test"));
        assert!(seen.first_time("job2", "a.test"));
        // Third distinct pair evicts the first (job1, a.test).
        assert!(seen.first_time("job3", "a.test"));
        assert!(seen.first_time("job1", "a.test"));
    }

    #[test]
    fn sitemap_entry_matches_auto_generated_include_pattern() {
        // Same matcher link extraction uses (extractor::url_allowed):
        // an auto-generated `https://host/path/*` include pattern must
        // accept the equivalent sitemap entry.
        let patterns = UrlPatterns {
            include: vec!["https://docs.a.test/guide/*".into()],
            ..Default::default()
        };
        assert!(sitemap_entry_allowed(
            "https://docs.a.test/guide/intro",
            Some(&patterns),
            false,
            false
        ));
        assert!(!sitemap_entry_allowed(
            "https://docs.a.test/blog/post",
            Some(&patterns),
            false,
            false
        ));
    }

    #[test]
    fn sitemap_entry_pdf_survives_filter_when_job_enables_pdf() {
        assert!(!sitemap_entry_allowed(
            "https://a.test/doc.pdf",
            None,
            false,
            false
        ));
        assert!(sitemap_entry_allowed(
            "https://a.test/doc.pdf",
            None,
            true,
            false
        ));
    }

    #[test]
    fn sitemap_entry_document_survives_filter_when_job_enables_documents() {
        assert!(!sitemap_entry_allowed(
            "https://a.test/r.docx",
            None,
            true,
            false
        ));
        assert!(sitemap_entry_allowed(
            "https://a.test/r.docx",
            None,
            false,
            true
        ));
    }
}
