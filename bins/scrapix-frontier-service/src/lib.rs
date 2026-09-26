//! Scrapix Frontier Service
//!
//! URL frontier management backed by a [`FrontierStore`] (Redis in
//! production, in-memory for tests and `scrapix all`).
//!
//! ## Responsibilities
//!
//! 1. Consume URLs from the frontier topic and admit them into the store
//!    (dedup, `max_depth`, exact `max_pages` budget, queue capacity — all
//!    decided atomically by `FrontierStore::admit`). The input message is
//!    acked only once `admit` returned `Ok`.
//! 2. Keep one job template per job (the first `UrlMessage` seen for it) so
//!    that every dispatched URL is `template.child(url)` and carries every
//!    job-scoped field (source, account, incremental, job spec, ...).
//! 3. Dispatch ready URLs to the processing topic, per job, only while this
//!    instance holds the job's dispatch lease, honoring per-domain
//!    politeness (URLs that may not be fetched yet go back via `requeue`).
//! 4. Publish `CrawlEvent::FrontierProgress` snapshots of the store
//!    counters.
//!
//! ## Architecture
//!
//! ```text
//! URL_FRONTIER → admit(store) → [lease] pop_ready → [Politeness] → URL_PROCESSING
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use parking_lot::Mutex;
use scrapix_lifecycle::{
    idle_minutes_from_env, install_signal_handlers, spawn_idle_watchdog, spawn_wake_listener,
    wake_port_from_env,
};
use tracing::{debug, error, info, warn};

use scrapix_core::{Ack, CrawlUrl};
use scrapix_frontier::{
    extract_domain, Admission, CrawlRecord, FrontierStore, JobCounters, JobRunState, LinkGraph,
    LinkGraphConfig, MemoryFrontierStore, PolitenessConfig, PolitenessScheduler, RecrawlConfig,
    RecrawlDecision, RecrawlScheduler, UrlHistory, UrlHistoryConfig,
};
use scrapix_queue::{
    topic_names, AnyConsumer, AnyProducer, ConsumerBuilder, CrawlEvent, CrawlHistoryMessage,
    LinksMessage, ProducerBuilder, UrlMessage,
};

/// How many input messages are admitted concurrently.
const ADMIT_CONCURRENCY: usize = 64;
/// How many times a failing `admit` is retried before the message is left
/// un-acked (and so redelivered after a restart/rebalance).
const ADMIT_ATTEMPTS: u32 = 3;
/// Per-job dispatch lease TTL; renewed on every dispatcher tick.
const LEASE_TTL: Duration = Duration::from_secs(5);
/// How often `FrontierProgress` is published (for jobs whose counters changed).
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// Upper bound on the per-process job caches (parsed templates, initialized jobs).
const JOB_CACHE_CAP: usize = 1024;

/// Scrapix Frontier Service
#[derive(Parser, Debug)]
#[command(name = "scrapix-frontier-service")]
#[command(
    version,
    about = "URL frontier management service with deduplication and politeness"
)]
pub struct Args {
    /// Kafka/Redpanda broker addresses
    #[arg(short, long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
    pub brokers: String,

    /// Consumer group ID
    #[arg(
        short,
        long,
        env = "KAFKA_GROUP_ID",
        default_value = "scrapix-frontier"
    )]
    pub group_id: String,

    /// Redis/DragonflyDB URL for the durable frontier store. Without it the
    /// frontier state lives in memory and is lost on restart.
    #[arg(long, env = "REDIS_URL")]
    pub redis_url: Option<String>,

    /// Key prefix for the Redis frontier store
    #[arg(long, env = "FRONTIER_KEY_PREFIX", default_value = "scrapix:frontier")]
    pub frontier_key_prefix: String,

    /// How long a finished/cancelled job's counters and state stay queryable
    /// after its queue and seen set are released (hours)
    #[arg(long, env = "JOB_RETENTION_HOURS", default_value = "168")]
    pub job_retention_hours: u64,

    /// Deprecated and ignored: dedup now lives in the frontier store.
    #[arg(long, env = "BLOOM_CAPACITY", default_value = "10000000")]
    pub bloom_capacity: usize,

    /// Deprecated and ignored: dedup now lives in the frontier store.
    #[arg(long, env = "BLOOM_FP_RATE", default_value = "0.01")]
    pub bloom_fp_rate: f64,

    /// Default delay between requests to the same domain (ms)
    #[arg(long, env = "DOMAIN_DELAY_MS", default_value = "50")]
    pub domain_delay_ms: u64,

    /// Maximum concurrent requests per domain
    #[arg(long, env = "CONCURRENT_PER_DOMAIN", default_value = "50")]
    pub concurrent_per_domain: usize,

    /// URL dispatch batch size
    #[arg(long, env = "DISPATCH_BATCH_SIZE", default_value = "2000")]
    pub dispatch_batch_size: usize,

    /// Dispatch interval (ms)
    #[arg(long, env = "DISPATCH_INTERVAL_MS", default_value = "20")]
    pub dispatch_interval_ms: u64,

    /// Maximum pending URLs per job (queue capacity passed to `admit`)
    #[arg(long, env = "MAX_PENDING_PER_JOB", default_value = "1000000")]
    pub max_pending_per_job: usize,

    /// Service instance ID
    #[arg(long, env = "INSTANCE_ID")]
    pub instance_id: Option<String>,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,

    // === LINK GRAPH OPTIONS ===
    /// Enable PageRank-based prioritization
    #[arg(long, env = "ENABLE_LINKGRAPH", default_value = "false")]
    pub enable_linkgraph: bool,

    /// PageRank damping factor (0.0-1.0)
    #[arg(long, env = "LINKGRAPH_DAMPING", default_value = "0.85")]
    pub linkgraph_damping: f64,

    /// Maximum priority boost from PageRank
    #[arg(long, env = "LINKGRAPH_MAX_BOOST", default_value = "50")]
    pub linkgraph_max_boost: i32,

    /// Maximum pages to track in link graph (0 = unlimited)
    #[arg(long, env = "LINKGRAPH_MAX_PAGES", default_value = "10000000")]
    pub linkgraph_max_pages: usize,

    /// PageRank computation interval in seconds
    #[arg(long, env = "LINKGRAPH_COMPUTE_INTERVAL", default_value = "300")]
    pub linkgraph_compute_interval: u64,

    // === RECRAWL OPTIONS ===
    /// Enable incremental recrawl scheduling
    #[arg(long, env = "ENABLE_RECRAWL", default_value = "false")]
    pub enable_recrawl: bool,

    /// Minimum age before allowing recrawl (seconds)
    #[arg(long, env = "RECRAWL_MIN_AGE", default_value = "3600")]
    pub recrawl_min_age: u64,

    /// Maximum age before forcing recrawl (seconds)
    #[arg(long, env = "RECRAWL_MAX_AGE", default_value = "604800")]
    pub recrawl_max_age: u64,

    /// Maximum URLs to track in history (0 = unlimited)
    #[arg(long, env = "RECRAWL_MAX_URLS", default_value = "10000000")]
    pub recrawl_max_urls: usize,
}

