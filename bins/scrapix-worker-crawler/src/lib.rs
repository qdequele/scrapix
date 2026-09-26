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

use scrapix_core::CrawlUrl;
#[cfg(feature = "browser")]
use scrapix_crawler::{CdpRenderer, CdpRendererBuilder};
use scrapix_crawler::{
    ExtractorConfig, HttpFetcher, HttpFetcherBuilder, RobotsCache, RobotsConfig, SitemapConfig,
    SitemapParser, UrlExtractor,
};
use scrapix_frontier::LinkGraph;
use scrapix_queue::{
    topic_names, AnyConsumer, AnyProducer, ConsumerBuilder, CrawlEvent, ProducerBuilder, UrlMessage,
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
    }

    fn record_failure(&self) {
        self.urls_processed.fetch_add(1, Ordering::Relaxed);
        self.urls_failed.fetch_add(1, Ordering::Relaxed);
    }

    fn record_not_modified(&self) {
        self.urls_processed.fetch_add(1, Ordering::Relaxed);
        self.urls_not_modified.fetch_add(1, Ordering::Relaxed);
    }

    fn record_retry(&self) {
        self.urls_processed.fetch_add(1, Ordering::Relaxed);
        self.urls_retried.fetch_add(1, Ordering::Relaxed);
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
    /// Tracks domains we've already discovered sitemaps for
    discovered_sitemap_domains: Arc<parking_lot::RwLock<std::collections::HashSet<String>>>,
}

impl CrawlerWorker {
    /// Create a new crawler worker from CLI args (uses Kafka).
    async fn new(args: &Args) -> anyhow::Result<Self> {
        let worker_id = args
            .worker_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string());

        info!(worker_id = %worker_id, "Initializing crawler worker");

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

        let consumer = AnyConsumer::from(kafka_consumer);
        let producer = AnyProducer::from(kafka_producer);

        Self::build(args, worker_id, consumer, producer).await
    }

    /// Create a new crawler worker using pre-built `AnyProducer`/`AnyConsumer` (for `scrapix all`).
    pub async fn with_bus(
        args: &Args,
        producer: AnyProducer,
        consumer: AnyConsumer,
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

        Self::build(args, worker_id, consumer, producer).await
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
            discovered_sitemap_domains: Arc::new(parking_lot::RwLock::new(HashSet::new())),
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

        // Process messages using concurrent processing to maintain heartbeats
        let result = self.clone().process_messages().await;

        // Cleanup
        self.shutdown.store(true, Ordering::Relaxed);
        metrics_handle.abort();

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

    /// Discover and publish sitemap URLs for a domain (if not already done).
    ///
    /// Sitemap URLs are derived from `parent` via `UrlMessage::child`, so
    /// they carry every job-scoped field (job spec, limits, features, ...).
    /// Runs in a background task spawned after a successful fetch; Task 7
    /// replaces this with per-job discovery.
    async fn maybe_discover_sitemaps(
        &self,
        domain: &str,
        parent: &UrlMessage,
    ) -> scrapix_core::Result<usize> {
        let job_id = parent.job_id.as_str();
        let url_patterns = parent.url_patterns.clone();
        // Check if we've already discovered sitemaps for this domain
        {
            let domains = self.discovered_sitemap_domains.read();
            if domains.contains(domain) {
                return Ok(0);
            }
        }

        // Mark as discovered (even before we try, to avoid duplicate work)
        {
            let mut domains = self.discovered_sitemap_domains.write();
            domains.insert(domain.to_string());
        }

        let sitemap_parser = match &self.sitemap_parser {
            Some(parser) => parser,
            None => return Ok(0),
        };

        // Use the same full discovery as /map: fetch robots.txt, follow all sub-sitemaps
        let base_url = format!("https://{domain}");
        let sitemap_entries = match sitemap_parser.discover_all_urls(&base_url).await {
            Ok(urls) => urls,
            Err(e) => {
                debug!(domain, error = %e, "Sitemap discovery failed");
                return Ok(0);
            }
        };

        info!(
            domain,
            found = sitemap_entries.len(),
            "Discovered URLs from sitemaps"
        );

        let mut discovered_count = 0;
        for sitemap_entry in sitemap_entries {
            // Filter non-page URLs (images, PDFs, CSS, JS, fonts, etc.)
            if scrapix_crawler::is_non_page_url(&sitemap_entry.loc) {
                continue;
            }

            // Filter by allowed_domains whitelist
            if let Some(ref patterns) = url_patterns {
                if !patterns.allowed_domains.is_empty() {
                    if let Ok(parsed_url) = url::Url::parse(&sitemap_entry.loc) {
                        if let Some(url_domain) = parsed_url.host_str() {
                            if !patterns
                                .allowed_domains
                                .iter()
                                .any(|d| d.eq_ignore_ascii_case(url_domain))
                            {
                                continue;
                            }
                        }
                    }
                }
            }

            // Filter by include/exclude glob patterns
            if let Some(ref patterns) = url_patterns {
                let matches_include = patterns.include.is_empty()
                    || patterns.include.iter().any(|p| {
                        if p.contains("**") {
                            let parts: Vec<&str> = p.split("**").collect();
                            parts.len() == 2
                                && sitemap_entry.loc.starts_with(parts[0])
                                && (parts[1].is_empty() || sitemap_entry.loc.ends_with(parts[1]))
                        } else {
                            sitemap_entry.loc == *p
                        }
                    });
                let matches_exclude = patterns.exclude.iter().any(|p| {
                    if p.contains("**") {
                        let parts: Vec<&str> = p.split("**").collect();
                        parts.len() == 2
                            && sitemap_entry.loc.starts_with(parts[0])
                            && (parts[1].is_empty() || sitemap_entry.loc.ends_with(parts[1]))
                    } else {
                        sitemap_entry.loc == *p
                    }
                });
                if !matches_include || matches_exclude {
                    continue;
                }
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

        if discovered_count > 0 {
            self.metrics
                .record_sitemap_discovery(discovered_count as u64);
            info!(
                domain,
                discovered_count, "Published sitemap URLs to frontier"
            );

            let event = CrawlEvent::UrlsDiscovered {
                job_id: job_id.to_string(),
                source_url: format!("https://{domain}/sitemap.xml"),
                count: discovered_count,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            if let Err(e) = self.publish_event(job_id, &event).await {
                debug!(domain, error = %e, "Failed to publish sitemap discovery event");
            }
        }

        Ok(discovered_count)
    }

    /// Publish a crawl event
    async fn publish_event(&self, job_id: &str, event: &CrawlEvent) -> scrapix_core::Result<()> {
        self.producer
            .send(topic_names::EVENTS, Some(job_id), event)
            .await?;
        Ok(())
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
) -> anyhow::Result<()> {
    info!(
        concurrency = args.concurrency,
        "Starting Scrapix crawler worker (in-process bus)"
    );

    let worker = Arc::new(CrawlerWorker::with_bus(&args, producer, consumer).await?);
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
