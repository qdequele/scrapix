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
//!    politeness and the job's rate limits (URLs that may not be fetched yet
//!    go back via `requeue`).
//! 4. Consume the crawler's `FetchFeedback` and release the politeness slot
//!    taken at dispatch (spec R7): a domain slot is held until the fetch
//!    finished, not until the URL reached the bus. robots.txt `Crawl-delay`
//!    and `Retry-After` from the feedback feed the domain's delay/pause.
//! 5. Publish `CrawlEvent::FrontierProgress` snapshots of the store
//!    counters.
//! 6. Consume the API's `JobControl` (spec R5), in a per-instance group so
//!    every instance applies every control: Cancel/Finish mark the job
//!    `Cancelled`/`Finished` and `release` it (queue and seen set dropped,
//!    state kept as a tombstone for `JOB_RETENTION_HOURS`, so late URLs are
//!    refused instead of resurrecting it); Pause/Resume flip
//!    `Running` ↔ `Paused` (a paused job keeps admitting, it only stops
//!    dispatching). A Pause that arrives before the job's first URL is
//!    remembered (bounded, 10 min) and the job starts `Paused`.
//!
//! Politeness state is in Redis when `REDIS_URL` is set (shared by every
//! instance; one consumer group for feedback), otherwise in memory (one
//! consumer group per instance so each instance sees the feedback for its
//! own dispatches).
//!
//! ## Architecture
//!
//! ```text
//! URL_FRONTIER → admit(store) → [lease] pop_ready → [Politeness] → URL_PROCESSING
//!                                                        ↑
//!                                  FETCH_FEEDBACK (crawler) ┘
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
    extract_domain, Acquire, Admission, CrawlRecord, FetchReport, FetchSignal, FrontierStore,
    JobCounters, JobLimits, JobRunState, LinkGraph, LinkGraphConfig, MemoryFrontierStore,
    PolitenessConfig, PolitenessScheduler, PolitenessStore, RecrawlConfig, RecrawlDecision,
    RecrawlScheduler, SlotRequest, UrlHistory, UrlHistoryConfig,
};
use scrapix_queue::{
    topic_names, AnyConsumer, AnyProducer, ConsumerBuilder, CrawlEvent, CrawlHistoryMessage,
    FetchFeedback, JobAction, JobControl, LinksMessage, ProducerBuilder, UrlMessage,
};

/// How many input messages are admitted concurrently.
const ADMIT_CONCURRENCY: usize = 64;
/// Capped exponential backoff for a failing `admit` (retried until it
/// succeeds or the service shuts down).
const ADMIT_BACKOFF_MIN: Duration = Duration::from_millis(100);
const ADMIT_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// Initial per-job pop size (see [`FrontierService::pop_sizes`]).
const INITIAL_POP_SIZE: usize = 64;
/// Renew the dispatch lease at least this often while sending a batch.
const LEASE_RENEW_EVERY: Duration = Duration::from_secs(1);
/// Per-job dispatch lease TTL; renewed on every dispatcher tick.
const LEASE_TTL: Duration = Duration::from_secs(5);
/// How often `FrontierProgress` is published (for jobs whose counters changed).
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// Upper bound on the per-process job caches (parsed templates, initialized jobs).
const JOB_CACHE_CAP: usize = 1024;
/// How many feedback messages are applied concurrently.
const FEEDBACK_CONCURRENCY: usize = 64;
/// A URL whose domain (or job) has every slot in flight is retried after this.
const BUSY_RETRY: Duration = Duration::from_millis(200);
/// Retry delay after a politeness-store or bus error.
const ERROR_RETRY: Duration = Duration::from_secs(1);
/// Upper bound on a `Retry-After` pause (same cap as the crawler's re-queue delay).
const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);
/// How long a Pause for a job this frontier does not know yet is kept,
/// waiting for the job's first URL (R-22).
const EARLY_CONTROL_TTL: Duration = Duration::from_secs(600);
/// Bound on remembered early controls.
const EARLY_CONTROL_CAP: usize = 10_000;
/// Capped backoff for a job control message the store failed to apply.
const CONTROL_BACKOFF_MAX: Duration = Duration::from_secs(5);

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

    /// Default delay between requests to the same domain (ms). Jobs can
    /// only raise it (`rate_limit`), as can robots.txt `Crawl-delay`.
    #[arg(long, env = "DOMAIN_DELAY_MS", default_value = "250")]
    pub domain_delay_ms: u64,

    /// Maximum concurrent requests per domain (in flight until the crawler
    /// reports the fetch back)
    #[arg(long, env = "CONCURRENT_PER_DOMAIN", default_value = "4")]
    pub concurrent_per_domain: usize,

    /// Crawler request timeout (s). A politeness slot whose feedback never
    /// arrives expires after twice this.
    #[arg(long, env = "REQUEST_TIMEOUT", default_value = "30")]
    pub request_timeout_secs: u64,

    /// Multiplier applied to robots.txt `Crawl-delay` (e.g. 1.5 to be extra
    /// polite)
    #[arg(long, env = "ROBOTS_DELAY_MULTIPLIER", default_value = "1.0")]
    pub robots_delay_multiplier: f64,

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

/// Politeness settings from `args`.
pub fn politeness_config(args: &Args) -> PolitenessConfig {
    PolitenessConfig {
        default_delay_ms: args.domain_delay_ms,
        min_delay_ms: 100,
        max_delay_ms: 30_000,
        respect_robots_delay: true,
        robots_delay_multiplier: args.robots_delay_multiplier,
        concurrent_per_domain: args.concurrent_per_domain.max(1),
        slot_ttl: Duration::from_secs(args.request_timeout_secs.max(1) * 2),
    }
}

/// Build the politeness store selected by `args`: shared in Redis when
/// `redis_url` is set (keys under `frontier_key_prefix`), otherwise in
/// memory for this instance.
pub async fn build_politeness(args: &Args) -> anyhow::Result<Arc<dyn PolitenessStore>> {
    let config = politeness_config(args);
    match args.redis_url.as_deref() {
        Some(url) if !url.is_empty() => {
            let p = scrapix_frontier::RedisPoliteness::new(url, &args.frontier_key_prefix, config)
                .await?;
            info!("Politeness state shared in Redis");
            Ok(Arc::new(p))
        }
        _ => Ok(Arc::new(PolitenessScheduler::new(config))),
    }
}

/// Consumer group for `FETCH_FEEDBACK`: one shared group when politeness
/// state is shared (each message applied once), one group per instance
/// otherwise (each instance sees the feedback for its own dispatches).
pub fn feedback_group_id(group_id: &str, instance_id: &str, shared: bool) -> String {
    if shared {
        format!("{group_id}-feedback")
    } else {
        format!("{group_id}-feedback-{instance_id}")
    }
}

/// The per-job politeness limits of a job template.
fn job_limits(template: &UrlMessage) -> JobLimits {
    match template.job {
        Some(ref j) => JobLimits {
            min_delay_ms: j.per_domain_delay_ms,
            max_rps: j.requests_per_second,
            respect_robots: j.respect_robots_txt,
            default_crawl_delay_ms: j.default_crawl_delay_ms,
            max_in_flight: j.max_concurrent_requests,
        },
        None => JobLimits::default(),
    }
}