/// Build the frontier store selected by `args`: Redis when `redis_url` is
/// set, otherwise an in-memory store (with a warning, since its state is
/// lost on restart and not shared between instances).
pub async fn build_store(args: &Args) -> anyhow::Result<Arc<dyn FrontierStore>> {
    match args.redis_url.as_deref() {
        Some(url) if !url.is_empty() => {
            let store =
                scrapix_frontier::store::RedisFrontierStore::new(url, &args.frontier_key_prefix)
                    .await?;
            info!(prefix = %args.frontier_key_prefix, "Frontier state stored in Redis");
            Ok(Arc::new(store))
        }
        _ => {
            warn!("REDIS_URL not set: frontier state is in memory and will be lost on restart");
            Ok(Arc::new(MemoryFrontierStore::default()))
        }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A small bounded map. When full, an arbitrary entry is evicted — every
/// value is cheap to recompute from the store, so eviction only costs a
/// store round trip.
struct BoundedCache<V> {
    map: Mutex<HashMap<String, V>>,
    cap: usize,
}

impl<V: Clone> BoundedCache<V> {
    fn new(cap: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            cap,
        }
    }

    fn get(&self, key: &str) -> Option<V> {
        self.map.lock().get(key).cloned()
    }

    fn insert(&self, key: &str, value: V) {
        let mut map = self.map.lock();
        if map.len() >= self.cap && !map.contains_key(key) {
            if let Some(victim) = map.keys().next().cloned() {
                map.remove(&victim);
            }
        }
        map.insert(key.to_string(), value);
    }

    fn remove(&self, key: &str) {
        self.map.lock().remove(key);
    }
}

#[derive(Debug, Default)]
struct ServiceMetrics {
    messages_consumed: AtomicU64,
    urls_received: AtomicU64,
    urls_new: AtomicU64,
    urls_duplicate: AtomicU64,
    urls_dispatched: AtomicU64,
    urls_delayed: AtomicU64,
    urls_recrawl_skipped: AtomicU64,
    admit_errors: AtomicU64,
    active_jobs: AtomicU64,
    active_domains: AtomicU64,
    links_recorded: AtomicU64,
    history_updates: AtomicU64,
}

impl ServiceMetrics {
    fn new() -> Self {
        Self::default()
    }

    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            messages_consumed: self.messages_consumed.load(Ordering::Relaxed),
            urls_received: self.urls_received.load(Ordering::Relaxed),
            urls_new: self.urls_new.load(Ordering::Relaxed),
            urls_duplicate: self.urls_duplicate.load(Ordering::Relaxed),
            urls_dispatched: self.urls_dispatched.load(Ordering::Relaxed),
            urls_delayed: self.urls_delayed.load(Ordering::Relaxed),
            urls_recrawl_skipped: self.urls_recrawl_skipped.load(Ordering::Relaxed),
            admit_errors: self.admit_errors.load(Ordering::Relaxed),
            active_jobs: self.active_jobs.load(Ordering::Relaxed),
            active_domains: self.active_domains.load(Ordering::Relaxed),
            links_recorded: self.links_recorded.load(Ordering::Relaxed),
            history_updates: self.history_updates.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
struct MetricsSnapshot {
    messages_consumed: u64,
    urls_received: u64,
    urls_new: u64,
    urls_duplicate: u64,
    urls_dispatched: u64,
    urls_delayed: u64,
    urls_recrawl_skipped: u64,
    admit_errors: u64,
    active_jobs: u64,
    active_domains: u64,
    links_recorded: u64,
    history_updates: u64,
}

struct FrontierService {
    consumer: Arc<AnyConsumer>,
    links_consumer: Option<Arc<AnyConsumer>>,
    history_consumer: Option<Arc<AnyConsumer>>,
    producer: Arc<AnyProducer>,
    store: Arc<dyn FrontierStore>,
    /// Parsed job templates (from `store.job_template`), used on dispatch.
    templates: BoundedCache<Arc<UrlMessage>>,
    /// Jobs this process already ran `ensure_job` (+ first `set_state`) for.
    initialized: BoundedCache<()>,
    /// Jobs whose dispatch lease this instance held on the last tick.
    held_leases: Mutex<HashSet<String>>,
    politeness: Arc<PolitenessScheduler>,
    link_graph: Option<Arc<LinkGraph>>,
    recrawl_scheduler: Option<Arc<RecrawlScheduler>>,
    url_history: Option<Arc<UrlHistory>>,
    metrics: Arc<ServiceMetrics>,
    shutdown: Arc<AtomicBool>,
    instance_id: String,
    queue_cap: usize,
    dispatch_batch_size: usize,
    dispatch_interval: Duration,
    linkgraph_compute_interval: Duration,
}

/// Link graph and recrawl components built from `Args`.
struct Extras {
    link_graph: Option<Arc<LinkGraph>>,
    recrawl_scheduler: Option<Arc<RecrawlScheduler>>,
    url_history: Option<Arc<UrlHistory>>,
}

fn build_extras(args: &Args) -> Extras {
    let link_graph = if args.enable_linkgraph {
        let config = LinkGraphConfig {
            damping_factor: args.linkgraph_damping,
            max_priority_boost: args.linkgraph_max_boost,
            max_pages: args.linkgraph_max_pages,
            ..Default::default()
        };
        info!(
            damping = args.linkgraph_damping,
            max_boost = args.linkgraph_max_boost,
            max_pages = args.linkgraph_max_pages,
            "LinkGraph enabled for PageRank-based prioritization"
        );
        Some(Arc::new(LinkGraph::new(config)))
    } else {
        None
    };

    let (recrawl_scheduler, url_history) = if args.enable_recrawl {
        let history_config = UrlHistoryConfig {
            max_entries: args.recrawl_max_urls,
            min_recrawl_interval: Duration::from_secs(args.recrawl_min_age),
            max_recrawl_interval: Duration::from_secs(args.recrawl_max_age),
            ..Default::default()
        };
        let history = Arc::new(UrlHistory::new(history_config));
        let recrawl_config = RecrawlConfig {
            enabled: true,
            min_age: Duration::from_secs(args.recrawl_min_age),
            max_age: Duration::from_secs(args.recrawl_max_age),
            ..Default::default()
        };
        let scheduler = Arc::new(RecrawlScheduler::new(recrawl_config, history.clone()));
        info!(
            min_age_secs = args.recrawl_min_age,
            max_age_secs = args.recrawl_max_age,
            max_urls = args.recrawl_max_urls,
            "RecrawlScheduler enabled for incremental crawling"
        );
        (Some(scheduler), Some(history))
    } else {
        (None, None)
    };

    Extras {
        link_graph,
        recrawl_scheduler,
        url_history,
    }
}

impl FrontierService {
    async fn new(args: &Args) -> anyhow::Result<Self> {
        let instance_id = args
            .instance_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string());

        info!(instance_id = %instance_id, "Initializing frontier service");

        let kafka_consumer = ConsumerBuilder::new(&args.brokers, &args.group_id)
            .client_id(format!("scrapix-frontier-{}", instance_id))
            .auto_offset_reset("earliest")
            .build()?;
        kafka_consumer.subscribe(&[topic_names::URL_FRONTIER])?;
        info!(
            topic = topic_names::URL_FRONTIER,
            "Subscribed to frontier topic"
        );
        let consumer: Arc<AnyConsumer> = Arc::new(AnyConsumer::from(kafka_consumer));

        let kafka_producer = ProducerBuilder::new(&args.brokers)
            .client_id(format!("scrapix-frontier-{}-producer", instance_id))
            .compression("lz4")
            .build()?;
        let producer: Arc<AnyProducer> = Arc::new(AnyProducer::from(kafka_producer));

        let links_consumer = if args.enable_linkgraph {
            let c = ConsumerBuilder::new(&args.brokers, format!("{}-links", args.group_id))
                .client_id(format!("scrapix-frontier-{}-links", instance_id))
                .auto_offset_reset("earliest")
                .build()?;
            c.subscribe(&[topic_names::LINKS])?;
            info!(topic = topic_names::LINKS, "Subscribed to links topic");
            Some(Arc::new(AnyConsumer::from(c)))
        } else {
            None
        };

        let history_consumer = if args.enable_recrawl {
            let c = ConsumerBuilder::new(&args.brokers, format!("{}-history", args.group_id))
                .client_id(format!("scrapix-frontier-{}-history", instance_id))
                .auto_offset_reset("earliest")
                .build()?;
            c.subscribe(&[topic_names::CRAWL_HISTORY])?;
            info!(
                topic = topic_names::CRAWL_HISTORY,
                "Subscribed to crawl history topic"
            );
            Some(Arc::new(AnyConsumer::from(c)))
        } else {
            None
        };

        let store = build_store(args).await?;
        Ok(Self::build(
            args,
            instance_id,
            producer,
            consumer,
            links_consumer,
            history_consumer,
            store,
        ))
    }

    /// Create a `FrontierService` from pre-built message bus trait objects
    /// and frontier store.
    ///
    /// Used by `scrapix all` (shared in-process bus) and by tests (with a
    /// `MemoryFrontierStore`).
    pub async fn with_bus(
        args: &Args,
        producer: Arc<AnyProducer>,
        main_consumer: Arc<AnyConsumer>,
        links_consumer: Option<Arc<AnyConsumer>>,
        history_consumer: Option<Arc<AnyConsumer>>,
        store: Arc<dyn FrontierStore>,
    ) -> anyhow::Result<Self> {
        let instance_id = args
            .instance_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string());

        info!(instance_id = %instance_id, "Initializing frontier service (pre-built bus)");

        Ok(Self::build(
            args,
            instance_id,
            producer,
            main_consumer,
            links_consumer,
            history_consumer,
            store,
        ))
    }

    fn build(
        args: &Args,
        instance_id: String,
        producer: Arc<AnyProducer>,
        main_consumer: Arc<AnyConsumer>,
        links_consumer: Option<Arc<AnyConsumer>>,
        history_consumer: Option<Arc<AnyConsumer>>,
        store: Arc<dyn FrontierStore>,
    ) -> Self {
        let politeness = PolitenessScheduler::new(PolitenessConfig {
            default_delay_ms: args.domain_delay_ms,
            min_delay_ms: 100,
            max_delay_ms: 30_000,
            respect_robots_delay: true,
            robots_delay_multiplier: 1.0,
            concurrent_per_domain: args.concurrent_per_domain,
        });

        let extras = build_extras(args);

        Self {
            consumer: main_consumer,
            links_consumer,
            history_consumer,
            producer,
            store,
            templates: BoundedCache::new(JOB_CACHE_CAP),
            initialized: BoundedCache::new(JOB_CACHE_CAP),
            held_leases: Mutex::new(HashSet::new()),
            politeness: Arc::new(politeness),
            link_graph: extras.link_graph,
            recrawl_scheduler: extras.recrawl_scheduler,
            url_history: extras.url_history,
            metrics: Arc::new(ServiceMetrics::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            instance_id,
            queue_cap: args.max_pending_per_job,
            dispatch_batch_size: args.dispatch_batch_size,
            dispatch_interval: Duration::from_millis(args.dispatch_interval_ms),
            linkgraph_compute_interval: Duration::from_secs(args.linkgraph_compute_interval),
        }
    }

    async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        info!(instance_id = %self.instance_id, "Starting frontier service main loop");

        let metrics_handle = self.start_metrics_logger();
        let dispatcher_handle = self.clone().start_dispatcher();
        let progress_handle = self.clone().start_progress_publisher();
        let links_handle = self.start_links_consumer();
        let history_handle = self.start_history_consumer();
        let pagerank_handle = self.start_pagerank_computer();

        let result = self.clone().process_messages().await;

        self.shutdown.store(true, Ordering::Relaxed);
        metrics_handle.abort();
        dispatcher_handle.abort();
        progress_handle.abort();
        for h in [links_handle, history_handle, pagerank_handle]
            .into_iter()
            .flatten()
        {
            h.abort();
        }

        result
    }

    fn start_metrics_logger(&self) -> tokio::task::JoinHandle<()> {
        let metrics = self.metrics.clone();
        let link_graph = self.link_graph.clone();
        let url_history = self.url_history.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            while !shutdown.load(Ordering::Relaxed) {
                interval.tick().await;
                let snapshot = metrics.snapshot();
                info!(
                    consumed = snapshot.messages_consumed,
                    received = snapshot.urls_received,
                    new = snapshot.urls_new,
                    duplicate = snapshot.urls_duplicate,
                    dispatched = snapshot.urls_dispatched,
                    delayed = snapshot.urls_delayed,
                    recrawl_skipped = snapshot.urls_recrawl_skipped,
                    admit_errors = snapshot.admit_errors,
                    links_recorded = snapshot.links_recorded,
                    history_updates = snapshot.history_updates,
                    jobs = snapshot.active_jobs,
                    domains = snapshot.active_domains,
                    "Frontier metrics"
                );

                if let Some(ref graph) = link_graph {
                    let stats = graph.stats();
                    info!(
                        pages = stats.page_count,
                        links = stats.link_count,
                        avg_inbound = format!("{:.2}", stats.avg_inbound),
                        "LinkGraph stats"
                    );
                }

                if let Some(ref history) = url_history {
                    let stats = history.stats();
                    info!(
                        tracked_urls = stats.tracked_urls,
                        total_crawls = stats.total_crawls,
                        total_changes = stats.total_changes,
                        avg_change_rate = format!("{:.2}", stats.avg_change_rate),
                        "Recrawl stats"
                    );
                }
            }
        })
    }

    /// Consume `URL_FRONTIER`, admitting each URL into the store. The message
    /// is acked only after `admit` returned `Ok` (whatever the admission
    /// outcome); a store error leaves it un-acked so it is redelivered.
    async fn process_messages(self: Arc<Self>) -> anyhow::Result<()> {
        let shutdown = self.shutdown.clone();
        let service = self.clone();
        self.consumer
            .process_with_ack::<UrlMessage, _, _>(
                move |msg, metadata, ack| {
                    let service = service.clone();
                    async move {
                        debug!(
                            url = %msg.url.url,
                            job_id = %msg.job_id,
                            depth = msg.url.depth,
                            retry_count = msg.url.retry_count,
                            partition = metadata.partition,
                            "Received URL"
                        );
                        service.handle_input(msg, ack).await;
                    }
                },
                ADMIT_CONCURRENCY,
                shutdown,
            )
            .await?;
        Ok(())
    }

    async fn handle_input(&self, msg: UrlMessage, ack: Ack) {
        self.metrics
            .messages_consumed
            .fetch_add(1, Ordering::Relaxed);
        self.metrics.urls_received.fetch_add(1, Ordering::Relaxed);

        let Some(url) = self.boost(msg.url.clone()) else {
            // Recrawl scheduler decided the URL is fresh enough: done.
            ack.ack();
            return;
        };

        for attempt in 1..=ADMIT_ATTEMPTS {
            match self.admit(&msg, &url).await {
                Ok(admission) => {
                    if admission == Admission::Admitted {
                        self.metrics.urls_new.fetch_add(1, Ordering::Relaxed);
                        debug!(url = %url.url, job_id = %msg.job_id, "URL admitted");
                    } else {
                        self.metrics.urls_duplicate.fetch_add(1, Ordering::Relaxed);
                        debug!(
                            url = %url.url,
                            job_id = %msg.job_id,
                            admission = ?admission,
                            "URL not admitted"
                        );
                    }
                    ack.ack();
                    return;
                }
                Err(e) => {
                    self.metrics.admit_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        url = %url.url,
                        job_id = %msg.job_id,
                        attempt,
                        error = %e,
                        "Frontier store admit failed"
                    );
                    // Re-run job initialization on the next attempt: the
                    // error may be a job the store no longer knows about.
                    self.initialized.remove(&msg.job_id);
                    if attempt < ADMIT_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
                    }
                }
            }
        }
        error!(
            url = %url.url,
            job_id = %msg.job_id,
            "Giving up on admit; leaving the message un-acked for redelivery"
        );
        drop(ack);
    }

    /// Apply the recrawl scheduler and link-graph boosts to an incoming URL.
    /// `None` means the recrawl scheduler says to skip it.
    fn boost(&self, mut url: CrawlUrl) -> Option<CrawlUrl> {
        if let Some(ref scheduler) = self.recrawl_scheduler {
            match scheduler.should_crawl(&url) {
                RecrawlDecision::Crawl {
                    priority_boost,
                    reason,
                    ..
                } => {
                    url.priority += priority_boost;
                    debug!(
                        url = %url.url,
                        reason = %reason,
                        priority_boost = priority_boost,
                        "Recrawl decision: crawl"
                    );
                }
                RecrawlDecision::Skip {
                    reason,
                    retry_after,
                } => {
                    self.metrics
                        .urls_recrawl_skipped
                        .fetch_add(1, Ordering::Relaxed);
                    debug!(
                        url = %url.url,
                        reason = %reason,
                        retry_after_secs = retry_after.map(|d| d.as_secs()),
                        "Recrawl decision: skip"
                    );
                    return None;
                }
            }
        }

        if let Some(ref graph) = self.link_graph {
            let boost = graph.get_priority_boost(&url.url);
            if boost > 0 {
                url.priority += boost;
                debug!(url = %url.url, pagerank_boost = boost, "Applied PageRank priority boost");
            }
        }
        Some(url)
    }

    async fn admit(&self, msg: &UrlMessage, url: &CrawlUrl) -> scrapix_core::Result<Admission> {
        if self.initialized.get(&msg.job_id).is_none() {
            self.init_job(msg).await?;
            self.initialized.insert(&msg.job_id, ());
        }
        self.store.admit(&msg.job_id, url, self.queue_cap).await
    }

    /// Make sure the store knows the job: store `msg` as its template (first
    /// writer wins) and, for a job that was never started, mark it
    /// `Running`. A job that is paused/cancelled/finished is left alone, so
    /// late URLs never resurrect it.
    async fn init_job(&self, msg: &UrlMessage) -> scrapix_core::Result<()> {
        let job_id = &msg.job_id;
        let fresh = match self.store.state(job_id).await? {
            None => true,
            // `ensure_job` creates jobs `Paused`: a paused job that never
            // received a URL is one whose creation was interrupted before
            // `set_state(Running)` (this message was then redelivered).
            Some(JobRunState::Paused) => self.store.counters(job_id).await?.received == 0,
            Some(_) => false,
        };
        let template = serde_json::to_string(msg)?;
        self.store
            .ensure_job(job_id, &template, msg.max_pages, msg.max_depth)
            .await?;
        if fresh {
            self.store.set_state(job_id, JobRunState::Running).await?;
            info!(
                job_id = %job_id,
                index_uid = %msg.index_uid,
                max_depth = ?msg.max_depth,
                max_pages = ?msg.max_pages,
                "New frontier job"
            );
        }
        Ok(())
    }

    /// The parsed template of `job_id`, from the cache or the store. `None`
    /// if the store has none (released job) or it does not parse.
    async fn template(&self, job_id: &str) -> Option<Arc<UrlMessage>> {
        if let Some(t) = self.templates.get(job_id) {
            return Some(t);
        }
        let json = match self.store.job_template(job_id).await {
            Ok(Some(json)) => json,
            Ok(None) => return None,
            Err(e) => {
                warn!(job_id = %job_id, error = %e, "Failed to load job template");
                return None;
            }
        };
        match serde_json::from_str::<UrlMessage>(&json) {
            Ok(t) => {
                let t = Arc::new(t);
                self.templates.insert(job_id, t.clone());
                Some(t)
            }
            Err(e) => {
                error!(job_id = %job_id, error = %e, "Job template does not parse");
                None
            }
        }
    }

    fn start_dispatcher(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(self.dispatch_interval);
            while !self.shutdown.load(Ordering::Relaxed) {
                tick.tick().await;
                self.dispatch_tick().await;
            }
        })
    }

    /// One dispatcher pass over every active job this instance can lease.
    async fn dispatch_tick(&self) {
        let jobs = match self.store.active_jobs().await {
            Ok(jobs) => jobs,
            Err(e) => {
                warn!(error = %e, "Failed to list active frontier jobs");
                return;
            }
        };
        self.metrics
            .active_jobs
            .store(jobs.len() as u64, Ordering::Relaxed);

        let mut held = HashSet::new();
        for job_id in jobs {
            match self
                .store
                .try_lease(&job_id, &self.instance_id, LEASE_TTL)
                .await
            {
                Ok(true) => {
                    held.insert(job_id.clone());
                }
                Ok(false) => continue,
                Err(e) => {
                    warn!(job_id = %job_id, error = %e, "Failed to acquire dispatch lease");
                    continue;
                }
            }
            if let Err(e) = self.dispatch_job(&job_id).await {
                warn!(job_id = %job_id, error = %e, "Dispatch failed");
            }
        }
        *self.held_leases.lock() = held;

        let domain_count = self.politeness.tracked_domains().len() as u64;
        self.metrics
            .active_domains
            .store(domain_count, Ordering::Relaxed);
    }

    /// Pop a batch of ready URLs for `job_id` and dispatch those politeness
    /// allows; the rest go back to the store via `requeue` (counters
    /// untouched, so they are never counted twice against `max_pages`).
    async fn dispatch_job(&self, job_id: &str) -> scrapix_core::Result<()> {
        if self.store.state(job_id).await? != Some(JobRunState::Running) {
            return Ok(());
        }
        let Some(template) = self.template(job_id).await else {
            return Ok(());
        };
        let now = now_ms();
        let urls = self
            .store
            .pop_ready(job_id, self.dispatch_batch_size, now)
            .await?;
        if urls.is_empty() {
            return Ok(());
        }

        let mut bounced = Vec::new();
        for mut url in urls {
            let domain = extract_domain(&url.url);
            if !self.politeness.can_fetch(&domain) {
                self.metrics.urls_delayed.fetch_add(1, Ordering::Relaxed);
                // Park it until the domain is expected to be fetchable, so the
                // next ticks don't pop and requeue it over and over.
                let wait = self.politeness.wait_time(&domain);
                url.not_before_ms = (!wait.is_zero()).then(|| now + wait.as_millis() as i64);
                bounced.push(url);
                continue;
            }

            self.politeness.start_request(&domain);
            let msg = template.child(url);
            match self
                .producer
                .send(
                    topic_names::URL_PROCESSING,
                    Some(&msg.partition_key()),
                    &msg,
                )
                .await
            {
                Ok(_) => {
                    self.metrics.urls_dispatched.fetch_add(1, Ordering::Relaxed);
                    debug!(url = %msg.url.url, job_id = %job_id, "Dispatched URL for crawling");
                    // R-3: the politeness slot is released at dispatch
                    // success for now (Task 12 moves it to crawler feedback).
                    self.politeness.complete_request(&domain);
                }
                Err(e) => {
                    error!(url = %msg.url.url, job_id = %job_id, error = %e, "Failed to dispatch URL");
                    self.politeness.failed_request(&domain, false);
                    let mut url = msg.url;
                    url.not_before_ms = Some(now + 1_000);
                    bounced.push(url);
                }
            }
        }

        if !bounced.is_empty() {
            self.store.requeue(job_id, bounced).await?;
        }
        Ok(())
    }

    fn start_progress_publisher(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
            let mut last: HashMap<String, (JobCounters, u64)> = HashMap::new();
            while !self.shutdown.load(Ordering::Relaxed) {
                tick.tick().await;
                self.publish_progress(&mut last).await;
            }
        })
    }

    /// Publish `FrontierProgress` for every job whose lease this instance
    /// holds and whose counters (or queue depth) changed since the last
    /// publish.
    async fn publish_progress(&self, last: &mut HashMap<String, (JobCounters, u64)>) {
        let held: Vec<String> = self.held_leases.lock().iter().cloned().collect();
        last.retain(|job, _| held.contains(job));
        for job_id in held {
            let snapshot = match (
                self.store.counters(&job_id).await,
                self.store.queued(&job_id).await,
            ) {
                (Ok(c), Ok(q)) => (c, q),
                (Err(e), _) | (_, Err(e)) => {
                    debug!(job_id = %job_id, error = %e, "Failed to read frontier counters");
                    continue;
                }
            };
            if last.get(&job_id) == Some(&snapshot) {
                continue;
            }
            let (c, queued) = &snapshot;
            let event = CrawlEvent::FrontierProgress {
                job_id: job_id.clone(),
                instance_id: self.instance_id.clone(),
                received: c.received,
                admitted: c.admitted,
                dispatched: c.dispatched,
                rejected: c.rejected,
                dropped: c.dropped,
                queued: *queued,
                timestamp: now_ms(),
            };
            match self
                .producer
                .send(topic_names::EVENTS, Some(&job_id), &event)
                .await
            {
                Ok(_) => {
                    last.insert(job_id, snapshot);
                }
                Err(e) => {
                    debug!(job_id = %job_id, error = %e, "Failed to publish FrontierProgress")
                }
            }
        }
    }

    fn start_links_consumer(&self) -> Option<tokio::task::JoinHandle<()>> {
        let consumer = self.links_consumer.clone()?;
        let link_graph = self.link_graph.clone()?;
        let metrics = Arc::clone(&self.metrics);
        let shutdown = Arc::clone(&self.shutdown);

        Some(tokio::spawn(async move {
            while !shutdown.load(Ordering::Relaxed) {
                match consumer
                    .poll_one::<LinksMessage>(Duration::from_millis(100))
                    .await
                {
                    Ok(Some(msg)) => {
                        link_graph.record_links(&msg.source_url, msg.target_urls.clone());
                        metrics
                            .links_recorded
                            .fetch_add(msg.target_urls.len() as u64, Ordering::Relaxed);
                        debug!(
                            source = %msg.source_url,
                            links_count = msg.target_urls.len(),
                            "Recorded links in graph"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!(error = %e, "Error polling links topic");
                    }
                }
            }
        }))
    }

    fn start_history_consumer(&self) -> Option<tokio::task::JoinHandle<()>> {
        let consumer = self.history_consumer.clone()?;
        let url_history = self.url_history.clone()?;
        let metrics = Arc::clone(&self.metrics);
        let shutdown = Arc::clone(&self.shutdown);

        Some(tokio::spawn(async move {
            while !shutdown.load(Ordering::Relaxed) {
                match consumer
                    .poll_one::<CrawlHistoryMessage>(Duration::from_millis(100))
                    .await
                {
                    Ok(Some(msg)) => {
                        let mut record = CrawlRecord::new().with_status(msg.status);

                        if let Some(etag) = msg.etag {
                            record = record.with_etag(etag);
                        }
                        if let Some(last_modified) = msg.last_modified {
                            record = record.with_last_modified(last_modified);
                        }
                        if let Some(content_hash) = msg.content_hash {
                            record = record.with_content_hash(content_hash);
                        }
                        if let Some(content_length) = msg.content_length {
                            record = record.with_content_length(content_length);
                        }

                        url_history.record_crawl(&msg.url, record);
                        metrics.history_updates.fetch_add(1, Ordering::Relaxed);
                        debug!(
                            url = %msg.url,
                            content_changed = msg.content_changed,
                            "Recorded crawl history"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!(error = %e, "Error polling history topic");
                    }
                }
            }
        }))
    }

    fn start_pagerank_computer(&self) -> Option<tokio::task::JoinHandle<()>> {
        let link_graph = self.link_graph.clone()?;
        let shutdown = Arc::clone(&self.shutdown);
        let interval = self.linkgraph_compute_interval;

        Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);

            while !shutdown.load(Ordering::Relaxed) {
                tick.tick().await;

                let start = std::time::Instant::now();
                link_graph.compute_scores();
                let duration = start.elapsed();

                let stats = link_graph.stats();
                info!(
                    pages = stats.page_count,
                    links = stats.link_count,
                    duration_ms = duration.as_millis(),
                    max_score = format!("{:.6}", stats.max_score),
                    "Recomputed PageRank scores"
                );
            }
        }))
    }

    async fn print_stats(&self) {
        let metrics = self.metrics.snapshot();
        info!(
            consumed = metrics.messages_consumed,
            received = metrics.urls_received,
            new = metrics.urls_new,
            duplicate = metrics.urls_duplicate,
            dispatched = metrics.urls_dispatched,
            delayed = metrics.urls_delayed,
            admit_errors = metrics.admit_errors,
            "Final frontier metrics"
        );

        let held: Vec<String> = self.held_leases.lock().iter().cloned().collect();
        for job_id in held {
            if let (Ok(c), Ok(queued)) = (
                self.store.counters(&job_id).await,
                self.store.queued(&job_id).await,
            ) {
                info!(
                    job_id = %job_id,
                    received = c.received,
                    admitted = c.admitted,
                    dispatched = c.dispatched,
                    rejected = c.rejected,
                    dropped = c.dropped,
                    queued,
                    "Job stats"
                );
            }
        }
    }
}