/// How crawler feedback affects the domain.
fn feedback_signal(fb: &FetchFeedback) -> FetchSignal {
    match fb.status {
        Some(429 | 503) => FetchSignal::RateLimited,
        Some(s) if s >= 500 => FetchSignal::Error,
        Some(_) => FetchSignal::Success,
        None if fb.transport_error => FetchSignal::Error,
        None => FetchSignal::NoRequest,
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
    feedback_received: AtomicU64,
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
            feedback_received: self.feedback_received.load(Ordering::Relaxed),
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
    feedback_received: u64,
    active_jobs: u64,
    active_domains: u64,
    links_recorded: u64,
    history_updates: u64,
}

struct FrontierService {
    consumer: Arc<AnyConsumer>,
    links_consumer: Option<Arc<AnyConsumer>>,
    history_consumer: Option<Arc<AnyConsumer>>,
    /// `FETCH_FEEDBACK` consumer (politeness slot release)
    feedback_consumer: Option<Arc<AnyConsumer>>,
    /// `JOB_STATUS` consumer (cancel / pause / resume / finish)
    control_consumer: Option<Arc<AnyConsumer>>,
    producer: Arc<AnyProducer>,
    store: Arc<dyn FrontierStore>,
    /// Parsed job templates (from `store.job_template`), used on dispatch.
    templates: BoundedCache<Arc<UrlMessage>>,
    /// Jobs this process already ran `ensure_job` (+ first `set_state`) for.
    initialized: BoundedCache<()>,
    /// Jobs whose dispatch lease this instance held on the last tick.
    held_leases: Mutex<HashSet<String>>,
    /// Per-job adaptive pop size (see `dispatch_job`).
    pop_sizes: Mutex<HashMap<String, usize>>,
    /// Popped URLs whose `requeue` failed, retried first on the next tick.
    unrequeued: Mutex<HashMap<String, Vec<CrawlUrl>>>,
    /// Pauses received before the job's first URL (the control overtook
    /// the seed): `init_job` starts such a job `Paused` (R-22).
    early_controls: Mutex<HashMap<String, (JobAction, std::time::Instant)>>,
    politeness: Arc<dyn PolitenessStore>,
    link_graph: Option<Arc<LinkGraph>>,
    recrawl_scheduler: Option<Arc<RecrawlScheduler>>,
    url_history: Option<Arc<UrlHistory>>,
    metrics: Arc<ServiceMetrics>,
    shutdown: Arc<AtomicBool>,
    instance_id: String,
    queue_cap: usize,
    /// How long a released job's state stays as a tombstone.
    job_retention: Duration,
    dispatch_batch_size: usize,
    dispatch_interval: Duration,
    linkgraph_compute_interval: Duration,
}

/// Message bus handles of one service instance.
struct Buses {
    producer: Arc<AnyProducer>,
    main: Arc<AnyConsumer>,
    links: Option<Arc<AnyConsumer>>,
    history: Option<Arc<AnyConsumer>>,
    feedback: Option<Arc<AnyConsumer>>,
    control: Option<Arc<AnyConsumer>>,
}

/// Consumer group for `JOB_STATUS`: one per instance, so every frontier
/// instance applies every control message.
pub fn control_group_id(group_id: &str, instance_id: &str) -> String {
    format!("{group_id}-control-{instance_id}")
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

        let politeness = build_politeness(args).await?;
        let feedback_group =
            feedback_group_id(&args.group_id, &instance_id, politeness.is_shared());
        let feedback_consumer = {
            let c = ConsumerBuilder::new(&args.brokers, &feedback_group)
                .client_id(format!("scrapix-frontier-{}-feedback", instance_id))
                // Old feedback is meaningless (its slots already expired).
                .auto_offset_reset("latest")
                .build()?;
            c.subscribe(&[topic_names::FETCH_FEEDBACK])?;
            info!(
                topic = topic_names::FETCH_FEEDBACK,
                group = %feedback_group,
                "Subscribed to fetch feedback topic"
            );
            Some(Arc::new(AnyConsumer::from(c)))
        };

        let control_group = control_group_id(&args.group_id, &instance_id);
        let control_consumer = {
            let c = ConsumerBuilder::new(&args.brokers, &control_group)
                .client_id(format!("scrapix-frontier-{}-control", instance_id))
                // A new group must not replay the whole control history.
                .auto_offset_reset("latest")
                .build()?;
            c.subscribe(&[topic_names::JOB_STATUS])?;
            info!(
                topic = topic_names::JOB_STATUS,
                group = %control_group,
                "Subscribed to job control topic"
            );
            Some(Arc::new(AnyConsumer::from(c)))
        };

        let store = build_store(args).await?;
        Ok(Self::build(
            args,
            instance_id,
            Buses {
                producer,
                main: consumer,
                links: links_consumer,
                history: history_consumer,
                feedback: feedback_consumer,
                control: control_consumer,
            },
            store,
            politeness,
        ))
    }

    /// Create a `FrontierService` from pre-built message bus trait objects
    /// and frontier store.
    ///
    /// Used by `scrapix all` (shared in-process bus) and by tests (with a
    /// `MemoryFrontierStore`).
    #[allow(clippy::too_many_arguments)]
    pub async fn with_bus(
        args: &Args,
        producer: Arc<AnyProducer>,
        main_consumer: Arc<AnyConsumer>,
        links_consumer: Option<Arc<AnyConsumer>>,
        history_consumer: Option<Arc<AnyConsumer>>,
        feedback_consumer: Option<Arc<AnyConsumer>>,
        control_consumer: Option<Arc<AnyConsumer>>,
        store: Arc<dyn FrontierStore>,
    ) -> anyhow::Result<Self> {
        let instance_id = args
            .instance_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string());

        info!(instance_id = %instance_id, "Initializing frontier service (pre-built bus)");

        let politeness = build_politeness(args).await?;
        if feedback_consumer.is_none() {
            warn!("No fetch feedback consumer: politeness slots only free on expiry");
        }
        if control_consumer.is_none() {
            warn!("No job control consumer: cancel/pause/resume do not reach this frontier");
        }
        Ok(Self::build(
            args,
            instance_id,
            Buses {
                producer,
                main: main_consumer,
                links: links_consumer,
                history: history_consumer,
                feedback: feedback_consumer,
                control: control_consumer,
            },
            store,
            politeness,
        ))
    }

    fn build(
        args: &Args,
        instance_id: String,
        buses: Buses,
        store: Arc<dyn FrontierStore>,
        politeness: Arc<dyn PolitenessStore>,
    ) -> Self {
        let extras = build_extras(args);

        Self {
            consumer: buses.main,
            links_consumer: buses.links,
            history_consumer: buses.history,
            feedback_consumer: buses.feedback,
            control_consumer: buses.control,
            producer: buses.producer,
            store,
            templates: BoundedCache::new(JOB_CACHE_CAP),
            initialized: BoundedCache::new(JOB_CACHE_CAP),
            held_leases: Mutex::new(HashSet::new()),
            pop_sizes: Mutex::new(HashMap::new()),
            unrequeued: Mutex::new(HashMap::new()),
            early_controls: Mutex::new(HashMap::new()),
            politeness,
            link_graph: extras.link_graph,
            recrawl_scheduler: extras.recrawl_scheduler,
            url_history: extras.url_history,
            metrics: Arc::new(ServiceMetrics::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            instance_id,
            queue_cap: args.max_pending_per_job,
            job_retention: Duration::from_secs(args.job_retention_hours.saturating_mul(3600)),
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
        let feedback_handle = self.clone().start_feedback_consumer();
        let control_handle = self.clone().start_control_consumer();

        let result = self.clone().process_messages().await;

        self.shutdown.store(true, Ordering::Relaxed);
        metrics_handle.abort();
        dispatcher_handle.abort();
        progress_handle.abort();
        for h in [
            links_handle,
            history_handle,
            pagerank_handle,
            feedback_handle,
            control_handle,
        ]
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
                    feedback = snapshot.feedback_received,
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

        let mut backoff = ADMIT_BACKOFF_MIN;
        let mut attempt: u64 = 0;
        loop {
            attempt += 1;
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
                        retry_in_ms = backoff.as_millis() as u64,
                        error = %e,
                        "Frontier store admit failed; retrying"
                    );
                    // Re-run job initialization on the next attempt: the
                    // error may be a job the store no longer knows about.
                    self.initialized.remove(&msg.job_id);
                }
            }
            // Keep holding the handler permit while the store is down: that
            // is the backpressure. Only shutdown stops retrying (the message
            // then stays un-acked and is redelivered).
            if self.sleep_unless_shutdown(backoff).await {
                warn!(
                    url = %url.url,
                    job_id = %msg.job_id,
                    attempt,
                    "Shutting down with admit still failing; leaving the message un-acked"
                );
                drop(ack);
                return;
            }
            backoff = (backoff * 2).min(ADMIT_BACKOFF_MAX);
        }
    }

    /// Sleep for `d`, waking early on shutdown. Returns true if shutting down.
    async fn sleep_unless_shutdown(&self, d: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + d;
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            tokio::time::sleep((deadline - now).min(Duration::from_millis(50))).await;
        }
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
            if self.pending_intent(job_id) == Some(JobAction::Pause) {
                self.store.set_state(job_id, JobRunState::Paused).await?;
                info!(job_id = %job_id, "New frontier job starts paused (Pause arrived first)");
                return Ok(());
            }
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

    /// The early control recorded for `job_id`, if not expired.
    fn pending_intent(&self, job_id: &str) -> Option<JobAction> {
        let mut early = self.early_controls.lock();
        match early.get(job_id) {
            Some((action, at)) if at.elapsed() < EARLY_CONTROL_TTL => Some(*action),
            Some(_) => {
                early.remove(job_id);
                None
            }
            None => None,
        }
    }

    fn record_intent(&self, job_id: &str, action: JobAction) {
        let mut early = self.early_controls.lock();
        if early.len() >= EARLY_CONTROL_CAP {
            early.retain(|_, (_, at)| at.elapsed() < EARLY_CONTROL_TTL);
            if early.len() >= EARLY_CONTROL_CAP {
                // Still full of live entries: drop the oldest.
                if let Some(oldest) = early
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(id, _)| id.clone())
                {
                    early.remove(&oldest);
                }
            }
        }
        early.insert(job_id.to_string(), (action, std::time::Instant::now()));
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

        // URLs a previous tick popped but could not put back come first.
        self.flush_unrequeued().await;

        {
            let active: HashSet<&String> = jobs.iter().collect();
            self.pop_sizes.lock().retain(|job, _| active.contains(job));
        }

        let mut held = HashSet::new();
        for job_id in jobs {
            match self.dispatch_job(&job_id).await {
                Ok(true) => {
                    held.insert(job_id);
                }
                Ok(false) => {}
                Err(e) => warn!(job_id = %job_id, error = %e, "Dispatch failed"),
            }
        }
        *self.held_leases.lock() = held;

        let domain_count = self.politeness.tracked_domain_count() as u64;
        self.metrics
            .active_domains
            .store(domain_count, Ordering::Relaxed);
    }

    /// Pop a batch of ready URLs for `job_id` and dispatch those politeness
    /// allows; the rest go back to the store via `requeue` (which undoes the
    /// pop in the counters, so nothing is counted twice). Returns whether
    /// this instance holds the job's dispatch lease.
    ///
    /// The pop size adapts per job (`2 × dispatched last time + 1`, capped
    /// at `dispatch_batch_size`), so a job throttled by politeness pops only
    /// a little more than it can send instead of churning a full batch
    /// through `requeue` on every tick.
    async fn dispatch_job(&self, job_id: &str) -> scrapix_core::Result<bool> {
        if self.unrequeued.lock().contains_key(job_id) {
            // Popped URLs still waiting to go back: don't pop more on top.
            return Ok(false);
        }
        let template = if self.store.state(job_id).await? == Some(JobRunState::Running) {
            self.template(job_id).await
        } else {
            None
        };
        // Renew the lease right before popping; only the holder pops.
        if !self
            .store
            .try_lease(job_id, &self.instance_id, LEASE_TTL)
            .await?
        {
            return Ok(false);
        }
        let Some(template) = template else {
            return Ok(true);
        };

        let pop_size = self.pop_size(job_id);
        let now = now_ms();
        let urls = self.store.pop_ready(job_id, pop_size, now).await?;
        if urls.is_empty() {
            return Ok(true);
        }

        let limits = job_limits(&template);
        let popped = urls.len();
        let mut bounced = Vec::new();
        let mut dispatched = 0usize;
        // URLs bounced because their domain (or the job) was not ready.
        let mut politeness_bounced = 0usize;
        // Domains found busy/waiting in this batch: their remaining URLs are
        // parked without asking the politeness store again.
        let mut not_ready: HashMap<String, Duration> = HashMap::new();
        let mut lease_held = true;
        let mut last_renew = std::time::Instant::now();
        let mut urls = urls.into_iter();
        while let Some(url) = urls.next() {
            if last_renew.elapsed() >= LEASE_RENEW_EVERY {
                match self
                    .store
                    .try_lease(job_id, &self.instance_id, LEASE_TTL)
                    .await
                {
                    Ok(true) => last_renew = std::time::Instant::now(),
                    held => {
                        warn!(job_id = %job_id, result = ?held.map(|_| ()), "Lost dispatch lease mid-batch; requeuing the rest");
                        bounced.push(url);
                        bounced.extend(urls.by_ref());
                        lease_held = false;
                        break;
                    }
                }
            }

            let domain = extract_domain(&url.url);
            if let Some(wait) = not_ready.get(&domain) {
                self.metrics.urls_delayed.fetch_add(1, Ordering::Relaxed);
                politeness_bounced += 1;
                let mut url = url;
                url.not_before_ms = Some(now + wait.as_millis() as i64);
                bounced.push(url);
                continue;
            }
            // The slot token becomes the dispatched message's id, which the
            // crawler echoes back in its FetchFeedback.
            let token = uuid::Uuid::new_v4().to_string();
            let slot = SlotRequest {
                domain: &domain,
                job_id,
                token: &token,
                limits,
            };
            // Park a URL that may not be fetched yet until it is expected to
            // be, so the next ticks don't pop and requeue it over and over.
            let wait = match self.politeness.try_acquire(&slot).await {
                Ok(Acquire::Granted) => None,
                Ok(Acquire::Wait(wait)) => {
                    not_ready.insert(domain.clone(), wait);
                    politeness_bounced += 1;
                    Some(wait)
                }
                Ok(Acquire::DomainBusy) => {
                    not_ready.insert(domain.clone(), BUSY_RETRY);
                    politeness_bounced += 1;
                    Some(BUSY_RETRY)
                }
                Ok(Acquire::JobBusy) => {
                    // Every other URL of the job would be refused too.
                    let until = Some(now + BUSY_RETRY.as_millis() as i64);
                    let mut rest: Vec<CrawlUrl> =
                        std::iter::once(url).chain(urls.by_ref()).collect();
                    for u in &mut rest {
                        u.not_before_ms = until;
                    }
                    self.metrics
                        .urls_delayed
                        .fetch_add(rest.len() as u64, Ordering::Relaxed);
                    politeness_bounced += rest.len();
                    bounced.extend(rest);
                    break;
                }
                Err(e) => {
                    warn!(job_id = %job_id, domain = %domain, error = %e, "Politeness check failed");
                    Some(ERROR_RETRY)
                }
            };
            if let Some(wait) = wait {
                self.metrics.urls_delayed.fetch_add(1, Ordering::Relaxed);
                let mut url = url;
                url.not_before_ms = (!wait.is_zero()).then(|| now + wait.as_millis() as i64);
                bounced.push(url);
                continue;
            }

            let mut msg = template.child(url);
            msg.message_id = token;

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
                    dispatched += 1;
                    self.metrics.urls_dispatched.fetch_add(1, Ordering::Relaxed);
                    debug!(url = %msg.url.url, job_id = %job_id, "Dispatched URL for crawling");
                    // The slot stays taken until the crawler's FetchFeedback
                    // (or its expiry).
                }
                Err(e) => {
                    error!(url = %msg.url.url, job_id = %job_id, error = %e, "Failed to dispatch URL");
                    // A bus outage is not the domain's fault: free the slot
                    // without error accounting (no backoff, no pause).
                    if let Err(e) = self
                        .politeness
                        .release(&domain, job_id, &msg.message_id)
                        .await
                    {
                        warn!(domain = %domain, error = %e, "Failed to release politeness slot; it will expire");
                    }
                    let mut url = msg.url;
                    url.not_before_ms = Some(now + ERROR_RETRY.as_millis() as i64);
                    bounced.push(url);
                }
            }
        }

        // A batch bounced entirely by politeness says nothing about how much
        // the job can send (the bounced URLs are parked, so the next pop
        // reaches other domains): keep the pop size instead of shrinking it.
        if !(dispatched == 0 && politeness_bounced == popped) {
            let next = (2 * dispatched + 1).clamp(1, self.dispatch_batch_size.max(1));
            self.pop_sizes.lock().insert(job_id.to_string(), next);
        }

        self.requeue_or_stash(job_id, bounced).await;
        Ok(lease_held)
    }

    fn pop_size(&self, job_id: &str) -> usize {
        self.pop_sizes
            .lock()
            .get(job_id)
            .copied()
            .unwrap_or_else(|| self.dispatch_batch_size.clamp(1, INITIAL_POP_SIZE))
    }

    /// Put popped URLs back. They were already popped, so they must never
    /// be dropped: if the store refuses them, keep them in memory and retry
    /// on the next tick (before that job pops anything else).
    async fn requeue_or_stash(&self, job_id: &str, urls: Vec<CrawlUrl>) {
        if urls.is_empty() {
            return;
        }
        if let Err(e) = self.store.requeue(job_id, urls.clone()).await {
            warn!(
                job_id = %job_id,
                count = urls.len(),
                error = %e,
                "Failed to requeue popped URLs; keeping them for the next tick"
            );
            self.unrequeued
                .lock()
                .entry(job_id.to_string())
                .or_default()
                .extend(urls);
        }
    }

    async fn flush_unrequeued(&self) {
        let pending = std::mem::take(&mut *self.unrequeued.lock());
        for (job_id, urls) in pending {
            self.requeue_or_stash(&job_id, urls).await;
        }
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

    /// Consume `FETCH_FEEDBACK`, releasing politeness slots. Feedback is
    /// best-effort: a message that cannot be applied is logged and acked
    /// (the slot expires on its own).
    fn start_feedback_consumer(self: Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let consumer = self.feedback_consumer.clone()?;
        Some(tokio::spawn(async move {
            let shutdown = self.shutdown.clone();
            let service = self.clone();
            let result = consumer
                .process_with_ack::<FetchFeedback, _, _>(
                    move |fb, _metadata, ack| {
                        let service = service.clone();
                        async move {
                            service.apply_feedback(&fb).await;
                            ack.ack();
                        }
                    },
                    FEEDBACK_CONCURRENCY,
                    shutdown,
                )
                .await;
            if let Err(e) = result {
                error!(error = %e, "Fetch feedback consumer stopped");
            }
        }))
    }

    async fn apply_feedback(&self, fb: &FetchFeedback) {
        self.metrics
            .feedback_received
            .fetch_add(1, Ordering::Relaxed);
        let domain = if fb.domain.is_empty() {
            extract_domain(&fb.url)
        } else {
            fb.domain.clone()
        };
        let report = FetchReport {
            domain: &domain,
            job_id: &fb.job_id,
            token: &fb.message_id,
            signal: feedback_signal(fb),
            crawl_delay_ms: fb.crawl_delay_ms,
            robots_checked: fb.robots_checked,
            retry_until_ms: fb.retry_after_ms.map(|ms| {
                // Measured from when the crawler saw the response (old
                // feedback without a timestamp: from now).
                let seen = if fb.timestamp > 0 {
                    fb.timestamp
                } else {
                    now_ms()
                };
                seen.saturating_add(ms.min(MAX_RETRY_AFTER.as_millis() as u64) as i64)
            }),
        };
        debug!(domain = %domain, job_id = %fb.job_id, status = ?fb.status, "Fetch feedback");
        if let Err(e) = self.politeness.report(&report).await {
            warn!(domain = %domain, error = %e, "Failed to apply fetch feedback; the slot will expire");
        }
    }

    /// Consume `JOB_STATUS` and apply each control to the store, in order
    /// (concurrency 1: a Pause and its Resume must not be reordered). A
    /// control the store fails to apply is retried with backoff until it
    /// succeeds or the service shuts down.
    fn start_control_consumer(self: Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let consumer = self.control_consumer.clone()?;
        Some(tokio::spawn(async move {
            let shutdown = self.shutdown.clone();
            let service = self.clone();
            let result = consumer
                .process_with_ack::<JobControl, _, _>(
                    move |control, _metadata, ack| {
                        let service = service.clone();
                        async move {
                            let mut backoff = ADMIT_BACKOFF_MIN;
                            loop {
                                match service.apply_control(&control).await {
                                    Ok(()) => {
                                        ack.ack();
                                        return;
                                    }
                                    Err(e) => warn!(
                                        job_id = %control.job_id,
                                        action = ?control.action,
                                        error = %e,
                                        "Failed to apply job control; retrying"
                                    ),
                                }
                                if service.sleep_unless_shutdown(backoff).await {
                                    return; // un-acked: redelivered
                                }
                                backoff = (backoff * 2).min(CONTROL_BACKOFF_MAX);
                            }
                        }
                    },
                    1,
                    shutdown,
                )
                .await;
            if let Err(e) = result {
                error!(error = %e, "Job control consumer stopped");
            }
        }))
    }

    /// Apply one control message to the store.
    ///
    /// - Cancel / Finish: the job becomes `Cancelled` / `Finished` and is
    ///   released. A job the store does not know yet (the control beat its
    ///   first URL) gets a tombstone entry first, so that URL is refused
    ///   too. A job already stopped keeps its state (a Finish never turns a
    ///   cancelled job into a finished one).
    /// - Pause: `Running` → `Paused`; for a job not known yet, remembered
    ///   (bounded, `EARLY_CONTROL_TTL`) so `init_job` starts it `Paused`.
    ///   Resume: `Paused` → `Running` (and forgets such an early Pause).
    ///   Anything else (stopped) is left alone, so a late Resume never
    ///   revives a cancelled or finished job.
    async fn apply_control(&self, control: &JobControl) -> scrapix_core::Result<()> {
        let job_id = control.job_id.as_str();
        if job_id.is_empty() {
            return Ok(());
        }
        let current = self.store.state(job_id).await?;
        match control.action {
            JobAction::Cancel | JobAction::Finish => {
                let target = if control.action == JobAction::Cancel {
                    JobRunState::Cancelled
                } else {
                    JobRunState::Finished
                };
                match current {
                    Some(JobRunState::Cancelled | JobRunState::Finished) => {}
                    Some(_) => self.store.set_state(job_id, target).await?,
                    None => {
                        self.store.ensure_job(job_id, "", None, None).await?;
                        self.store.set_state(job_id, target).await?;
                    }
                }
                self.store.release(job_id, self.job_retention).await?;
                self.early_controls.lock().remove(job_id);
                self.templates.remove(job_id);
                self.pop_sizes.lock().remove(job_id);
                info!(job_id = %job_id, action = ?control.action, "Frontier job stopped and released");
            }
            JobAction::Pause => match current {
                Some(JobRunState::Running) => {
                    self.store.set_state(job_id, JobRunState::Paused).await?;
                    info!(job_id = %job_id, "Frontier job paused");
                }
                None => {
                    info!(job_id = %job_id, "Pause for a job not started yet: it will start paused");
                    self.record_intent(job_id, JobAction::Pause);
                }
                Some(_) => {}
            },
            JobAction::Resume => {
                self.early_controls.lock().remove(job_id);
                if current == Some(JobRunState::Paused) {
                    self.store.set_state(job_id, JobRunState::Running).await?;
                    info!(job_id = %job_id, "Frontier job resumed");
                }
            }
        }
        Ok(())
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
            feedback = metrics.feedback_received,
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
        concurrent_per_domain = args.concurrent_per_domain,
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
#[allow(clippy::too_many_arguments)]
pub async fn run_with_bus(
    args: Args,
    producer: Arc<AnyProducer>,
    main_consumer: Arc<AnyConsumer>,
    links_consumer: Option<Arc<AnyConsumer>>,
    history_consumer: Option<Arc<AnyConsumer>>,
    feedback_consumer: Option<Arc<AnyConsumer>>,
    control_consumer: Option<Arc<AnyConsumer>>,
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
            feedback_consumer,
            control_consumer,
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
    use scrapix_queue::{ChannelBus, CrawlEvent, FetchFeedback};
    use std::time::Instant;

    fn test_args() -> Args {
        let mut args = Args::parse_from(["scrapix-frontier-service"]);
        args.instance_id = Some("test-frontier".to_string());
        args
    }

    fn seed_message(job_id: &str, max_pages: Option<u64>) -> UrlMessage {
        let spec = JobSpec {
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

    async fn spawn_service(
        bus: &ChannelBus,
        args: &Args,
        store: Arc<dyn FrontierStore>,
    ) -> Arc<FrontierService> {
        let producer = Arc::new(AnyProducer::channel(bus.producer()));
        let consumer = Arc::new(AnyConsumer::channel(bus.consumer()));
        consumer.subscribe(&[topic_names::URL_FRONTIER]).unwrap();
        let feedback = Arc::new(AnyConsumer::channel(bus.consumer()));
        feedback.subscribe(&[topic_names::FETCH_FEEDBACK]).unwrap();
        let control = Arc::new(AnyConsumer::channel(
            bus.consumer_in_group(format!("frontier-control-{}", uuid::Uuid::new_v4())),
        ));
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        Arc::new(
            FrontierService::with_bus(
                args,
                producer,
                consumer,
                None,
                None,
                Some(feedback),
                Some(control),
                store,
            )
            .await
            .unwrap(),
        )
    }

    /// Wraps a `MemoryFrontierStore`, counting calls and failing `admit` /
    /// `requeue` a configurable number of times.
    #[derive(Default)]
    struct FlakyStore {
        inner: MemoryFrontierStore,
        admit_failures: AtomicU64,
        requeue_failures: AtomicU64,
        admit_calls: AtomicU64,
        requeued_urls: AtomicU64,
    }

    fn flaky(msg: &str) -> scrapix_core::ScrapixError {
        scrapix_core::ScrapixError::Storage(msg.to_string())
    }

    /// Decrement `n` if positive; true if a failure should be injected.
    fn take_failure(n: &AtomicU64) -> bool {
        n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
            .is_ok()
    }

    #[async_trait::async_trait]
    impl FrontierStore for FlakyStore {
        async fn ensure_job(
            &self,
            job_id: &str,
            template_json: &str,
            max_pages: Option<u64>,
            max_depth: Option<u32>,
        ) -> scrapix_core::Result<()> {
            self.inner
                .ensure_job(job_id, template_json, max_pages, max_depth)
                .await
        }
        async fn job_template(&self, job_id: &str) -> scrapix_core::Result<Option<String>> {
            self.inner.job_template(job_id).await
        }
        async fn admit(
            &self,
            job_id: &str,
            url: &CrawlUrl,
            queue_cap: usize,
        ) -> scrapix_core::Result<Admission> {
            self.admit_calls.fetch_add(1, Ordering::SeqCst);
            if take_failure(&self.admit_failures) {
                return Err(flaky("admit down"));
            }
            self.inner.admit(job_id, url, queue_cap).await
        }
        async fn pop_ready(
            &self,
            job_id: &str,
            n: usize,
            now_ms: i64,
        ) -> scrapix_core::Result<Vec<CrawlUrl>> {
            self.inner.pop_ready(job_id, n, now_ms).await
        }
        async fn requeue(&self, job_id: &str, urls: Vec<CrawlUrl>) -> scrapix_core::Result<()> {
            if take_failure(&self.requeue_failures) {
                return Err(flaky("requeue down"));
            }
            self.requeued_urls
                .fetch_add(urls.len() as u64, Ordering::SeqCst);
            self.inner.requeue(job_id, urls).await
        }
        async fn queued(&self, job_id: &str) -> scrapix_core::Result<u64> {
            self.inner.queued(job_id).await
        }
        async fn counters(&self, job_id: &str) -> scrapix_core::Result<JobCounters> {
            self.inner.counters(job_id).await
        }
        async fn set_state(&self, job_id: &str, state: JobRunState) -> scrapix_core::Result<()> {
            self.inner.set_state(job_id, state).await
        }
        async fn state(&self, job_id: &str) -> scrapix_core::Result<Option<JobRunState>> {
            self.inner.state(job_id).await
        }
        async fn active_jobs(&self) -> scrapix_core::Result<Vec<String>> {
            self.inner.active_jobs().await
        }
        async fn release(&self, job_id: &str, retention: Duration) -> scrapix_core::Result<()> {
            self.inner.release(job_id, retention).await
        }
        async fn try_lease(
            &self,
            job_id: &str,
            owner: &str,
            ttl: Duration,
        ) -> scrapix_core::Result<bool> {
            self.inner.try_lease(job_id, owner, ttl).await
        }
    }

    /// Create a running job with `n` queued URLs on one domain.
    async fn seed_store(store: &dyn FrontierStore, job_id: &str, n: usize) -> UrlMessage {
        let seed = seed_message(job_id, None);
        store
            .ensure_job(job_id, &serde_json::to_string(&seed).unwrap(), None, None)
            .await
            .unwrap();
        store.set_state(job_id, JobRunState::Running).await.unwrap();
        for i in 0..n {
            let url = CrawlUrl::new(format!("https://one.test/{i}"), 1);
            assert_eq!(
                store.admit(job_id, &url, 1_000_000).await.unwrap(),
                Admission::Admitted
            );
        }
        seed
    }

    struct Harness {
        bus: ChannelBus,
        service: Arc<FrontierService>,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Harness {
        async fn start() -> Self {
            let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
            Self::start_with(test_args(), store).await
        }

        async fn start_with(args: Args, store: Arc<dyn FrontierStore>) -> Self {
            let bus = ChannelBus::new();
            let service = spawn_service(&bus, &args, store).await;
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

        /// Publish the crawler's `FetchFeedback` for a dispatched message.
        async fn feedback(&self, m: &UrlMessage, status: Option<u16>, retry_after_ms: Option<u64>) {
            let fb = FetchFeedback {
                status,
                retry_after_ms,
                ..FetchFeedback::new(
                    extract_domain(&m.url.url),
                    m.job_id.clone(),
                    m.message_id.clone(),
                    m.url.url.clone(),
                )
            };
            AnyProducer::channel(self.bus.producer())
                .send(topic_names::FETCH_FEEDBACK, Some(&fb.domain), &fb)
                .await
                .unwrap();
        }

        /// Publish a `JobControl` as the API does.
        async fn control(&self, job_id: &str, action: JobAction) {
            AnyProducer::channel(self.bus.producer())
                .send(
                    topic_names::JOB_STATUS,
                    Some(job_id),
                    &JobControl::new(job_id, action),
                )
                .await
                .unwrap();
        }

        /// Wait (bounded) until the store reports `state` for `job_id`.
        async fn wait_state(&self, job_id: &str, state: JobRunState) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if self.service.store.state(job_id).await.unwrap() == Some(state) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!(
                "{job_id} never reached {state:?}: {:?}",
                self.service.store.state(job_id).await
            );
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

    #[tokio::test]
    async fn admit_errors_are_retried_and_acked_once_after_success() {
        let store = Arc::new(FlakyStore::default());
        store.admit_failures.store(3, Ordering::SeqCst);
        let bus = ChannelBus::new();
        let service = spawn_service(&bus, &test_args(), store.clone()).await;

        let acks = Arc::new(AtomicU64::new(0));
        let calls_at_ack = Arc::new(AtomicU64::new(0));
        let ack = {
            let (acks, calls_at_ack, store) = (acks.clone(), calls_at_ack.clone(), store.clone());
            Ack::from_fn(move || {
                acks.fetch_add(1, Ordering::SeqCst);
                calls_at_ack.store(store.admit_calls.load(Ordering::SeqCst), Ordering::SeqCst);
            })
        };
        service
            .handle_input(seed_message("job-flaky", None), ack)
            .await;

        assert_eq!(acks.load(Ordering::SeqCst), 1, "acked exactly once");
        assert_eq!(
            calls_at_ack.load(Ordering::SeqCst),
            4,
            "acked only after the 4th (first successful) admit"
        );
        assert_eq!(store.counters("job-flaky").await.unwrap().admitted, 1);
    }

    #[tokio::test]
    async fn admit_failing_until_shutdown_is_never_acked() {
        let store = Arc::new(FlakyStore::default());
        store.admit_failures.store(u64::MAX, Ordering::SeqCst);
        let bus = ChannelBus::new();
        let service = spawn_service(&bus, &test_args(), store.clone()).await;

        let acks = Arc::new(AtomicU64::new(0));
        let ack = {
            let acks = acks.clone();
            Ack::from_fn(move || {
                acks.fetch_add(1, Ordering::SeqCst);
            })
        };
        let task = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .handle_input(seed_message("job-down", None), ack)
                    .await
            }
        });
        // Well past the old 3-attempt give-up point.
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(
            !task.is_finished(),
            "keeps retrying while the store is down"
        );
        assert!(store.admit_calls.load(Ordering::SeqCst) >= 4);
        service.shutdown.store(true, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("stops on shutdown")
            .unwrap();
        assert_eq!(acks.load(Ordering::SeqCst), 0, "never acked");
    }

    #[tokio::test]
    async fn politeness_throttled_job_does_not_churn_requeues() {
        let store = Arc::new(FlakyStore::default());
        seed_store(&*store, "job-churn", 500).await;
        let mut args = test_args();
        args.domain_delay_ms = 50;
        // No crawler sends feedback here: don't let held slots cap the test.
        args.concurrent_per_domain = 10_000;
        let h = Harness::start_with(args, store.clone()).await;
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        let dispatched = h.service.metrics.urls_dispatched.load(Ordering::Relaxed);
        h.stop();

        let requeued = store.requeued_urls.load(Ordering::SeqCst);
        assert!(
            dispatched >= 5,
            "a 50ms domain should get ~20 dispatches/s, got {dispatched}"
        );
        assert!(
            requeued <= 10 * dispatched + 64,
            "requeued {requeued} URLs for {dispatched} dispatched"
        );
        assert_eq!(
            store.counters("job-churn").await.unwrap().dispatched,
            dispatched,
            "store `dispatched` equals URLs actually sent"
        );
    }

    #[tokio::test]
    async fn bounced_urls_are_eventually_dispatched_once() {
        let store = Arc::new(FlakyStore::default());
        seed_store(&*store, "job-bounce", 5).await;
        let mut args = test_args();
        args.domain_delay_ms = 50;
        // No crawler sends feedback here: don't let held slots cap the test.
        args.concurrent_per_domain = 10_000;
        let h = Harness::start_with(args, store.clone()).await;
        let dispatched = h.collect_dispatched(Duration::from_millis(1_500)).await;
        h.stop();

        let urls: std::collections::HashSet<_> =
            dispatched.iter().map(|m| m.url.url.clone()).collect();
        assert_eq!(dispatched.len(), 5, "each URL dispatched exactly once");
        assert_eq!(urls.len(), 5);
        assert!(
            store.requeued_urls.load(Ordering::SeqCst) > 0,
            "politeness must have bounced some URLs"
        );
        let c = store.counters("job-bounce").await.unwrap();
        assert_eq!((c.admitted, c.dispatched), (5, 5));
    }

    #[tokio::test]
    async fn failed_requeue_keeps_popped_urls_and_retries() {
        let store = Arc::new(FlakyStore::default());
        store.requeue_failures.store(2, Ordering::SeqCst);
        seed_store(&*store, "job-requeue", 5).await;
        let mut args = test_args();
        args.domain_delay_ms = 50;
        // No crawler sends feedback here: don't let held slots cap the test.
        args.concurrent_per_domain = 10_000;
        let h = Harness::start_with(args, store.clone()).await;
        let dispatched = h.collect_dispatched(Duration::from_millis(1_500)).await;
        h.stop();

        assert_eq!(
            store.requeue_failures.load(Ordering::SeqCst),
            0,
            "failures injected"
        );
        let urls: std::collections::HashSet<_> =
            dispatched.iter().map(|m| m.url.url.clone()).collect();
        assert_eq!(dispatched.len(), 5, "no popped URL is lost");
        assert_eq!(urls.len(), 5);
        assert_eq!(store.queued("job-requeue").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn only_the_lease_holder_dispatches_a_job() {
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let bus = ChannelBus::new();
        let mut args_a = test_args();
        args_a.instance_id = Some("frontier-a".to_string());
        let mut args_b = test_args();
        args_b.instance_id = Some("frontier-b".to_string());
        let a = spawn_service(&bus, &args_a, store.clone()).await;
        let b = spawn_service(&bus, &args_b, store.clone()).await;
        let ha = tokio::spawn(a.clone().run());
        let hb = tokio::spawn(b.clone().run());

        let processing = AnyConsumer::channel(bus.consumer());
        processing
            .subscribe(&[topic_names::URL_PROCESSING])
            .unwrap();
        let producer = AnyProducer::channel(bus.producer());
        let seed = seed_message("job-lease", None);
        producer
            .send(topic_names::URL_FRONTIER, None, &seed)
            .await
            .unwrap();
        for i in 0..9 {
            // Different domains so politeness doesn't slow the test down.
            let child = seed.child(CrawlUrl::new(format!("https://d{i}.test/"), 1));
            producer
                .send(topic_names::URL_FRONTIER, None, &child)
                .await
                .unwrap();
        }

        let deadline = Instant::now() + Duration::from_millis(1_500);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            if let Some(m) = processing
                .poll_one::<UrlMessage>(Duration::from_millis(50))
                .await
                .unwrap()
            {
                got.push(m.url.url);
            }
        }
        for s in [&a, &b] {
            s.shutdown.store(true, Ordering::Relaxed);
        }
        ha.abort();
        hb.abort();

        let unique: std::collections::HashSet<_> = got.iter().cloned().collect();
        assert_eq!(got.len(), 10, "each URL dispatched once: {got:?}");
        assert_eq!(unique.len(), 10);
        let (da, db) = (
            a.metrics.urls_dispatched.load(Ordering::Relaxed),
            b.metrics.urls_dispatched.load(Ordering::Relaxed),
        );
        assert!(
            (da, db) == (10, 0) || (da, db) == (0, 10),
            "one instance dispatched everything: a={da} b={db}"
        );
    }

    fn one_domain_job(job_id: &str, spec: Option<JobSpec>) -> UrlMessage {
        UrlMessage::new(CrawlUrl::seed("https://one.test/"), job_id, "idx").with_job(spec)
    }

    #[tokio::test]
    async fn domain_slot_is_held_until_fetch_feedback() {
        let mut args = test_args();
        args.concurrent_per_domain = 1;
        args.domain_delay_ms = 0;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(args, store).await;
        let seed = one_domain_job("job-slot", None);
        h.publish(&seed).await;
        for i in 0..2 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://one.test/{i}"), 1)))
                .await;
        }

        let first = h.collect_dispatched(Duration::from_millis(800)).await;
        assert_eq!(first.len(), 1, "one slot: only one URL in flight");
        h.feedback(&first[0], Some(200), None).await;
        let second = h.collect_dispatched(Duration::from_millis(800)).await;
        assert_eq!(second.len(), 1, "feedback frees exactly one slot");
        assert_ne!(second[0].url.url, first[0].url.url);
        // Feedback for an unknown message frees nothing.
        let mut stranger = second[0].clone();
        stranger.message_id = "not-dispatched".to_string();
        h.feedback(&stranger, Some(200), None).await;
        assert!(h
            .collect_dispatched(Duration::from_millis(500))
            .await
            .is_empty());
        h.feedback(&second[0], None, None).await; // fail-closed: still frees
        let third = h.collect_dispatched(Duration::from_millis(800)).await;
        h.stop();
        assert_eq!(third.len(), 1);
    }

    #[tokio::test]
    async fn per_job_in_flight_cap_is_enforced_across_domains() {
        let mut args = test_args();
        args.domain_delay_ms = 0;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(args, store).await;
        let spec = JobSpec {
            max_concurrent_requests: Some(2),
            ..JobSpec::default()
        };
        let seed = UrlMessage::new(CrawlUrl::seed("https://d0.test/"), "job-cap", "idx")
            .with_job(Some(spec));
        h.publish(&seed).await;
        for i in 1..5 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://d{i}.test/"), 1)))
                .await;
        }

        let first = h.collect_dispatched(Duration::from_millis(800)).await;
        assert_eq!(first.len(), 2, "max_concurrent_requests=2");
        h.feedback(&first[0], Some(200), None).await;
        let second = h.collect_dispatched(Duration::from_millis(800)).await;
        h.stop();
        assert_eq!(second.len(), 1, "one feedback, one more dispatch");
    }

    #[tokio::test]
    async fn retry_after_feedback_pauses_the_domain() {
        let mut args = test_args();
        args.concurrent_per_domain = 1;
        args.domain_delay_ms = 0;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(args, store).await;
        let seed = one_domain_job("job-429", None);
        h.publish(&seed).await;
        h.publish(&seed.child(CrawlUrl::new("https://one.test/a", 1)))
            .await;

        let first = h.collect_dispatched(Duration::from_millis(800)).await;
        assert_eq!(first.len(), 1);
        h.feedback(&first[0], Some(429), Some(60_000)).await;
        let during_pause = h.collect_dispatched(Duration::from_millis(1_000)).await;
        h.stop();
        assert!(
            during_pause.is_empty(),
            "slot is free but the domain is paused by Retry-After: {:?}",
            during_pause.iter().map(|m| &m.url.url).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn job_rate_limit_spaces_requests_to_a_domain() {
        let mut args = test_args();
        args.domain_delay_ms = 0;
        args.concurrent_per_domain = 10_000;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(args, store).await;
        let spec = JobSpec {
            requests_per_second: Some(4.0), // 250 ms apart
            ..JobSpec::default()
        };
        let seed = one_domain_job("job-rps", Some(spec));
        h.publish(&seed).await;
        for i in 0..9 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://one.test/{i}"), 1)))
                .await;
        }
        // Timestamp each dispatch as it is received.
        let c = h.consumer(topic_names::URL_PROCESSING);
        let deadline = Instant::now() + Duration::from_millis(1_500);
        let mut at = Vec::new();
        while Instant::now() < deadline {
            if c.poll_one::<UrlMessage>(Duration::from_millis(5))
                .await
                .unwrap()
                .is_some()
            {
                at.push(Instant::now());
            }
        }
        h.stop();
        assert!(
            at.len() >= 3,
            "4 rps over 1.5 s: at least 3 dispatches, got {}",
            at.len()
        );
        for w in at.windows(2) {
            let gap = w[1] - w[0];
            // 250 ms minus receive jitter (dispatch tick + poll interval).
            assert!(
                gap >= Duration::from_millis(200),
                "dispatches {gap:?} apart"
            );
        }
    }

    /// With `REDIS_URL` set, two frontier instances share one domain slot:
    /// only one URL of the domain is in flight across both until feedback.
    /// Needs `SCRAPIX_TEST_REDIS_URL`; skipped otherwise. Keys live under a
    /// random prefix and expire on their own (slot TTL / one day).
    #[tokio::test]
    async fn redis_politeness_is_shared_between_instances() {
        let Ok(url) = std::env::var("SCRAPIX_TEST_REDIS_URL") else {
            eprintln!("SCRAPIX_TEST_REDIS_URL not set; skipping shared politeness test");
            return;
        };
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let bus = ChannelBus::new();
        let prefix = format!("test-svc-{}", uuid::Uuid::new_v4());
        let mut services = Vec::new();
        for name in ["frontier-a", "frontier-b"] {
            let mut args = test_args();
            args.instance_id = Some(name.to_string());
            args.redis_url = Some(url.clone());
            args.frontier_key_prefix = prefix.clone();
            args.concurrent_per_domain = 1;
            args.domain_delay_ms = 0;
            let s = spawn_service(&bus, &args, store.clone()).await;
            services.push((s.clone(), tokio::spawn(s.run())));
        }
        let processing = AnyConsumer::channel(bus.consumer());
        processing
            .subscribe(&[topic_names::URL_PROCESSING])
            .unwrap();
        let producer = AnyProducer::channel(bus.producer());
        // Two jobs on the same domain: whichever instance leases each, they
        // compete for the same shared slot.
        for job in ["job-shared-1", "job-shared-2"] {
            let seed = one_domain_job(job, None);
            producer
                .send(topic_names::URL_FRONTIER, None, &seed)
                .await
                .unwrap();
            producer
                .send(
                    topic_names::URL_FRONTIER,
                    None,
                    &seed.child(CrawlUrl::new(format!("https://one.test/{job}"), 1)),
                )
                .await
                .unwrap();
        }
        let collect = |window: Duration| {
            let processing = &processing;
            async move {
                let deadline = Instant::now() + window;
                let mut out = Vec::new();
                while Instant::now() < deadline {
                    if let Some(m) = processing
                        .poll_one::<UrlMessage>(Duration::from_millis(50))
                        .await
                        .unwrap()
                    {
                        out.push(m);
                    }
                }
                out
            }
        };
        let first = collect(Duration::from_millis(1_000)).await;
        assert_eq!(first.len(), 1, "one shared slot across both instances");
        let fb = FetchFeedback {
            status: Some(200),
            ..FetchFeedback::new(
                "one.test",
                first[0].job_id.clone(),
                first[0].message_id.clone(),
                first[0].url.url.clone(),
            )
        };
        producer
            .send(topic_names::FETCH_FEEDBACK, Some("one.test"), &fb)
            .await
            .unwrap();
        let second = collect(Duration::from_millis(1_000)).await;
        for (s, h) in services {
            s.shutdown.store(true, Ordering::Relaxed);
            h.abort();
        }
        assert_eq!(second.len(), 1, "feedback frees the shared slot once");
    }

    /// A job spec built the way the API builds it (default `CrawlConfig`,
    /// with `rate_limit` overrides from `rate_limit_json`).
    fn api_job_spec(rate_limit_json: serde_json::Value) -> JobSpec {
        let config: scrapix_core::CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://example.com/"],
            "index_uid": "idx",
            "rate_limit": rate_limit_json,
        }))
        .unwrap();
        JobSpec::from_config(&config)
    }

    /// The delay the service's politeness store imposes on `domain` for a
    /// job with `spec`, after the crawler reported the domain's robots.txt
    /// (`robots_checked`, no Crawl-delay): take one slot, then read the
    /// wait before the next.
    async fn delay_after_robots_feedback(
        service: &FrontierService,
        domain: &str,
        spec: JobSpec,
        robots_checked: bool,
    ) -> Duration {
        let template = UrlMessage::new(
            CrawlUrl::seed(format!("https://{domain}/")),
            "job-delay",
            "idx",
        )
        .with_job(Some(spec));
        let limits = job_limits(&template);
        let slot = |token: &'static str| SlotRequest {
            domain,
            job_id: "job-delay",
            token,
            limits,
        };
        assert_eq!(
            service.politeness.try_acquire(&slot("t1")).await.unwrap(),
            Acquire::Granted
        );
        service
            .apply_feedback(&FetchFeedback {
                status: Some(200),
                robots_checked,
                ..FetchFeedback::new(domain, "job-delay", "t1", format!("https://{domain}/"))
            })
            .await;
        match service.politeness.try_acquire(&slot("t2")).await.unwrap() {
            Acquire::Wait(d) => d,
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    fn assert_near(d: Duration, expected_ms: u64) {
        let ms = d.as_millis() as u64;
        assert!(
            ms <= expected_ms && ms + 100 >= expected_ms,
            "expected ~{expected_ms} ms, got {ms} ms"
        );
    }

    #[tokio::test]
    async fn default_crawl_delay_applies_only_when_robots_checked_and_no_explicit_delay() {
        let bus = ChannelBus::new();
        let args = test_args(); // DOMAIN_DELAY_MS default: 250
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let service = spawn_service(&bus, &args, store).await;

        // A default API job: only the worker delay.
        let d = delay_after_robots_feedback(
            &service,
            "a.test",
            api_job_spec(serde_json::json!({})),
            true,
        )
        .await;
        assert_near(d, 250);

        // Explicit per-domain delay wins over default_crawl_delay_ms.
        let spec = api_job_spec(serde_json::json!({
            "per_domain_delay_ms": 400, "default_crawl_delay_ms": 1500
        }));
        let d = delay_after_robots_feedback(&service, "b.test", spec, true).await;
        assert_near(d, 400);

        // No explicit delay, robots checked without Crawl-delay: 1500.
        let spec = api_job_spec(serde_json::json!({
            "per_domain_delay_ms": 0, "default_crawl_delay_ms": 1500
        }));
        let d = delay_after_robots_feedback(&service, "c.test", spec.clone(), true).await;
        assert_near(d, 1500);

        // Same job, robots.txt never consulted: worker delay only.
        let d = delay_after_robots_feedback(&service, "d.test", spec, false).await;
        assert_near(d, 250);

        // An explicit rate (rpm) also disables default_crawl_delay_ms.
        let spec = api_job_spec(serde_json::json!({
            "per_domain_delay_ms": 0, "default_crawl_delay_ms": 1500, "requests_per_minute": 120
        }));
        let d = delay_after_robots_feedback(&service, "e.test", spec, true).await;
        assert_near(d, 500);
    }

    #[tokio::test]
    async fn stale_retry_after_is_measured_from_the_feedback_timestamp() {
        let bus = ChannelBus::new();
        let mut args = test_args();
        args.domain_delay_ms = 0;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let service = spawn_service(&bus, &args, store).await;
        let l = JobLimits::default();
        let slot = |domain: &'static str, token: &'static str| SlotRequest {
            domain,
            job_id: "j",
            token,
            limits: l,
        };
        let now = now_ms();

        // Produced 90 s ago with Retry-After 60 s: already over.
        assert_eq!(
            service
                .politeness
                .try_acquire(&slot("old.test", "o1"))
                .await
                .unwrap(),
            Acquire::Granted
        );
        service
            .apply_feedback(&FetchFeedback {
                status: Some(429),
                retry_after_ms: Some(60_000),
                timestamp: now - 90_000,
                ..FetchFeedback::new("old.test", "j", "o1", "https://old.test/")
            })
            .await;
        assert_eq!(
            service
                .politeness
                .try_acquire(&slot("old.test", "o2"))
                .await
                .unwrap(),
            Acquire::Granted
        );

        // Produced 50 s ago with Retry-After 60 s: ~10 s left.
        assert_eq!(
            service
                .politeness
                .try_acquire(&slot("new.test", "n1"))
                .await
                .unwrap(),
            Acquire::Granted
        );
        service
            .apply_feedback(&FetchFeedback {
                status: Some(429),
                retry_after_ms: Some(60_000),
                timestamp: now - 50_000,
                ..FetchFeedback::new("new.test", "j", "n1", "https://new.test/")
            })
            .await;
        match service
            .politeness
            .try_acquire(&slot("new.test", "n2"))
            .await
            .unwrap()
        {
            Acquire::Wait(d) => assert!(
                d <= Duration::from_secs(10) && d >= Duration::from_secs(9),
                "{d:?}"
            ),
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn saturated_domain_does_not_starve_other_domains() {
        let mut args = test_args();
        args.concurrent_per_domain = 1;
        args.domain_delay_ms = 0;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(args, store).await;
        // Another job holds busy.test's only slot and never reports back.
        assert_eq!(
            h.service
                .politeness
                .try_acquire(&SlotRequest {
                    domain: "busy.test",
                    job_id: "hog",
                    token: "hog-1",
                    limits: JobLimits::default(),
                })
                .await
                .unwrap(),
            Acquire::Granted
        );
        let seed = UrlMessage::new(CrawlUrl::seed("https://busy.test/"), "job-mixed", "idx");
        h.publish(&seed).await;
        // Busy-domain URLs first (FIFO), then 20 free domains.
        for i in 0..300 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://busy.test/{i}"), 1)))
                .await;
        }
        for i in 0..20 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://free{i}.test/"), 1)))
                .await;
        }
        let got = h.collect_dispatched(Duration::from_millis(1_000)).await;
        h.stop();
        let free = got.iter().filter(|m| m.url.url.contains("free")).count();
        assert_eq!(
            free, 20,
            "every free domain dispatched despite the busy one"
        );
        assert!(got.iter().all(|m| !m.url.url.contains("busy.test")));
    }

    /// R5: Cancel stops the job for good: queued work is dropped, later
    /// URLs are refused (and acked), and a late URL never resurrects it.
    #[tokio::test]
    async fn cancel_drops_queued_work_and_late_urls_never_resurrect_the_job() {
        let mut args = test_args();
        args.concurrent_per_domain = 1;
        args.domain_delay_ms = 0;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(args, store.clone()).await;
        let seed = one_domain_job("job-cancel", None);
        h.publish(&seed).await;
        for i in 0..3 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://one.test/{i}"), 1)))
                .await;
        }
        // One slot and no feedback: one URL in flight, the rest queued.
        let first = h.collect_dispatched(Duration::from_millis(600)).await;
        assert_eq!(first.len(), 1);
        assert_eq!(store.queued("job-cancel").await.unwrap(), 3);

        h.control("job-cancel", JobAction::Cancel).await;
        h.wait_state("job-cancel", JobRunState::Cancelled).await;
        assert_eq!(store.queued("job-cancel").await.unwrap(), 0);
        assert!(!store
            .active_jobs()
            .await
            .unwrap()
            .contains(&"job-cancel".to_string()));

        // The slot frees, more URLs arrive: nothing is dispatched.
        h.feedback(&first[0], Some(200), None).await;
        for i in 10..13 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://one.test/{i}"), 1)))
                .await;
        }
        assert!(h
            .collect_dispatched(Duration::from_millis(600))
            .await
            .is_empty());
        assert_eq!(store.queued("job-cancel").await.unwrap(), 0);
        assert_eq!(
            store.state("job-cancel").await.unwrap(),
            Some(JobRunState::Cancelled),
            "a late URL must not resurrect a cancelled job"
        );
        let c = store.counters("job-cancel").await.unwrap();
        assert_eq!(c.received, 7, "late URLs were consumed (acked), not stuck");

        // A Cancel that beats the job's first URL leaves a tombstone.
        h.control("job-ghost", JobAction::Cancel).await;
        h.wait_state("job-ghost", JobRunState::Cancelled).await;
        h.publish(&one_domain_job("job-ghost", None)).await;
        assert!(h
            .collect_dispatched(Duration::from_millis(500))
            .await
            .is_empty());
        assert_eq!(
            store.state("job-ghost").await.unwrap(),
            Some(JobRunState::Cancelled)
        );
        h.stop();
    }

    /// Finish releases a completed job the same way, and it stays dead.
    #[tokio::test]
    async fn finish_releases_the_job_and_keeps_it_finished() {
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(test_args(), store.clone()).await;
        let seed = seed_message("job-fin", None);
        h.publish(&seed).await;
        assert_eq!(
            h.collect_dispatched(Duration::from_millis(500)).await.len(),
            1
        );
        h.control("job-fin", JobAction::Finish).await;
        h.wait_state("job-fin", JobRunState::Finished).await;
        // A Resume after Finish must not revive it.
        h.control("job-fin", JobAction::Resume).await;
        h.publish(&seed.child(CrawlUrl::new("https://example.com/late", 1)))
            .await;
        assert!(h
            .collect_dispatched(Duration::from_millis(500))
            .await
            .is_empty());
        assert_eq!(
            store.state("job-fin").await.unwrap(),
            Some(JobRunState::Finished)
        );
        h.stop();
    }

    /// Pause stops dispatch but keeps the queue; Resume restarts it.
    #[tokio::test]
    async fn pause_stops_dispatch_and_resume_restarts_it() {
        let mut args = test_args();
        args.domain_delay_ms = 0;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(args, store.clone()).await;
        let seed = seed_message("job-pause", None);
        h.publish(&seed).await;
        assert_eq!(
            h.collect_dispatched(Duration::from_millis(500)).await.len(),
            1
        );

        h.control("job-pause", JobAction::Pause).await;
        h.wait_state("job-pause", JobRunState::Paused).await;
        // Links found by pages still in flight keep arriving while paused:
        // they are queued, not dispatched, and not lost.
        for i in 0..3 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://d{i}.test/"), 1)))
                .await;
        }
        assert!(h
            .collect_dispatched(Duration::from_millis(600))
            .await
            .is_empty());
        assert_eq!(store.queued("job-pause").await.unwrap(), 3);

        h.control("job-pause", JobAction::Resume).await;
        h.wait_state("job-pause", JobRunState::Running).await;
        let resumed = h.collect_dispatched(Duration::from_millis(800)).await;
        h.stop();
        assert_eq!(resumed.len(), 3, "dispatch restarts after Resume");
    }

    /// R-22: a Pause that overtakes the job's seed (different topic) is
    /// remembered and applied when the job starts: nothing dispatches until
    /// Resume.
    #[tokio::test]
    async fn pause_before_the_seed_is_applied_when_the_job_starts() {
        let mut args = test_args();
        args.domain_delay_ms = 0;
        let store: Arc<dyn FrontierStore> = Arc::new(MemoryFrontierStore::default());
        let h = Harness::start_with(args, store.clone()).await;
        h.control("job-early", JobAction::Pause).await;
        let deadline = Instant::now() + Duration::from_secs(2);
        while h.service.pending_intent("job-early").is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            h.service.pending_intent("job-early"),
            Some(JobAction::Pause)
        );

        let seed = seed_message("job-early", None);
        h.publish(&seed).await;
        for i in 0..2 {
            h.publish(&seed.child(CrawlUrl::new(format!("https://d{i}.test/"), 1)))
                .await;
        }
        assert!(h
            .collect_dispatched(Duration::from_millis(600))
            .await
            .is_empty());
        assert_eq!(
            store.state("job-early").await.unwrap(),
            Some(JobRunState::Paused)
        );
        assert_eq!(store.queued("job-early").await.unwrap(), 3);

        h.control("job-early", JobAction::Resume).await;
        h.wait_state("job-early", JobRunState::Running).await;
        let resumed = h.collect_dispatched(Duration::from_millis(800)).await;
        h.stop();
        assert_eq!(resumed.len(), 3);
    }
}