/// Run the frontier service with the given arguments.
pub async fn run(args: Args) -> anyhow::Result<()> {
    info!(
        brokers = %args.brokers,
        group_id = %args.group_id,
        domain_delay_ms = args.domain_delay_ms,
        "Starting Scrapix frontier service"
    );

    let service = Arc::new(FrontierService::new(&args).await?);

    let signal_handle = install_signal_handlers(service.shutdown.clone());
    let wake_handle = spawn_wake_listener(wake_port_from_env(), service.shutdown.clone());

    let idle_metrics = service.metrics.clone();
    let idle_handle = spawn_idle_watchdog(
        move || idle_metrics.messages_consumed.load(Ordering::Relaxed),
        idle_minutes_from_env(10.0),
        service.shutdown.clone(),
    );

    let result = service.clone().run().await;

    signal_handle.abort();
    wake_handle.abort();
    idle_handle.abort();
    service.print_stats().await;

    result
}

/// Run the frontier service using pre-built message bus trait objects and
/// frontier store (see [`build_store`]).
///
/// Used by `scrapix all` to run the frontier in-process alongside other services,
/// sharing an in-process channel bus instead of Kafka.
pub async fn run_with_bus(
    args: Args,
    producer: Arc<AnyProducer>,
    main_consumer: Arc<AnyConsumer>,
    links_consumer: Option<Arc<AnyConsumer>>,
    history_consumer: Option<Arc<AnyConsumer>>,
    store: Arc<dyn FrontierStore>,
) -> anyhow::Result<()> {
    info!(
        domain_delay_ms = args.domain_delay_ms,
        "Starting Scrapix frontier service (in-process bus)"
    );

    let service = Arc::new(
        FrontierService::with_bus(
            &args,
            producer,
            main_consumer,
            links_consumer,
            history_consumer,
            store,
        )
        .await?,
    );

    let result = service.clone().run().await;

    service.print_stats().await;

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrapix_core::JobSpec;
    use scrapix_queue::{ChannelBus, CrawlEvent};
    use std::time::Instant;

    fn test_args() -> Args {
        let mut args = Args::parse_from(["scrapix-frontier-service"]);
        args.instance_id = Some("test-frontier".to_string());
        args
    }

    fn seed_message(job_id: &str, max_pages: Option<u64>) -> UrlMessage {
        let spec = JobSpec {
            per_domain_delay_ms: 1234,
            user_agents: vec!["ua-test".to_string()],
            ..JobSpec::default()
        };
        UrlMessage::new(CrawlUrl::seed("https://example.com/"), job_id, "idx")
            .account("acct-42")
            .with_source(Some("src-7".to_string()))
            .with_incremental(false)
            .with_job(Some(spec))
            .with_limits(None, max_pages)
    }

    struct Harness {
        bus: ChannelBus,
        service: Arc<FrontierService>,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Harness {
        async fn start() -> Self {
            let bus = ChannelBus::new();
            let producer = Arc::new(AnyProducer::channel(bus.producer()));
            let consumer = Arc::new(AnyConsumer::channel(bus.consumer()));
            consumer.subscribe(&[topic_names::URL_FRONTIER]).unwrap();
            let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
            let service = Arc::new(
                FrontierService::with_bus(&test_args(), producer, consumer, None, None, store)
                    .await
                    .unwrap(),
            );
            let handle = tokio::spawn(service.clone().run());
            Self {
                bus,
                service,
                handle,
            }
        }

        async fn publish(&self, msg: &UrlMessage) {
            AnyProducer::channel(self.bus.producer())
                .send(topic_names::URL_FRONTIER, Some(&msg.job_id), msg)
                .await
                .unwrap();
        }

        fn consumer(&self, topic: &str) -> AnyConsumer {
            let c = AnyConsumer::channel(self.bus.consumer());
            c.subscribe(&[topic]).unwrap();
            c
        }

        /// Collect every `UrlMessage` on `URL_PROCESSING` until `deadline`
        /// passes.
        async fn collect_dispatched(&self, window: Duration) -> Vec<UrlMessage> {
            let c = self.consumer(topic_names::URL_PROCESSING);
            let deadline = Instant::now() + window;
            let mut out = Vec::new();
            while Instant::now() < deadline {
                if let Some(m) = c
                    .poll_one::<UrlMessage>(Duration::from_millis(50))
                    .await
                    .unwrap()
                {
                    out.push(m);
                }
            }
            out
        }

        fn stop(self) {
            self.service.shutdown.store(true, Ordering::Relaxed);
            self.handle.abort();
        }
    }

    #[tokio::test]
    async fn dispatches_child_messages_that_keep_job_context_and_respect_max_pages() {
        let h = Harness::start().await;
        let seed = seed_message("job-ctx", Some(3));
        h.publish(&seed).await;
        for i in 0..5 {
            let child = seed.child(CrawlUrl::new(format!("https://example.com/p{i}"), 1));
            h.publish(&child).await;
        }

        let dispatched = h.collect_dispatched(Duration::from_secs(2)).await;
        h.stop();

        assert_eq!(
            dispatched.len(),
            3,
            "max_pages=3 must dispatch exactly 3 URLs, got {:?}",
            dispatched.iter().map(|m| &m.url.url).collect::<Vec<_>>()
        );
        let mut ids = std::collections::HashSet::new();
        for m in &dispatched {
            assert_eq!(m.job_id, "job-ctx");
            assert_eq!(m.index_uid, "idx");
            assert_eq!(m.source.as_deref(), Some("src-7"));
            assert_eq!(m.account_id.as_deref(), Some("acct-42"));
            assert!(!m.incremental, "incremental=false must survive dispatch");
            assert_eq!(m.job, seed.job, "JobSpec must survive dispatch");
            assert_eq!(m.max_pages, Some(3));
            assert!(ids.insert(m.message_id.clone()), "fresh message_id each");
        }
        let urls: std::collections::HashSet<_> =
            dispatched.iter().map(|m| m.url.url.clone()).collect();
        assert_eq!(urls.len(), 3, "each URL dispatched once");
    }

    #[tokio::test]
    async fn retry_of_already_dispatched_url_is_readmitted() {
        let h = Harness::start().await;
        let seed = seed_message("job-retry", None);
        h.publish(&seed).await;
        let first = h.collect_dispatched(Duration::from_millis(600)).await;
        assert_eq!(first.len(), 1);

        // A plain duplicate is filtered...
        h.publish(&seed.child(CrawlUrl::seed("https://example.com/")))
            .await;
        // ...but a crawler re-queue (retry_count > 0) is admitted again.
        let mut retry_url = CrawlUrl::seed("https://example.com/");
        retry_url.retry_count = 1;
        h.publish(&seed.child(retry_url)).await;

        let second = h.collect_dispatched(Duration::from_millis(800)).await;
        h.stop();
        assert_eq!(second.len(), 1, "only the retry is re-dispatched");
        assert_eq!(second[0].url.url, "https://example.com/");
        assert_eq!(second[0].url.retry_count, 1);
        assert_eq!(second[0].account_id.as_deref(), Some("acct-42"));
    }

    #[tokio::test]
    async fn publishes_frontier_progress_with_cumulative_counters() {
        let h = Harness::start().await;
        let events = h.consumer(topic_names::EVENTS);
        let seed = seed_message("job-progress", None);
        h.publish(&seed).await;
        h.publish(&seed.child(CrawlUrl::new("https://example.com/a", 1)))
            .await;
        h.publish(&seed.child(CrawlUrl::new("https://example.com/b", 1)))
            .await;
        // duplicate -> rejected
        h.publish(&seed.child(CrawlUrl::new("https://example.com/a", 1)))
            .await;

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut last = None;
        while Instant::now() < deadline {
            if let Some(CrawlEvent::FrontierProgress {
                job_id,
                instance_id,
                received,
                admitted,
                dispatched,
                rejected,
                dropped,
                queued,
                timestamp,
            }) = events
                .poll_one::<CrawlEvent>(Duration::from_millis(50))
                .await
                .unwrap()
            {
                assert_eq!(job_id, "job-progress");
                assert_eq!(instance_id, "test-frontier");
                assert!(timestamp > 0);
                let snapshot = (received, admitted, dispatched, rejected, dropped, queued);
                if snapshot == (4, 3, 3, 1, 0, 0) {
                    last = Some(snapshot);
                    break;
                }
                last = Some(snapshot);
            }
        }
        h.stop();
        assert_eq!(last, Some((4, 3, 3, 1, 0, 0)));
    }
}
