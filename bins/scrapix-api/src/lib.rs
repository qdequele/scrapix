//! Scrapix API Server
//!
//! REST API and WebSocket server for managing crawl jobs.
//!
//! ## REST Endpoints
//!
//! - `POST /scrape` - Scrape a single URL (instant, no queue)
//! - `POST /map` - Discover all URLs on a website with titles and descriptions
//! - `POST /crawl` - Create a new async crawl job
//! - `POST /crawl/sync` - Create a sync crawl job (waits for completion)
//! - `GET /job/:id/status` - Get job status
//! - `GET /job/:id/events` - SSE stream for job events
//! - `DELETE /job/:id` - Cancel a job
//! - `GET /jobs` - List all jobs
//! - `GET /health` - Health check
//!
//! ## WebSocket Endpoints
//!
//! - `GET /ws` - WebSocket for subscribing to multiple job events
//! - `GET /ws/job/:id` - WebSocket for a specific job (auto-subscribes)
//!
//! ### WebSocket Protocol
//!
//! Client messages (JSON):
//! - `{"type": "subscribe", "job_id": "..."}` - Subscribe to job events
//! - `{"type": "unsubscribe", "job_id": "..."}` - Unsubscribe from job
//! - `{"type": "get_status", "job_id": "..."}` - Request current status
//! - `{"type": "ping"}` - Keepalive ping
//!
//! Server messages (JSON):
//! - `{"type": "event", "job_id": "...", "event": {...}}` - Job event
//! - `{"type": "status", "job_id": "...", "status": {...}}` - Job status
//! - `{"type": "subscribed", "job_id": "..."}` - Subscription confirmed
//! - `{"type": "unsubscribed", "job_id": "..."}` - Unsubscription confirmed
//! - `{"type": "error", "message": "...", "code": "..."}` - Error
//! - `{"type": "pong", "timestamp": 123456789}` - Pong response

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub mod analytics;
pub(crate) mod analytics_pipes;
pub mod auth;
pub(crate) mod batch;
pub mod billing;
pub mod completion;
pub(crate) mod diagnostics;
pub mod documents;
pub(crate) mod engine_jobs;
pub(crate) mod extract;
pub(crate) mod job_kind;
pub mod job_store;
pub(crate) mod lab_client;
pub(crate) mod lab_events;
pub(crate) mod lab_sink;
pub(crate) mod legacy_credits;
pub mod meili;
pub mod openapi;
pub(crate) mod results;
pub(crate) mod router;
pub mod settings;
pub mod webhooks;

#[cfg(test)]
mod scrape_browser_tests;
#[cfg(test)]
mod scrape_tests;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension, Path, Query, State,
    },
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use clap::Parser;
use futures::{stream::Stream, SinkExt, StreamExt as FuturesStreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{debug, error, info, warn};

use scrapix_ai::{AiClient, AiService, FieldDefinition as AiFieldDefinition, SchemaBuilder};
use scrapix_core::browser::{Action, RequestCookie};
use scrapix_core::{
    ConcurrencyConfig, CrawlConfig, CrawlUrl, CrawlerType, FeaturesConfig, JobSpec, JobState,
    JobStatus,
};
use scrapix_crawler::{
    is_non_page_url, CdpRenderer, CdpRendererBuilder, HttpFetcher, HttpFetcherBuilder, PageOptions,
    RobotsCache, RobotsConfig, ScreenshotOptions, SitemapParser, WaitUntil,
};
use scrapix_extractor::{
    ContentBlock, ExtractedMetadata, ExtractedSchema, Extractor, SelectorDefinition,
    SelectorExtractor,
};
use scrapix_parser::{
    detect_language_info, extract_content, html_to_main_content_markdown,
    html_to_main_content_minihtml, html_to_markdown, html_to_minihtml,
};
use scrapix_queue::{
    topic_names, Ack, AnyConsumer, AnyProducer, ConsumerBuilder, CrawlEvent, EventPosition,
    JobAccounting, JobAction, JobControl, ProducerBuilder, UrlMessage,
};

use completion::{finalize_decision, Finalize};
use scrapix_storage::clickhouse::{
    AiUsageBatcher, AiUsageEvent as ClickHouseAiUsageEvent, ClickHouseStorage,
    JobEvent as ClickHouseJobEvent, JobEventBatcher, PageEvent as ClickHousePageEvent,
    PageEventBatcher, RequestEvent as ClickHouseRequestEvent, RequestEventBatcher,
};

/// Scrapix API Server
#[derive(Parser, Debug)]
#[command(name = "scrapix-api")]
#[command(version, about = "REST API server for Scrapix crawl jobs")]
pub struct Args {
    /// Server host
    #[arg(short = 'H', long, env = "HOST", default_value = "0.0.0.0")]
    pub host: String,

    /// Server port
    #[arg(short, long, env = "PORT", default_value = "8080")]
    pub port: u16,

    /// Kafka/Redpanda broker addresses
    #[arg(short, long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
    pub brokers: String,

    /// PostgreSQL database URL
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: Option<String>,

    /// JWT_SECRET: ignored by the engine (the Lab verifies sessions).
    #[arg(long, env = "JWT_SECRET")]
    pub jwt_secret: Option<String>,

    /// Deployment mode: `standalone` (default, self-hosted, admin key) or
    /// `hosted` (Lab control plane; the engine keeps its own database).
    #[arg(long, env = "SCRAPIX_MODE", default_value = "standalone")]
    pub mode: String,

    /// Standalone admin key (min 16 chars). Accepted as `Authorization:
    /// Bearer` or `X-API-Key`.
    #[arg(long, env = "SCRAPIX_ADMIN_KEY", hide_env_values = true)]
    pub admin_key: Option<String>,

    /// `disabled` turns auth off in standalone (local dev only).
    #[arg(long, env = "SCRAPIX_AUTH")]
    pub auth: Option<String>,

    /// Hosted: the Lab base URL — events go to {LAB_URL}/internal/events, lookups to {LAB_URL}/internal/*.
    #[arg(long, env = "LAB_URL")]
    pub lab_url: Option<String>,

    /// Lab endpoint receiving usage/job events (deprecated: set LAB_URL).
    #[arg(long, env = "LAB_EVENTS_URL")]
    pub lab_events_url: Option<String>,

    /// Deprecated and ignored: event batches are signed with LAB_INSTANCE_SECRET.
    #[arg(long, env = "LAB_EVENTS_SECRET", hide_env_values = true)]
    pub lab_events_secret: Option<String>,

    /// Hosted only (min 32 chars): the token the Lab presents when it calls this engine for an account (X-Scrapix-Account-Id).
    #[arg(long, env = "LAB_SERVICE_TOKEN", hide_env_values = true)]
    pub lab_service_token: Option<String>,

    /// The instance id the Lab minted for this hosted engine deployment
    /// (uuid). Required in hosted mode, refused in standalone mode.
    #[arg(long, env = "LAB_INSTANCE_ID")]
    pub lab_instance_id: Option<String>,

    /// The secret the Lab minted with LAB_INSTANCE_ID (64 hex chars).
    #[arg(long, env = "LAB_INSTANCE_SECRET", hide_env_values = true)]
    pub lab_instance_secret: Option<String>,

    /// Default Meilisearch for crawls and /search in standalone.
    #[arg(long, env = "MEILISEARCH_URL")]
    pub meilisearch_url: Option<String>,

    /// API key for the default Meilisearch.
    #[arg(long, env = "MEILISEARCH_API_KEY", hide_env_values = true)]
    pub meilisearch_api_key: Option<String>,

    /// Maximum jobs to keep in memory
    #[arg(long, env = "MAX_JOBS", default_value = "10000")]
    pub max_jobs: usize,

    /// Fail a Running job that received no pipeline event for this many
    /// seconds while its work is not fully accounted for (R5).
    #[arg(long, env = "JOB_STALL_TIMEOUT_SECS", default_value = "1800")]
    pub job_stall_timeout_secs: u64,

    /// A job's work accounting must stay balanced for this long before the
    /// job is finalized (absorbs transient frontier snapshots and late
    /// sitemap events).
    #[arg(long, env = "JOB_COMPLETION_GRACE_MS", default_value = "3000")]
    pub completion_grace_ms: u64,

    /// A Running job whose work is not fully accounted for and that received
    /// no pipeline event for this many seconds gets its `Resume` control
    /// re-published (a lost or reordered Resume would otherwise leave the
    /// frontier paused until the stall timeout fails the job). Resume is a
    /// no-op for a job the frontier already runs.
    #[arg(long, env = "RESUME_HEAL_AFTER_SECS", default_value = "60")]
    pub resume_heal_after_secs: u64,

    /// Maximum event acks held while waiting for the accounting flush; at
    /// the cap the event consumer blocks (backpressure) until a flush frees
    /// room.
    #[arg(long, env = "MAX_PENDING_ACKS", default_value = "50000")]
    pub max_pending_acks: usize,

    /// Allow webhook deliveries to reach private/loopback/link-local
    /// addresses. Off by default (SSRF protection, same policy as the
    /// crawler's own `ALLOW_PRIVATE_IPS`); tests turn it on to reach a
    /// local wiremock server.
    #[arg(long, env = "ALLOW_PRIVATE_IPS", default_value = "false")]
    pub allow_private_ips: bool,

    /// Maximum number of webhook deliveries in flight at once, across all
    /// jobs and hooks (SCR-72). Bounds resource use under load; does not
    /// need to be large relative to hook count since a slow/blackholed
    /// endpoint only ties up one slot regardless of how long it hangs.
    #[arg(
        long,
        env = "WEBHOOK_MAX_CONCURRENT_DELIVERIES",
        default_value_t = webhooks::DEFAULT_MAX_CONCURRENT_DELIVERIES
    )]
    pub webhook_max_concurrent_deliveries: usize,

    /// Whether the crawler workers can render pages in a browser
    /// (`crawler_type: "browser"` crawls). Unset: unknown, browser crawls
    /// are accepted. `false`: they are refused at `POST /crawl` (503
    /// `render_js_unavailable`). `scrapix all` sets it from its in-process
    /// crawler's `BROWSER_RENDER`.
    #[arg(long, env = "CRAWL_BROWSER_AVAILABLE")]
    pub crawl_browser_available: Option<bool>,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,
}

/// Crawl job state: jobs, events, activity tracking
struct CrawlState {
    /// Job state storage (in-memory, could be Redis)
    jobs: RwLock<HashMap<String, JobState>>,
    /// Event broadcaster for SSE
    event_tx: broadcast::Sender<(String, CrawlEvent)>,
    /// Time of the last pipeline event applied per job (stall detection)
    job_last_activity: RwLock<HashMap<String, std::time::Instant>>,
    /// Job IDs with pending counter updates awaiting DB flush
    dirty_jobs: RwLock<HashSet<String>>,
    /// Exact work accounting per non-terminal job (R5). An entry exists from
    /// job creation (or startup recovery) until the job is terminal; it is
    /// freed then (and whenever the job itself is evicted).
    accounting: RwLock<HashMap<String, JobAccounting>>,
    /// Start of the current uninterrupted balanced streak per Running job
    /// (completion grace period).
    balanced_since: RwLock<HashMap<String, std::time::Instant>>,
    /// Acks of events that changed a job's accounting, held until the
    /// accounting flush containing them succeeded (R-19): the Kafka offset
    /// only advances past an event once its effect is durable.
    /// Held acks with the job each event belongs to.
    pending_acks: parking_lot::Mutex<Vec<(String, Ack)>>,
    /// Terminal job snapshots whose checked full write is still owed; their
    /// jobs' held acks are released only once it succeeded.
    terminal_pending: RwLock<HashMap<String, JobState>>,
    /// Acks taken by the flush in progress (`begin_flush` .. `finish_flush`).
    /// Counted against `max_pending_acks` together with `pending_acks`, and
    /// only changed while holding the `pending_acks` lock, so
    /// `pending_acks.len() + in_flight_acks` (everything held: waiting,
    /// in flight, or retained for a failed terminal write) never exceeds the
    /// cap.
    in_flight_acks: std::sync::atomic::AtomicUsize,
    /// When the "consumer blocked at the ack cap" warning last fired
    /// (shared, so it fires at most once a minute across `settle_ack` calls).
    ack_cap_warned_at: parking_lot::Mutex<Option<std::time::Instant>>,
    /// When each currently Paused job was paused (R-22 self-heal grace).
    paused_since: parking_lot::Mutex<HashMap<String, std::time::Instant>>,
    /// Last self-heal re-publish of a `JobControl`, per job (rate limit).
    control_republished: parking_lot::Mutex<HashMap<String, std::time::Instant>>,
    /// Lab events (crawl charge, lifecycle email) owed by a job's terminal
    /// write, hosted only. The job's terminal status is persisted only after
    /// these were recorded in the outbox (by the flush, see `flush_to_db`):
    /// a failed record leaves both owed, and the deterministic event ids
    /// make any re-finalization record nothing new.
    pending_lab_events: parking_lot::Mutex<HashMap<String, Vec<lab_events::LabEvent>>>,
    /// Wakes the flush loop as soon as a terminal write gated on Lab events
    /// becomes owed, instead of waiting for its next tick.
    terminal_flush_wake: tokio::sync::Notify,
}

/// Diagnostics: errors, domain stats, service health
struct DiagnosticsState {
    /// Recent errors ring buffer (for diagnostics)
    recent_errors: RwLock<VecDeque<diagnostics::ErrorRecord>>,
    /// Per-(account, domain) counters (for diagnostics)
    domain_counters: RwLock<HashMap<diagnostics::DomainKey, diagnostics::DomainCounter>>,
    /// Last time each service type was seen (for health monitoring)
    service_last_seen: RwLock<HashMap<String, std::time::Instant>>,
    /// Number of job completion/failure emails requested (one per terminal
    /// job; test hook for the single-email invariant, R5)
    job_emails_requested: std::sync::atomic::AtomicU64,
    /// Number of job billing requests and total pages billed (test hook /
    /// observability; one request per billed terminal job)
    job_bills_requested: std::sync::atomic::AtomicU64,
    pages_billed: std::sync::atomic::AtomicU64,
}

/// ClickHouse analytics batchers
struct AnalyticsState {
    /// Request event batcher (billing atom: 1 row per API call)
    request_batcher: Option<Arc<RequestEventBatcher>>,
    /// AI usage batcher (per-LLM-call tracking)
    ai_usage_batcher: Option<Arc<AiUsageBatcher>>,
    /// Job event batcher (lifecycle: JobStarted/Completed/Failed)
    job_event_batcher: Option<Arc<JobEventBatcher>>,
    /// Page event batcher (one row per crawled/failed page)
    page_event_batcher: Option<Arc<PageEventBatcher>>,
}

/// Application state shared across handlers
struct AppState {
    /// Message bus producer for publishing URLs
    producer: AnyProducer,
    /// Configuration
    config: AppConfig,
    /// Crawl job state
    crawl: CrawlState,
    /// Diagnostics and observability
    diagnostics: DiagnosticsState,
    /// Analytics batchers
    analytics: AnalyticsState,
    /// Shared HTTP fetcher for /scrape endpoint (connection pooling, retries, DNS cache)
    fetcher: Arc<HttpFetcher>,
    /// Optional browser renderer for JS rendering in /scrape and /map
    browser_renderer: Option<Arc<CdpRenderer>>,
    /// Optional AI service for /scrape enrichment
    ai_service: Option<Arc<AiService>>,
    /// OCR engine for scanned documents on /scrape and /parse (opt-in per
    /// request via `parsers.ocr`); `None` with `OCR_BACKEND=off`.
    ocr: Option<Arc<scrapix_ocr::OcrEngine>>,
    /// Records usage and job events for the Lab (hosted only; `None` in
    /// standalone, where nothing is billed or emailed).
    pub(crate) lab: Option<Arc<lab_events::Lab>>,
    /// Durable job state and engine-job results (`jobs`, `job_results`).
    pub(crate) job_store: Option<Arc<dyn job_store::JobStore>>,
    /// The Lab's internal API (hosted only): credit pre-check and
    /// Meilisearch lookups. `None` in standalone.
    pub(crate) lab_api: Option<Arc<lab_client::LabClient>>,
    /// Optional ClickHouse analytics store (used for event history queries)
    analytics_store: Option<Arc<analytics::AnalyticsState>>,
    /// Delivers `CrawlEvent`s to jobs' subscribed webhooks (SCR-72).
    webhook_dispatcher: webhooks::WebhookDispatcher,
    /// Accounting is persisted and event acks are deferred until the flush
    /// (true when a job store is configured; turned off for the process if
    /// the `accounting` column turns out to be missing, see `finish_flush`).
    accounting_persisted: std::sync::atomic::AtomicBool,
    /// Set on shutdown (unblocks a `settle_ack` waiting at the cap).
    shutting_down: std::sync::atomic::AtomicBool,
    /// Ordered `JobControl` queue: one drainer task publishes the controls
    /// one at a time, in request order, so a Pause and the Resume after it
    /// can never reach the bus swapped (see `publish_control`).
    control_tx: tokio::sync::mpsc::UnboundedSender<JobControl>,
    /// Receiving end, taken by the drainer when it is first needed.
    control_rx: parking_lot::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<JobControl>>>,
    /// Controls queued but not yet published (or given up on).
    controls_pending: Arc<std::sync::atomic::AtomicUsize>,
    /// Job results layer (`GET /job/{id}/results`, SCR-71).
    results: results::ResultsState,
    /// Resolves which Meilisearch instance a crawl/search/results request
    /// uses: env-configured in standalone mode, the account's
    /// `meilisearch_engines` rows in hosted mode.
    pub(crate) meili: Arc<dyn crate::meili::MeilisearchResolver>,
    /// `SCRAPIX_AUTH=disabled`: `/health` keeps reminding the logs that
    /// every route is unauthenticated (see [`AppState::warn_auth_disabled`]).
    pub(crate) auth_disabled: bool,
    /// Last time `/health` logged the auth-disabled warning.
    auth_disabled_warned_at: parking_lot::Mutex<Option<std::time::Instant>>,
    /// Whether the crawler workers have a browser (`None`: unknown), see
    /// [`Args::crawl_browser_available`].
    pub(crate) crawl_browser: Option<bool>,
}

#[derive(Debug, Clone)]
struct AppConfig {
    max_jobs: usize,
    /// See [`Args::job_stall_timeout_secs`]
    job_stall_timeout: Duration,
    /// See [`Args::completion_grace_ms`]
    completion_grace: Duration,
    /// See [`Args::resume_heal_after_secs`]
    resume_heal_after: Duration,
    /// See [`Args::max_pending_acks`]
    max_pending_acks: usize,
}

impl AppConfig {
    fn from_args(args: &Args) -> Self {
        Self {
            max_jobs: args.max_jobs,
            job_stall_timeout: Duration::from_secs(args.job_stall_timeout_secs),
            completion_grace: Duration::from_millis(args.completion_grace_ms),
            resume_heal_after: Duration::from_secs(args.resume_heal_after_secs.max(1)),
            max_pending_acks: args.max_pending_acks.max(1),
        }
    }
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        producer: AnyProducer,
        config: AppConfig,
        request_batcher: Option<Arc<RequestEventBatcher>>,
        ai_usage_batcher: Option<Arc<AiUsageBatcher>>,
        job_event_batcher: Option<Arc<JobEventBatcher>>,
        page_event_batcher: Option<Arc<PageEventBatcher>>,
        fetcher: Arc<HttpFetcher>,
        browser_renderer: Option<Arc<CdpRenderer>>,
        ai_service: Option<Arc<AiService>>,
        lab_api: Option<Arc<lab_client::LabClient>>,
        job_store: Option<Arc<dyn job_store::JobStore>>,
        analytics_store: Option<Arc<analytics::AnalyticsState>>,
        webhook_dispatcher: webhooks::WebhookDispatcher,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(10_000);
        let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            producer,
            config,
            crawl: CrawlState {
                jobs: RwLock::new(HashMap::new()),
                event_tx,
                job_last_activity: RwLock::new(HashMap::new()),
                dirty_jobs: RwLock::new(HashSet::new()),
                accounting: RwLock::new(HashMap::new()),
                balanced_since: RwLock::new(HashMap::new()),
                pending_acks: parking_lot::Mutex::new(Vec::new()),
                terminal_pending: RwLock::new(HashMap::new()),
                in_flight_acks: std::sync::atomic::AtomicUsize::new(0),
                ack_cap_warned_at: parking_lot::Mutex::new(None),
                paused_since: parking_lot::Mutex::new(HashMap::new()),
                control_republished: parking_lot::Mutex::new(HashMap::new()),
                pending_lab_events: parking_lot::Mutex::new(HashMap::new()),
                terminal_flush_wake: tokio::sync::Notify::new(),
            },
            diagnostics: DiagnosticsState {
                recent_errors: RwLock::new(VecDeque::with_capacity(1000)),
                domain_counters: RwLock::new(HashMap::new()),
                service_last_seen: RwLock::new(HashMap::new()),
                job_emails_requested: std::sync::atomic::AtomicU64::new(0),
                job_bills_requested: std::sync::atomic::AtomicU64::new(0),
                pages_billed: std::sync::atomic::AtomicU64::new(0),
            },
            analytics: AnalyticsState {
                request_batcher,
                ai_usage_batcher,
                job_event_batcher,
                page_event_batcher,
            },
            fetcher,
            browser_renderer,
            ai_service,
            ocr: None,
            lab: None,
            accounting_persisted: std::sync::atomic::AtomicBool::new(job_store.is_some()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            control_tx,
            control_rx: parking_lot::Mutex::new(Some(control_rx)),
            controls_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            results: results::ResultsState::default(),
            meili: Arc::new(crate::meili::EnvResolver(None)),
            auth_disabled: false,
            auth_disabled_warned_at: parking_lot::Mutex::new(None),
            crawl_browser: None,
            lab_api,
            job_store,
            analytics_store,
            webhook_dispatcher,
        }
    }

    /// With auth disabled, log a WARN at most once per minute (called from
    /// `/health`, so a probed deployment keeps saying it is open). Returns
    /// whether it logged.
    fn warn_auth_disabled(&self, now: std::time::Instant) -> bool {
        if !self.auth_disabled {
            return false;
        }
        let mut warned_at = self.auth_disabled_warned_at.lock();
        if warned_at.is_some_and(|at| now.duration_since(at) < Duration::from_secs(60)) {
            return false;
        }
        warn!("SCRAPIX_AUTH=disabled: every route is UNAUTHENTICATED. Local development only.");
        *warned_at = Some(now);
        true
    }

    /// Create a new job
    fn create_job(&self, job_id: &str, index_uid: &str) -> JobState {
        self.insert_job(JobState::new(job_id, index_uid))
    }

    /// Insert a new job into the in-memory map, evicting terminal jobs (and
    /// their per-job tracking) first when at capacity.
    fn insert_job(&self, job: JobState) -> JobState {
        let evicted: Vec<String> = {
            let mut jobs = self.crawl.jobs.write();
            let mut evicted = Vec::new();
            if jobs.len() >= self.config.max_jobs {
                // Remove oldest completed jobs first
                evicted = jobs
                    .iter()
                    .filter(|(_, j)| is_terminal(&j.status))
                    .map(|(id, _)| id.clone())
                    .take(self.config.max_jobs / 10)
                    .collect();
                for id in &evicted {
                    jobs.remove(id);
                }
            }
            jobs.insert(job.job_id.clone(), job.clone());
            evicted
        };
        for id in &evicted {
            self.forget_job_tracking(id);
            self.crawl.control_republished.lock().remove(id);
        }
        job
    }

    /// Serialized accounting of the given jobs (those that still have one),
    /// for the job-store flush.
    fn accounting_snapshots(&self, job_ids: &[String]) -> Vec<(String, serde_json::Value)> {
        let accs = self.crawl.accounting.read();
        job_ids
            .iter()
            .filter_map(|id| {
                let v = serde_json::to_value(accs.get(id)?).ok()?;
                Some((id.clone(), v))
            })
            .collect()
    }

    /// Drop the per-job tracking state (accounting incl. its seen-sets,
    /// balanced streak, last activity) of a job that is terminal or evicted.
    fn forget_job_tracking(&self, job_id: &str) {
        self.crawl.paused_since.lock().remove(job_id);
        self.crawl.accounting.write().remove(job_id);
        self.crawl.balanced_since.write().remove(job_id);
        self.crawl.job_last_activity.write().remove(job_id);
    }

    /// Get a job by ID
    fn get_job(&self, job_id: &str) -> Option<JobState> {
        self.crawl.jobs.read().get(job_id).cloned()
    }

    /// Whether `job_id` is a pipeline (crawl) job, as opposed to a job the
    /// API runs itself (batch scrape, extract). Unknown jobs count as crawls.
    fn is_pipeline_job(&self, job_id: &str) -> bool {
        self.crawl
            .jobs
            .read()
            .get(job_id)
            .is_none_or(|j| job_kind::JobKind::of(j).is_pipeline())
    }

    /// The account a diagnostics record of `job_id` belongs to: the job's
    /// own (engine-owned), else the one the event carries.
    fn diagnostics_account(&self, job_id: &str, event: &CrawlEvent) -> Option<String> {
        let job_account = self
            .crawl
            .jobs
            .read()
            .get(job_id)
            .and_then(|j| j.account_id.clone());
        job_account.or_else(|| match event {
            CrawlEvent::PageCrawled { account_id, .. }
            | CrawlEvent::PageFailed { account_id, .. } => account_id.clone(),
            _ => None,
        })
    }

    /// Update a job
    fn update_job<F>(&self, job_id: &str, f: F) -> Option<JobState>
    where
        F: FnOnce(&mut JobState),
    {
        let mut jobs = self.crawl.jobs.write();
        if let Some(job) = jobs.get_mut(job_id) {
            f(job);
            Some(job.clone())
        } else {
            None
        }
    }

    /// List all jobs (in-memory, unordered)
    #[cfg(test)]
    fn list_jobs(&self, limit: usize, offset: usize) -> Vec<JobState> {
        let jobs = self.crawl.jobs.read();
        jobs.values().skip(offset).take(limit).cloned().collect()
    }

    /// Broadcast an event
    fn broadcast_event(&self, job_id: &str, event: CrawlEvent) {
        if let Err(e) = self.crawl.event_tx.send((job_id.to_string(), event)) {
            debug!("No active event subscribers, dropping event: {}", e);
        }
    }

    /// A job just became terminal (completed/failed): write it through to
    /// the job store immediately and free its per-job tracking state.
    fn on_terminal(&self, job_id: &str, updated: Option<JobState>) {
        self.forget_job_tracking(job_id);
        match updated {
            Some(snapshot) => self.write_terminal(snapshot),
            // A terminal event for a job this process does not know: there
            // is no terminal status to persist, so its events (if any) are
            // recorded on their own, best effort.
            None => self.record_unowned_lab_events(job_id),
        }
    }

    /// Record, in the background, the Lab events of a job that has no
    /// terminal write to order them against.
    fn record_unowned_lab_events(&self, job_id: &str) {
        let events = self.take_pending_lab_events(job_id);
        if let (false, Some(lab)) = (events.is_empty(), self.lab.clone()) {
            tokio::spawn(async move {
                // Failures are logged by `Lab::record`.
                let _ = lab.record(&events).await;
            });
        }
    }

    /// Persist a terminal job.
    ///
    /// Without Lab events owed by the job: written through to the job store
    /// now (best effort, for latency) and, while acks are deferred, a
    /// checked write is owed to the next flush (the job's held acks wait for
    /// it).
    ///
    /// With Lab events owed (hosted: its crawl charge / lifecycle email,
    /// queued before this call): no direct write. The terminal write is
    /// always owed to the flush, which records the events first and writes
    /// the terminal status only once they are durably in the outbox
    /// (`flush_to_db`), so a crash can never persist a terminal job whose
    /// charge was not recorded.
    fn write_terminal(&self, snapshot: JobState) {
        let job_id = snapshot.job_id.clone();
        self.crawl.dirty_jobs.write().remove(&job_id);
        let owes_events = self.has_pending_lab_events(&job_id);
        if self.accounting_persisted() || owes_events {
            self.crawl
                .terminal_pending
                .write()
                .insert(job_id, snapshot.clone());
        }
        if owes_events {
            self.crawl.terminal_flush_wake.notify_one();
            return;
        }
        if let Some(store) = self.job_store.clone() {
            tokio::spawn(async move {
                let _ = store.update_job_full(&snapshot).await;
            });
        }
    }

    fn accounting_persisted(&self) -> bool {
        self.accounting_persisted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Owe `event` to `job_id`'s terminal write (hosted only: a no-op
    /// without a Lab). Must be called before the job's `write_terminal`.
    /// An event whose account is not a uuid is dropped with a warning: the
    /// outbox would refuse it forever, and with it the job's terminal write.
    fn owe_lab_event(&self, job_id: &str, event: lab_events::LabEvent) {
        if self.lab.is_none() {
            return;
        }
        if uuid::Uuid::parse_str(&event.account_id).is_err() {
            warn!(
                job_id = %job_id,
                account_id = %event.account_id,
                kind = %event.kind,
                "Not recording a lab event for a non-uuid account"
            );
            return;
        }
        self.crawl
            .pending_lab_events
            .lock()
            .entry(job_id.to_string())
            .or_default()
            .push(event);
    }

    /// An account's pending/running jobs, for the concurrent-job quota: the
    /// store's active rows, minus those this process already knows are
    /// terminal (their terminal write may still be owed, e.g. waiting for
    /// its Lab events to be recorded, so the row still reads active).
    pub(crate) async fn active_job_count(&self, account_id: &str) -> i64 {
        let Some(ref store) = self.job_store else {
            return 0;
        };
        let mut ids = store.active_job_ids(account_id).await.unwrap_or_default();
        {
            let jobs = self.crawl.jobs.read();
            ids.retain(|id| !jobs.get(id).is_some_and(|j| is_terminal(&j.status)));
        }
        // Also terminal: evicted from memory with the write still owed.
        let owed = self.crawl.terminal_pending.read();
        ids.retain(|id| !owed.contains_key(id));
        ids.len() as i64
    }

    fn has_pending_lab_events(&self, job_id: &str) -> bool {
        self.crawl
            .pending_lab_events
            .lock()
            .get(job_id)
            .is_some_and(|events| !events.is_empty())
    }

    /// Take the Lab events owed by `job_id`'s terminal write.
    fn take_pending_lab_events(&self, job_id: &str) -> Vec<lab_events::LabEvent> {
        self.crawl
            .pending_lab_events
            .lock()
            .remove(job_id)
            .unwrap_or_default()
    }

    /// Put back events whose record failed (ahead of any queued since).
    fn requeue_pending_lab_events(&self, job_id: &str, events: Vec<lab_events::LabEvent>) {
        if events.is_empty() {
            return;
        }
        let mut pending = self.crawl.pending_lab_events.lock();
        let entry = pending.entry(job_id.to_string()).or_default();
        let later = std::mem::replace(entry, events);
        entry.extend(later);
    }

    /// Record the Lab events owed by `job_id`'s terminal write. `true` when
    /// the terminal status may now be persisted (nothing owed, or recorded);
    /// `false` when the record failed: the events are put back and the
    /// terminal write must stay owed.
    async fn record_owed_lab_events(&self, job_id: &str) -> bool {
        let events = self.take_pending_lab_events(job_id);
        if events.is_empty() {
            return true;
        }
        let Some(lab) = self.lab.as_ref() else {
            return true; // not reachable: events are only owed with a Lab
        };
        match lab.record(&events).await {
            Ok(()) => true,
            Err(_) => {
                // Logged by `Lab::record`.
                self.requeue_pending_lab_events(job_id, events);
                false
            }
        }
    }

    /// Queue a job's lifecycle event (the Lab emails the account). Called
    /// only from `process_event`'s terminal branches, which run at most once
    /// per job, before the job's terminal write.
    fn request_job_event(
        &self,
        job_id: &str,
        make_event: fn(&str, &str, serde_json::Value) -> lab_events::LabEvent,
        account_id: Option<String>,
        payload: serde_json::Value,
    ) {
        self.diagnostics
            .job_emails_requested
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(acct_id) = account_id else {
            return;
        };
        self.owe_lab_event(job_id, make_event(job_id, &acct_id, payload));
    }

    /// One completion-loop tick (R5): refresh each Running job's balanced
    /// streak and return the jobs that must be finalized now.
    fn completion_decisions(&self, now: std::time::Instant) -> Vec<(String, Finalize)> {
        let running: HashSet<String> = self
            .crawl
            .jobs
            .read()
            .iter()
            .filter(|(_, j)| {
                matches!(j.status, JobStatus::Running) && job_kind::JobKind::of(j).is_pipeline()
            })
            .map(|(id, _)| id.clone())
            .collect();

        let accs = self.crawl.accounting.read();
        let mut since = self.crawl.balanced_since.write();
        let mut activity = self.crawl.job_last_activity.write();
        since.retain(|id, _| running.contains(id));

        // A Running job without an entry (should not happen: entries are
        // created with the job and recovered at startup) is treated as having
        // no accounted work, so it stalls out instead of running forever.
        let untracked = JobAccounting::default();
        let mut out = Vec::new();
        for id in running {
            let acc = accs.get(&id).unwrap_or(&untracked);
            if acc.is_balanced() {
                since.entry(id.clone()).or_insert(now);
            } else {
                since.remove(&id);
            }
            let last_event = *activity.entry(id.clone()).or_insert(now);
            let decision = finalize_decision(
                acc,
                since.get(&id).copied(),
                last_event,
                now,
                self.config.completion_grace,
                self.config.job_stall_timeout,
            );
            if decision != Finalize::Wait {
                out.push((id, decision));
            }
        }
        out
    }

    /// Finalize a Running job per `decision`: run Replace-strategy cleanup
    /// on true completion, apply the terminal event through `process_event`
    /// (which alone schedules the email), and tell the pipeline to release
    /// the job's state.
    ///
    /// The decision is re-validated against the job's current state first:
    /// decisions are computed for all jobs up front and finalized one after
    /// the other, so an earlier slow Replace cleanup can make a later
    /// decision stale before its own destructive cleanup.
    ///
    /// `now` is the re-validation time (the loop passes `Instant::now()`).
    async fn finalize_job(&self, job_id: &str, decision: Finalize, now: std::time::Instant) {
        let Some(job) = self.get_job(job_id) else {
            return;
        };
        if !matches!(job.status, JobStatus::Running) {
            return;
        }
        let acc = self
            .crawl
            .accounting
            .read()
            .get(job_id)
            .cloned()
            .unwrap_or_default();
        let still_valid = match decision {
            Finalize::Wait => false,
            // Don't fail a job that crawled pages in the meantime (or
            // complete one that turned out to have none).
            Finalize::Complete => acc.is_balanced() && acc.pages_crawled_ok > 0,
            Finalize::FailNoPages => acc.is_balanced() && acc.pages_crawled_ok == 0,
            Finalize::FailStalled => {
                !acc.is_balanced()
                    && self
                        .crawl
                        .job_last_activity
                        .read()
                        .get(job_id)
                        .is_none_or(|last| {
                            now.saturating_duration_since(*last) >= self.config.job_stall_timeout
                        })
            }
        };
        if !still_valid {
            debug!(job_id = %job_id, ?decision, "Finalize decision went stale, skipping");
            if decision != Finalize::FailStalled {
                self.crawl.balanced_since.write().remove(job_id);
            }
            return;
        }

        let timestamp = chrono::Utc::now().timestamp_millis();
        let failed = |error: String| CrawlEvent::JobFailed {
            job_id: job_id.to_string(),
            account_id: job.account_id.clone(),
            error,
            timestamp,
        };

        let event = match decision {
            Finalize::Wait => return,
            Finalize::FailStalled => failed(format!(
                "Stalled: no progress for {}s",
                self.config.job_stall_timeout.as_secs()
            )),
            Finalize::FailNoPages => failed(format!(
                "No page could be crawled ({} failures)",
                acc.pages_failed
            )),
            Finalize::Complete => match replace_cleanup(&job, acc.documents_indexed).await {
                Ok(documents_indexed) => CrawlEvent::JobCompleted {
                    job_id: job_id.to_string(),
                    account_id: job.account_id.clone(),
                    pages_crawled: acc.pages_crawled_ok,
                    documents_indexed,
                    errors: acc.pages_failed,
                    bytes_downloaded: acc.bytes_downloaded,
                    duration_secs: job.duration_seconds().unwrap_or(0).max(0) as u64,
                    timestamp,
                },
                Err(error) => failed(error),
            },
        };

        // R-20: a stalled job still pays for the pages it crawled (the old
        // idle detector completed, and so billed, such jobs). A Replace
        // cleanup failure stays unbilled, as before. Charged by
        // process_event, before the terminal write.
        let failed_charge = (decision == Finalize::FailStalled).then(|| PageCharge {
            pages_http: acc.pages_crawled_ok.saturating_sub(acc.pages_browser),
            pages_browser: acc.pages_browser,
            pages_ai: acc.pages_ai,
            pages_ocr: acc.pages_ocr,
        });

        info!(job_id = %job_id, ?decision, "Finalizing job from work accounting");
        // The terminal transition is atomic in process_event: if the job was
        // cancelled meanwhile (e.g. during the Replace cleanup), nothing is
        // applied (nor charged) and nothing else happens here.
        if !self
            .process_event_charging(job_id, &event, None, failed_charge)
            .applied
        {
            debug!(job_id = %job_id, "Job became terminal before finalize, skipping");
            return;
        }
        self.broadcast_event(job_id, event);

        // Tell the pipeline to release the job's state.
        self.publish_control(job_id, JobAction::Finish);
    }

    /// Self-heal (R-22): a pipeline event for a job whose API status is
    /// Cancelled, Completed/Failed, or Paused for longer than
    /// `PAUSE_HEAL_GRACE` means some service missed (or never got) the
    /// matching control — a lost fire-and-forget publish, a Pause that
    /// overtook the seed, a service restarting with a `latest` group.
    /// Re-publish it, at most once per `CONTROL_REPUBLISH_EVERY` per job.
    /// Lifecycle events the API produces itself are ignored.
    fn heal_control(&self, job_id: &str, event: &CrawlEvent, now: std::time::Instant) {
        if matches!(
            event,
            CrawlEvent::JobStarted { .. }
                | CrawlEvent::JobCompleted { .. }
                | CrawlEvent::JobFailed { .. }
        ) {
            return;
        }
        let Some(status) = self.crawl.jobs.read().get(job_id).map(|j| j.status.clone()) else {
            return;
        };
        let action = match status {
            JobStatus::Cancelled => JobAction::Cancel,
            JobStatus::Completed | JobStatus::Failed => JobAction::Finish,
            JobStatus::Paused => {
                // Unknown pause time (recovered at startup): heal now.
                let since = self.crawl.paused_since.lock().get(job_id).copied();
                if since.is_some_and(|t| now.saturating_duration_since(t) < PAUSE_HEAL_GRACE) {
                    return;
                }
                JobAction::Pause
            }
            JobStatus::Pending | JobStatus::Running => return,
        };
        if !self.take_republish_slot(job_id, now) {
            return;
        }
        info!(job_id = %job_id, ?action, "Pipeline event for a stopped/paused job: re-publishing its control");
        self.publish_control(job_id, action);
    }

    /// Rate limit of the self-heal re-publishes: true (and the slot taken)
    /// if `job_id` had no re-publish in the last `CONTROL_REPUBLISH_EVERY`.
    fn take_republish_slot(&self, job_id: &str, now: std::time::Instant) -> bool {
        let mut last = self.crawl.control_republished.lock();
        if last
            .get(job_id)
            .is_some_and(|t| now.saturating_duration_since(*t) < CONTROL_REPUBLISH_EVERY)
        {
            return false;
        }
        if last.len() >= MAX_CONTROL_REPUBLISH_ENTRIES {
            last.retain(|_, t| now.saturating_duration_since(*t) < CONTROL_REPUBLISH_EVERY);
        }
        last.insert(job_id.to_string(), now);
        true
    }

    /// Self-heal of a lost or reordered Resume: a Running job whose work is
    /// not balanced and that has had no pipeline event for
    /// `resume_heal_after` may be sitting `Paused` at the frontier. Re-publish
    /// `Resume` (rate-limited with the other re-publishes). Safe for a job
    /// that is just slow: the frontier ignores Resume for Running, Cancelled
    /// and Finished jobs. Called by the completion loop every tick.
    fn heal_silent_running(&self, now: std::time::Instant) {
        let threshold = self.config.resume_heal_after;
        let silent: Vec<String> = {
            let jobs = self.crawl.jobs.read();
            let accs = self.crawl.accounting.read();
            let activity = self.crawl.job_last_activity.read();
            jobs.iter()
                .filter(|(_, j)| matches!(j.status, JobStatus::Running))
                .filter(|(id, _)| accs.get(*id).is_some_and(|a| !a.is_balanced()))
                .filter(|(id, _)| {
                    activity
                        .get(*id)
                        .is_some_and(|t| now.saturating_duration_since(*t) >= threshold)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for job_id in silent {
            if self.take_republish_slot(&job_id, now) {
                info!(
                    job_id = %job_id,
                    silent_secs = threshold.as_secs(),
                    "Running job silent and unbalanced: re-publishing Resume"
                );
                self.publish_control(&job_id, JobAction::Resume);
            }
        }
    }

    /// Publish a `JobControl` to the pipeline (frontier + workers). Queued,
    /// so a slow or blocked publish never delays the caller (a finalize
    /// batch, an HTTP request), and published by a single drainer task in
    /// request order (a Pause and the Resume after it never swap). Best
    /// effort: a send that fails or times out (5 s) is logged and dropped;
    /// a lost Cancel/Finish/Pause is re-published by the self-heal on the
    /// job's next pipeline event (`heal_control`), a lost Resume by
    /// `heal_silent_running`.
    fn publish_control(&self, job_id: &str, action: JobAction) {
        self.ensure_control_drainer();
        self.controls_pending
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .control_tx
            .send(JobControl::new(job_id, action))
            .is_err()
        {
            self.controls_pending
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            warn!(job_id = %job_id, ?action, "JobControl queue closed; control dropped");
        }
    }

    /// Start the control drainer on first use (needs a runtime, which
    /// `AppState::new` does not).
    fn ensure_control_drainer(&self) {
        let Some(mut rx) = self.control_rx.lock().take() else {
            return;
        };
        let producer = self.producer.clone();
        let pending = self.controls_pending.clone();
        tokio::spawn(async move {
            while let Some(control) = rx.recv().await {
                let (job_id, action) = (control.job_id.clone(), control.action);
                match tokio::time::timeout(
                    Duration::from_secs(5),
                    producer.send(topic_names::JOB_STATUS, Some(&job_id), &control),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        warn!(job_id = %job_id, ?action, error = %e, "Failed to publish JobControl")
                    }
                    Err(_) => warn!(job_id = %job_id, ?action, "Timed out publishing JobControl"),
                }
                pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }

    /// Wait (at most `timeout`) until every queued control was published.
    /// Returns false if some were still pending at the deadline.
    async fn drain_controls(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self
                .controls_pending
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Cancel a job (R5): `Cancelled` is terminal, so the check-and-set runs
    /// under the same single `jobs.write()` as `transition_terminal` — a
    /// cancel and a completion can never both apply, and a cancel never
    /// overwrites a terminal status (`Conflict`). Only the caller that
    /// applied the transition bills the pages crawled so far, persists the
    /// terminal state and tells the pipeline to stop the job.
    fn cancel(&self, job_id: &str) -> Result<JobState, ControlError> {
        let snapshot = {
            let mut jobs = self.crawl.jobs.write();
            let j = jobs.get_mut(job_id).ok_or(ControlError::NotFound)?;
            if is_terminal(&j.status) {
                return Err(ControlError::Conflict(j.status.clone()));
            }
            j.status = JobStatus::Cancelled;
            j.completed_at = Some(chrono::Utc::now());
            j.clone()
        };
        let (pages_http, pages_browser, pages_ai, pages_ocr) =
            self.crawl.accounting.read().get(job_id).map_or(
                (snapshot.pages_crawled, 0, 0, 0),
                |acc| {
                    (
                        acc.pages_crawled_ok.saturating_sub(acc.pages_browser),
                        acc.pages_browser,
                        acc.pages_ai,
                        acc.pages_ocr,
                    )
                },
            );
        self.bill_job(
            job_id,
            snapshot.account_id.as_ref(),
            pages_http,
            pages_browser,
            pages_ai,
            pages_ocr,
        );
        // Terminal: free the accounting so the completion loop never
        // finalizes it, and persist it (checked by the next flush before
        // the job's held acks are released).
        self.forget_job_tracking(job_id);
        self.crawl.paused_since.lock().remove(job_id);
        self.write_terminal(snapshot.clone());
        self.publish_control(job_id, JobAction::Cancel);
        // Cancellation doesn't flow through the pipeline's CrawlEvent
        // stream (nothing publishes one for a cancel), so it's the one
        // terminal transition `process_event_at` never sees. Fire a
        // synthetic `JobFailed { error: "cancelled" }` directly so
        // `crawl_failed` webhook subscribers still hear about it — chosen
        // over adding a dedicated `crawl.cancelled` WebhookEvent to avoid a
        // config schema change (see webhooks.rs module docs).
        self.webhook_dispatcher.enqueue(
            &snapshot.webhooks,
            job_id,
            &CrawlEvent::JobFailed {
                job_id: job_id.to_string(),
                account_id: snapshot.account_id.clone(),
                error: "cancelled".to_string(),
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
        );
        info!(
            job_id = %job_id,
            pages_billed = pages_http + pages_browser,
            "Job cancelled"
        );
        Ok(snapshot)
    }

    /// Pause a Running job: the frontier stops dispatching it (in-flight
    /// pages still finish and report), and the completion loop neither
    /// finalizes nor stall-fails it while paused.
    fn pause(&self, job_id: &str) -> Result<JobState, ControlError> {
        let snapshot = self.transition_status(job_id, JobStatus::Running, JobStatus::Paused)?;
        self.crawl
            .paused_since
            .lock()
            .insert(job_id.to_string(), std::time::Instant::now());
        self.crawl.balanced_since.write().remove(job_id);
        self.crawl.dirty_jobs.write().insert(job_id.to_string());
        self.publish_control(job_id, JobAction::Pause);
        info!(job_id = %job_id, "Job paused");
        Ok(snapshot)
    }

    /// Resume a Paused job, restarting its stall clock (the time spent
    /// paused is not a stall).
    fn resume(&self, job_id: &str) -> Result<JobState, ControlError> {
        let snapshot = self.transition_status(job_id, JobStatus::Paused, JobStatus::Running)?;
        self.crawl.paused_since.lock().remove(job_id);
        self.crawl
            .job_last_activity
            .write()
            .insert(job_id.to_string(), std::time::Instant::now());
        self.crawl.balanced_since.write().remove(job_id);
        self.crawl.dirty_jobs.write().insert(job_id.to_string());
        self.publish_control(job_id, JobAction::Resume);
        info!(job_id = %job_id, "Job resumed");
        Ok(snapshot)
    }

    /// Atomic `from` → `to` status change (`Conflict` with the current
    /// status otherwise).
    fn transition_status(
        &self,
        job_id: &str,
        from: JobStatus,
        to: JobStatus,
    ) -> Result<JobState, ControlError> {
        let mut jobs = self.crawl.jobs.write();
        let j = jobs.get_mut(job_id).ok_or(ControlError::NotFound)?;
        if j.status != from {
            return Err(ControlError::Conflict(j.status.clone()));
        }
        j.status = to;
        Ok(j.clone())
    }

    /// Report the crawled pages of a finished job: owes its crawl usage event
    /// (deterministic id, one per job) to the job's terminal write, so it
    /// must be called before `write_terminal`. The event carries raw units
    /// plus, for the contract v2 transition release, the pre-v2 `credits`
    /// (`legacy_credits::crawl_credits` + `ocr_credits`, from the job's
    /// enabled features). The single billing path for terminal jobs.
    /// D4/R4: units count what was actually delivered, not the job's static
    /// config — `pages_http`/`pages_browser` split the crawled-ok page count
    /// by whether each page was actually rendered with a browser
    /// (`PageCrawled.js_rendered`), `pages_ai` counts only pages that were
    /// actually AI-enriched (`DocumentIndexed.ai_enriched`), and `pages_ocr`
    /// counts OCR'd document pages (`DocumentIndexed.ocr_pages`).
    fn bill_job(
        &self,
        job_id: &str,
        account_id: Option<&String>,
        pages_http: u64,
        pages_browser: u64,
        pages_ai: u64,
        pages_ocr: u64,
    ) {
        let total_pages = pages_http + pages_browser;
        if total_pages == 0 || !self.is_pipeline_job(job_id) {
            return;
        }
        self.diagnostics
            .job_bills_requested
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.diagnostics
            .pages_billed
            .fetch_add(total_pages, std::sync::atomic::Ordering::Relaxed);

        let (Some(_), Some(acct_id)) = (self.lab.as_ref(), account_id) else {
            return;
        };
        let description = if pages_ocr > 0 {
            format!(
                "Job {} ({} http + {} browser pages, {} AI-enriched, {} OCR pages)",
                job_id, pages_http, pages_browser, pages_ai, pages_ocr
            )
        } else {
            format!(
                "Job {} ({} http + {} browser pages, {} AI-enriched)",
                job_id, pages_http, pages_browser, pages_ai
            )
        };
        // The job's enabled features (persisted config): they set the
        // per-page feature surcharge, reported as `feature_pages`.
        let features = {
            let jobs = self.crawl.jobs.read();
            jobs.get(job_id)
                .and_then(|j| j.config.as_ref())
                .and_then(|cfg| {
                    cfg.get("features")
                        .and_then(|v| serde_json::from_value::<FeaturesConfig>(v.clone()).ok())
                })
                .unwrap_or_default()
        };
        let feature_pages =
            total_pages.saturating_mul(legacy_credits::non_ai_feature_credits(&features) as u64);
        let credits = legacy_credits::crawl_credits(pages_http, pages_browser, pages_ai, &features)
            + legacy_credits::ocr_credits(pages_ocr);
        let units = serde_json::json!({
            "pages_http": pages_http,
            "pages_browser": pages_browser,
            "pages_ai": pages_ai,
            "pages_ocr": pages_ocr,
            "feature_pages": feature_pages,
        });
        let event =
            lab_events::LabEvent::crawl_final_usage(job_id, acct_id, credits, units, description);
        self.owe_lab_event(job_id, event);
    }

    /// Atomically apply the state change of a terminal event: check and set
    /// under one `jobs.write()`, so two terminal events (or a terminal event
    /// and a cancel) can never both transition the job.
    fn transition_terminal(&self, job_id: &str, event: &CrawlEvent) -> TerminalTransition {
        if !matches!(
            event,
            CrawlEvent::JobCompleted { .. } | CrawlEvent::JobFailed { .. }
        ) {
            return TerminalTransition::NotTerminal;
        }
        let now = chrono::Utc::now();
        let mut jobs = self.crawl.jobs.write();
        let Some(j) = jobs.get_mut(job_id) else {
            return TerminalTransition::UnknownJob;
        };
        if is_terminal(&j.status) {
            return TerminalTransition::AlreadyTerminal;
        }
        match event {
            CrawlEvent::JobCompleted {
                pages_crawled,
                documents_indexed,
                duration_secs,
                ..
            } => {
                j.status = JobStatus::Completed;
                j.pages_crawled = *pages_crawled;
                j.pages_indexed = *documents_indexed;
                j.completed_at = Some(now);
                if *duration_secs > 0 {
                    j.crawl_rate = j.pages_crawled as f64 / *duration_secs as f64;
                }
            }
            CrawlEvent::JobFailed { error, .. } => {
                j.status = JobStatus::Failed;
                j.error_message = Some(error.clone());
                j.completed_at = Some(now);
            }
            _ => unreachable!("checked above"),
        }
        TerminalTransition::Applied(Box::new(j.clone()))
    }

    /// Hand an event's ack back: acked now, unless the event changed a job's
    /// accounting and accounting is persisted, in which case it is held
    /// until the flush containing it succeeds (R-19).
    ///
    /// At most `max_pending_acks` acks are held: at the cap this waits until
    /// a flush frees room (the consumer runs at concurrency 1, so this
    /// backpressures consumption instead of growing memory), or until
    /// shutdown, where the ack is dropped un-acked (redelivered).
    async fn settle_ack(&self, job_id: &str, ack: Ack, outcome: EventOutcome) {
        if !outcome.accounting_touched || !self.accounting_persisted() {
            ack.ack();
            return;
        }
        let mut ack = Some(ack);
        loop {
            {
                let mut pending = self.crawl.pending_acks.lock();
                if !self.accounting_persisted() {
                    drop(pending);
                    if let Some(a) = ack.take() {
                        a.ack();
                    }
                    return;
                }
                if pending.len() + self.in_flight_acks() < self.config.max_pending_acks {
                    if let Some(a) = ack.take() {
                        pending.push((job_id.to_string(), a));
                    }
                    return;
                }
            }
            if self
                .shutting_down
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return; // dropped un-acked: redelivered after restart
            }
            {
                let mut warned_at = self.crawl.ack_cap_warned_at.lock();
                if warned_at.is_none_or_elapsed(Duration::from_secs(60)) {
                    warn!(
                        held = self.config.max_pending_acks,
                        "Event consumer blocked: held acks at the cap, waiting for a \
                         successful accounting flush (job store unavailable?)"
                    );
                    *warned_at = Some(std::time::Instant::now());
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Start a job-store flush: take the held acks first, then the dirty jobs
    /// and their snapshots and the owed terminal writes, so every taken
    /// ack's event is covered (an event is applied, and its job marked dirty
    /// or terminal, before its ack is held).
    fn begin_flush(&self) -> FlushBatch {
        let acks = {
            let mut pending = self.crawl.pending_acks.lock();
            let acks = std::mem::take(&mut *pending);
            // Still held until finish_flush: keep them counted.
            self.crawl
                .in_flight_acks
                .store(acks.len(), std::sync::atomic::Ordering::SeqCst);
            acks
        };
        let dirty_ids: Vec<String> = self.crawl.dirty_jobs.write().drain().collect();
        // Terminal jobs are left out of the counter flush: a terminal row is
        // written only by the owed `update_job_full`, after the job's Lab
        // events were recorded (a job can still be dirty when it turns
        // terminal, or be re-marked dirty by an event racing a cancel).
        let snapshots: Vec<JobState> = {
            let jobs = self.crawl.jobs.read();
            dirty_ids
                .iter()
                .filter_map(|id| jobs.get(id).cloned())
                .filter(|j| !is_terminal(&j.status))
                .collect()
        };
        let accounting = if self.accounting_persisted() {
            self.accounting_snapshots(&dirty_ids)
        } else {
            Vec::new()
        };
        let terminal: Vec<JobState> = self
            .crawl
            .terminal_pending
            .write()
            .drain()
            .map(|(_, j)| j)
            .collect();
        FlushBatch {
            acks,
            dirty_ids,
            snapshots,
            accounting,
            terminal,
        }
    }

    /// Finish a flush.
    ///
    /// - Accounting durable: ack the held events, except those of jobs whose
    ///   owed terminal write failed (kept held, write retried next flush).
    /// - Transient accounting failure: keep everything held, the jobs dirty
    ///   and the terminal writes owed, for the next attempt.
    /// - Missing `accounting` column or `jobs` table (the engine's own
    ///   migrations did not apply, or the database was altered by hand):
    ///   non-retryable. Accounting persistence and ack deferral are turned
    ///   off for this process and every held ack is released, degrading to
    ///   in-memory accounting instead of blocking consumption forever.
    ///
    /// (Acks may complete in any order: the offset tracker only commits the
    /// contiguous acked prefix of each partition.)
    fn finish_flush(
        &self,
        batch: FlushBatch,
        accounting: AccountingFlush,
        failed_terminal: &HashSet<String>,
    ) {
        let FlushBatch {
            acks,
            dirty_ids,
            terminal,
            ..
        } = batch;
        let mut owed = self.crawl.terminal_pending.write();
        match accounting {
            AccountingFlush::SchemaMissing => {
                // In this degraded mode nothing is persisted, so jobs still
                // running at a restart end as FailStalled after the stall timeout.
                error!(
                    "jobs.accounting column is missing (the engine's own migrations did not \
                     apply): job accounting is kept in memory only and events are acked \
                     immediately for this process"
                );
                self.accounting_persisted
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                // Terminal writes whose Lab events are still unrecorded stay
                // owed (the flush is their only writer); the others are
                // dropped as before (their direct write already ran).
                {
                    let pending = self.crawl.pending_lab_events.lock();
                    owed.retain(|id, _| pending.contains_key(id));
                    for j in terminal {
                        if pending.contains_key(&j.job_id) {
                            owed.entry(j.job_id.clone()).or_insert(j);
                        }
                    }
                }
                drop(owed);
                let held = {
                    let mut pending = self.crawl.pending_acks.lock();
                    self.crawl
                        .in_flight_acks
                        .store(0, std::sync::atomic::Ordering::SeqCst);
                    std::mem::take(&mut *pending)
                };
                for (_, ack) in acks.into_iter().chain(held) {
                    ack.ack();
                }
            }
            AccountingFlush::Retry => {
                for j in terminal {
                    owed.entry(j.job_id.clone()).or_insert(j);
                }
                drop(owed);
                self.crawl.dirty_jobs.write().extend(dirty_ids);
                self.return_in_flight(acks);
            }
            AccountingFlush::Ok => {
                for j in terminal {
                    if failed_terminal.contains(&j.job_id) {
                        owed.entry(j.job_id.clone()).or_insert(j);
                    }
                }
                drop(owed);
                let mut keep = Vec::new();
                for (job_id, ack) in acks {
                    if failed_terminal.contains(&job_id) {
                        keep.push((job_id, ack));
                    } else {
                        ack.ack();
                    }
                }
                self.return_in_flight(keep);
            }
        }
    }

    /// End of a flush: put the batch acks still held back into
    /// `pending_acks` and stop counting the batch as in flight, atomically
    /// under the `pending_acks` lock (the held total never grows here).
    fn return_in_flight(&self, keep: Vec<(String, Ack)>) {
        let mut pending = self.crawl.pending_acks.lock();
        pending.extend(keep);
        self.crawl
            .in_flight_acks
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn in_flight_acks(&self) -> usize {
        self.crawl
            .in_flight_acks
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Everything currently held: waiting, in flight, or retained.
    #[cfg(test)]
    fn held_acks(&self) -> usize {
        let pending = self.crawl.pending_acks.lock();
        pending.len() + self.in_flight_acks()
    }

    /// Flush dirty job counters, accounting and owed terminal writes to the
    /// job store, then release the acks of the events they cover.
    async fn flush_to_db(&self, store: &dyn job_store::JobStore) {
        let batch = self.begin_flush();
        if !batch.snapshots.is_empty() {
            if let Err(e) = store.flush_job_counters(&batch.snapshots).await {
                warn!(error = %e, "Failed to flush job counters");
            }
        }
        let accounting = match store.flush_job_accounting(&batch.accounting).await {
            Ok(()) => AccountingFlush::Ok,
            Err(e) => classify_flush_error(&e),
        };
        let mut failed_terminal = HashSet::new();
        if accounting == AccountingFlush::Ok {
            for job in &batch.terminal {
                // The job's Lab events (charge, lifecycle email) must be in
                // the outbox before its terminal status is persisted. A
                // failed record puts them back and keeps the write owed
                // (`failed_terminal`): retried, events first, next flush.
                if !self.record_owed_lab_events(&job.job_id).await
                    || store.update_job_full(job).await.is_err()
                {
                    failed_terminal.insert(job.job_id.clone());
                }
            }
        }
        self.finish_flush(batch, accounting, &failed_terminal);
    }

    /// Process an event (not from the events topic) and update job state.
    fn process_event(&self, job_id: &str, event: &CrawlEvent) -> EventOutcome {
        self.process_event_at(job_id, event, None)
    }

    /// Process an event and update job state accordingly. `pos` is the
    /// event's position in the (durable) events topic, used to skip
    /// accounting events already folded into a restored snapshot (R-19).
    fn process_event_at(
        &self,
        job_id: &str,
        event: &CrawlEvent,
        pos: Option<EventPosition>,
    ) -> EventOutcome {
        self.process_event_charging(job_id, event, pos, None)
    }

    /// `process_event_at`, charging a `JobFailed` that transitions the job
    /// for `failed_charge` (a stalled job pays for the pages it crawled):
    /// the charge is owed before the terminal write, like every other.
    fn process_event_charging(
        &self,
        job_id: &str,
        event: &CrawlEvent,
        pos: Option<EventPosition>,
        failed_charge: Option<PageCharge>,
    ) -> EventOutcome {
        // Terminal transitions are applied exactly once, atomically: a second
        // JobCompleted/JobFailed for a job that is already terminal (a
        // redelivered event, or a finalize racing a cancel) must not re-bill,
        // re-email or re-record it (R5: exactly one completion email).
        let terminal_snapshot = match self.transition_terminal(job_id, event) {
            TerminalTransition::AlreadyTerminal => {
                debug!(job_id = %job_id, "Ignoring terminal event for an already terminal job");
                return EventOutcome::default();
            }
            TerminalTransition::Applied(snapshot) => Some(*snapshot),
            TerminalTransition::NotTerminal | TerminalTransition::UnknownJob => None,
        };
        // Captured now (before `terminal_snapshot` is moved into
        // `on_terminal` below) so webhook delivery still sees this job's
        // subscriptions even after its accounting/activity tracking is
        // freed (SCR-72).
        let webhook_hooks_from_terminal = terminal_snapshot.as_ref().map(|j| j.webhooks.clone());

        // A pipeline event for a job the API has stopped (or paused a while
        // ago) means the pipeline missed that control: re-publish it (R-22).
        self.heal_control(job_id, event, std::time::Instant::now());

        let live = self
            .crawl
            .jobs
            .read()
            .get(job_id)
            .is_some_and(|j| !is_terminal(&j.status));
        // Late events for a job that was already terminal before this event
        // no longer change its counters: they stay what was billed.
        let frozen = terminal_snapshot.is_none() && !live;

        // Track last activity (stall detection) for live jobs only, so late
        // events for a finished/unknown job do not leak entries.
        {
            let now = std::time::Instant::now();
            if live {
                self.crawl
                    .job_last_activity
                    .write()
                    .insert(job_id.to_string(), now);
            }

            // Track which services are alive based on event type
            let service = match event {
                CrawlEvent::PageCrawled { .. }
                | CrawlEvent::PageFailed { .. }
                | CrawlEvent::PageRetried { .. } => Some("crawler"),
                CrawlEvent::DocumentIndexed { .. } => Some("content"),
                CrawlEvent::UrlsDiscovered { .. } => Some("frontier"),
                _ => None,
            };
            if let Some(svc) = service {
                self.diagnostics
                    .service_last_seen
                    .write()
                    .insert(svc.to_string(), now);
            }
        }

        // Persist lifecycle events to ClickHouse job_events (JobStarted/Completed/Failed only)
        if let Some(ref batcher) = self.analytics.job_event_batcher {
            if let Some(job_event) = crawl_event_to_job_event(job_id, event) {
                let batcher = batcher.clone();
                tokio::spawn(async move {
                    if let Err(e) = batcher.add(job_event).await {
                        debug!(error = %e, "Failed to add job event to ClickHouse batcher");
                    }
                });
            }
        }

        // Persist intermediate page events to ClickHouse page_events (7-day TTL)
        if let Some(ref batcher) = self.analytics.page_event_batcher {
            if let Some(page_event) = crawl_event_to_page_event(job_id, event) {
                let batcher = batcher.clone();
                tokio::spawn(async move {
                    if let Err(e) = batcher.add(page_event).await {
                        debug!(error = %e, "Failed to add page event to ClickHouse batcher");
                    }
                });
            }
        }

        // Fold the event into the job's exact work accounting (R5). Only jobs
        // with an entry (created with the job / recovered at startup, freed
        // once terminal) are tracked, so late events for a finished job do
        // not resurrect state. The JobState counters below mirror the
        // (deduplicated) accounting when it exists.
        //
        // An accounting event at or below the job's persisted high-water mark
        // was already folded into the restored snapshot and is skipped
        // (redelivery after a restart, R-19).
        //
        // Computed here (before the ClickHouse request-event block below) so
        // that block can report what was actually delivered (js_rendered /
        // AI flags from `pages_browser`/`pages_ai`) instead of the job's
        // static config (D4/R4).
        let (accounted, accounting_touched) = {
            let mut accs = self.crawl.accounting.write();
            match accs.get_mut(job_id) {
                None => (None, false),
                Some(acc) => {
                    let mut touched = false;
                    if is_accounting_event(event) {
                        if pos.is_some_and(|p| acc.already_applied(p)) {
                            debug!(job_id = %job_id, ?pos, "Skipping already-accounted event");
                        } else {
                            acc.apply(event);
                            if pos.is_some() {
                                acc.event_hwm = pos;
                            }
                            touched = true;
                        }
                    }
                    (Some(AccountedCounters::from(&*acc)), touched)
                }
            }
        };
        if accounting_touched {
            self.crawl.dirty_jobs.write().insert(job_id.to_string());
        }

        // Terminal events of engine-run jobs (batch scrape, extract) get no
        // crawl request event nor job email: their pages are billed and
        // logged one by one as they run.
        let pipeline_terminal = !matches!(
            event,
            CrawlEvent::JobCompleted { .. } | CrawlEvent::JobFailed { .. }
        ) || self.is_pipeline_job(job_id);

        // Persist crawl completion to request_events (1 row per crawl job at completion)
        if let (Some(batcher), true) = (&self.analytics.request_batcher, pipeline_terminal) {
            if let CrawlEvent::JobCompleted {
                account_id,
                pages_crawled,
                bytes_downloaded,
                duration_secs,
                errors,
                ..
            } = event
            {
                // Look up the job to get the start URL and api_key_id. Note:
                // js_rendered/AI flags below come from delivered-page
                // accounting, not from this config lookup (D4/R4).
                let (url, domain, job_api_key_id) = {
                    let jobs = self.crawl.jobs.read();
                    jobs.get(job_id)
                        .map(|j| {
                            let url = j.start_urls.first().cloned().unwrap_or_default();
                            let domain = extract_domain(&url).unwrap_or_default();
                            let api_key_id = j.api_key_id.clone().unwrap_or_default();
                            (url, domain, api_key_id)
                        })
                        .unwrap_or_default()
                };

                // D4/R4: bill and report what was actually delivered, not
                // what the job's config merely enabled. Falls back to
                // treating all pages as plain HTTP/no-AI if the accounting
                // entry was already freed (shouldn't happen: `accounted` is
                // captured above, before `on_terminal` runs).
                let (pages_browser, pages_ai, pages_ocr) = accounted
                    .as_ref()
                    .map(|c| (c.pages_browser, c.pages_ai, c.pages_ocr))
                    .unwrap_or((0, 0, 0));
                let is_js_rendered = pages_browser > 0;
                let has_ai = pages_ai > 0;

                let ch_event = ClickHouseRequestEvent {
                    account_id: account_id.clone().unwrap_or_default(),
                    api_key_id: job_api_key_id,
                    job_id: job_id.to_string(),
                    operation: "crawl".to_string(),
                    url,
                    domain,
                    status_code: if *errors > 0 { 0 } else { 200 },
                    duration_ms: (*duration_secs * 1000) as u32,
                    content_length: *bytes_downloaded,
                    error: String::new(),
                    js_rendered: is_js_rendered,
                    ai_summary: has_ai,
                    ai_extraction: has_ai,
                    ai_prompt_tokens: 0,
                    ai_completion_tokens: 0,
                    ai_model: String::new(),
                    urls_found: 0,
                    pages_fetched: *pages_crawled as u32,
                    search_query: String::new(),
                    results_count: 0,
                    ocr_pages: u32::try_from(pages_ocr).unwrap_or(u32::MAX),
                    timestamp: time::OffsetDateTime::now_utc(),
                };
                let batcher = batcher.clone();
                tokio::spawn(async move {
                    if let Err(e) = batcher.add(ch_event).await {
                        debug!(error = %e, "Failed to add crawl request event to ClickHouse");
                    }
                });
            }
        }

        match event {
            CrawlEvent::PageCrawled {
                url, duration_ms, ..
            } => {
                if !frozen {
                    self.update_job(job_id, |j| {
                        match accounted {
                            Some(c) => {
                                j.pages_crawled = c.pages_crawled_ok;
                                j.bytes_downloaded = c.bytes_downloaded;
                            }
                            None => j.pages_crawled += 1,
                        }
                        // Update crawl rate based on elapsed time
                        if let Some(started) = j.started_at {
                            let elapsed = chrono::Utc::now()
                                .signed_duration_since(started)
                                .num_seconds();
                            if elapsed > 0 {
                                j.crawl_rate = j.pages_crawled as f64 / elapsed as f64;
                            }
                        }
                    });
                    self.crawl.dirty_jobs.write().insert(job_id.to_string());
                }

                // Track domain stats
                if let Some(domain) = extract_domain(url) {
                    self.diagnostics.record_success(
                        self.diagnostics_account(job_id, event),
                        domain,
                        *duration_ms,
                    );
                }
            }
            CrawlEvent::PageFailed {
                url,
                error,
                retry_count,
                status,
                ..
            } => {
                if !frozen {
                    self.update_job(job_id, |j| match accounted {
                        Some(c) => j.errors = c.pages_failed,
                        None => j.errors += 1,
                    });
                    self.crawl.dirty_jobs.write().insert(job_id.to_string());
                }

                // Track error
                let domain = extract_domain(url).unwrap_or_else(|| "unknown".to_string());
                self.diagnostics.record_failure(diagnostics::ErrorRecord {
                    url: url.clone(),
                    domain,
                    error: error.clone(),
                    status_code: status.or_else(|| extract_status_code(error)),
                    job_id: job_id.to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    retry_count: *retry_count,
                    account_id: self.diagnostics_account(job_id, event),
                });
            }
            CrawlEvent::DocumentIndexed { .. } if !frozen => {
                self.update_job(job_id, |j| {
                    match accounted {
                        Some(c) => j.pages_indexed = c.documents_indexed,
                        None => j.pages_indexed += 1,
                    }
                    j.documents_sent += 1;
                });
                self.crawl.dirty_jobs.write().insert(job_id.to_string());
            }
            CrawlEvent::JobCompleted {
                account_id,
                pages_crawled,
                documents_indexed,
                duration_secs,
                ..
            } => {
                let index_uid = terminal_snapshot
                    .as_ref()
                    .map(|j| j.index_uid.clone())
                    .unwrap_or_default();

                // Queue the job completion event (the Lab emails the
                // account). The only place a completion email is requested
                // (R5). Owed before the terminal write (`on_terminal` below).
                self.request_job_event(
                    job_id,
                    lab_events::LabEvent::job_completed,
                    account_id.clone().filter(|_| pipeline_terminal),
                    serde_json::json!({
                        "job_id": job_id,
                        "index_uid": index_uid,
                        "pages_crawled": pages_crawled,
                        "documents_indexed": documents_indexed,
                        "duration_secs": duration_secs,
                    }),
                );

                // Charge the crawled pages. D4/R4: bill for what was
                // actually delivered — `accounted` (folded above, before
                // `on_terminal` frees the accounting entry) carries the
                // browser/AI-enriched page counts.
                let (pages_browser, pages_ai, pages_ocr) = accounted
                    .as_ref()
                    .map(|c| (c.pages_browser, c.pages_ai, c.pages_ocr))
                    .unwrap_or((0, 0, 0));
                let pages_http = pages_crawled.saturating_sub(pages_browser);
                self.bill_job(
                    job_id,
                    account_id.as_ref(),
                    pages_http,
                    pages_browser,
                    pages_ai,
                    pages_ocr,
                );
                // Only now, with the charge and the email owed: persist.
                self.on_terminal(job_id, terminal_snapshot);
            }
            CrawlEvent::JobFailed { error, .. } => {
                // No temp index cleanup needed — Replace strategy writes directly to the real index.
                // Stale documents from a failed job will be cleaned up by the next successful crawl.

                let (pages_crawled, account_id) = terminal_snapshot
                    .as_ref()
                    .map(|j| (j.pages_crawled, j.account_id.clone()))
                    .unwrap_or((0, None));

                // A stalled job's charge (`finalize_job`), only when this
                // event is the one that made the job terminal.
                if let (Some(c), Some(_)) = (failed_charge, terminal_snapshot.as_ref()) {
                    self.bill_job(
                        job_id,
                        account_id.as_ref(),
                        c.pages_http,
                        c.pages_browser,
                        c.pages_ai,
                        c.pages_ocr,
                    );
                }

                // Queue the job failure event (the Lab emails the account).
                // The only place a failure email is requested (R5).
                self.request_job_event(
                    job_id,
                    lab_events::LabEvent::job_failed,
                    account_id.filter(|_| pipeline_terminal),
                    serde_json::json!({
                        "job_id": job_id,
                        "error_message": error,
                        "pages_crawled": pages_crawled,
                    }),
                );
                // Only now, with the charge and the email owed: persist.
                self.on_terminal(job_id, terminal_snapshot);
            }
            CrawlEvent::JobWarning { message, .. } => {
                if !message.is_empty() && !frozen {
                    self.update_job(job_id, |j| {
                        if j.warnings.len() < MAX_JOB_WARNINGS && !j.warnings.contains(message) {
                            j.warnings.push(message.clone());
                        }
                    });
                }
            }
            CrawlEvent::AiUsage { .. } => {
                if let Some(ref batcher) = self.analytics.ai_usage_batcher {
                    let fallback_account = self
                        .crawl
                        .jobs
                        .read()
                        .get(job_id)
                        .and_then(|j| j.account_id.clone());
                    if let Some(ch_event) = crawl_event_to_ai_usage(job_id, event, fallback_account)
                    {
                        let batcher = batcher.clone();
                        tokio::spawn(async move {
                            if let Err(e) = batcher.add(ch_event).await {
                                debug!(error = %e, "Failed to add AI usage event to ClickHouse");
                            }
                        });
                    }
                }
            }
            CrawlEvent::UrlsDiscovered { count, .. } if !frozen => {
                self.update_job(job_id, |j| {
                    // Track discovered URLs for progress estimation
                    if j.crawl_rate > 0.0 {
                        j.eta_seconds = Some((*count as f64 / j.crawl_rate) as u64);
                    }
                });
                self.crawl.dirty_jobs.write().insert(job_id.to_string());
            }
            _ => {}
        }

        // Deliver webhooks for this job's subscriptions (SCR-72). Captured
        // from the terminal snapshot when this event just finalized the job
        // (its accounting/activity tracking is freed by `on_terminal`
        // above, but the snapshot was taken before that), falling back to
        // the live in-memory job otherwise.
        let hooks = webhook_hooks_from_terminal
            .or_else(|| {
                self.crawl
                    .jobs
                    .read()
                    .get(job_id)
                    .map(|j| j.webhooks.clone())
            })
            .unwrap_or_default();
        self.webhook_dispatcher.enqueue(&hooks, job_id, event);

        EventOutcome {
            applied: true,
            accounting_touched,
        }
    }
}

/// Why a cancel / pause / resume request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ControlError {
    /// No such job (in memory).
    NotFound,
    /// The job's current status does not allow the transition (409).
    Conflict(JobStatus),
}

impl From<ControlError> for ApiError {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::NotFound => ApiError::new("Job not found", "not_found"),
            ControlError::Conflict(status) => ApiError::new(
                format!(
                    "Job is {}: transition not allowed",
                    format!("{status:?}").to_lowercase()
                ),
                "conflict",
            ),
        }
    }
}

/// The pages a terminal job is charged for (see `AppState::bill_job`).
#[derive(Debug, Clone, Copy)]
struct PageCharge {
    pages_http: u64,
    pages_browser: u64,
    pages_ai: u64,
    pages_ocr: u64,
}

/// Result of the atomic terminal check-and-set.
#[derive(Debug)]
enum TerminalTransition {
    /// Not a JobCompleted/JobFailed event.
    NotTerminal,
    /// Terminal event for a job not in memory (applied as before).
    UnknownJob,
    /// The job was already terminal: nothing applied.
    AlreadyTerminal,
    /// The job transitioned; its new state.
    Applied(Box<JobState>),
}

/// What `process_event` did with an event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EventOutcome {
    /// False only for a terminal event ignored because the job already was
    /// terminal.
    applied: bool,
    /// The event changed a job's work accounting (its ack must wait for the
    /// accounting flush, R-19).
    accounting_touched: bool,
}

/// One job-store flush round (see `AppState::begin_flush`).
struct FlushBatch {
    acks: Vec<(String, Ack)>,
    dirty_ids: Vec<String>,
    snapshots: Vec<JobState>,
    accounting: Vec<(String, serde_json::Value)>,
    /// Owed checked writes of terminal jobs
    terminal: Vec<JobState>,
}

/// Outcome of the accounting flush statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountingFlush {
    Ok,
    /// Transient failure: retry next flush, keep acks held.
    Retry,
    /// The `jobs.accounting` column / `jobs` table does not exist: the
    /// engine's own migrations did not apply. Not retryable.
    SchemaMissing,
}

fn classify_flush_error(e: &job_store::StoreError) -> AccountingFlush {
    match e {
        job_store::StoreError::SchemaMissing(_) => AccountingFlush::SchemaMissing,
        job_store::StoreError::Other(_) => AccountingFlush::Retry,
    }
}

/// `Option<Instant>` helper for rate-limited logging.
trait ElapsedSince {
    fn is_none_or_elapsed(&self, every: Duration) -> bool;
}

impl ElapsedSince for Option<std::time::Instant> {
    fn is_none_or_elapsed(&self, every: Duration) -> bool {
        self.is_none_or(|t| t.elapsed() >= every)
    }
}

/// Maximum distinct warnings kept per job.
const MAX_JOB_WARNINGS: usize = 100;
/// A paused job's in-flight pages may still report for this long before
/// its events mean the frontier missed the Pause (R-22).
const PAUSE_HEAL_GRACE: Duration = Duration::from_secs(5);
/// At most one self-heal `JobControl` re-publish per job per this window.
const CONTROL_REPUBLISH_EVERY: Duration = Duration::from_secs(10);
/// Bound on the per-job re-publish timestamps kept for rate limiting.
const MAX_CONTROL_REPUBLISH_ENTRIES: usize = 10_000;

fn is_terminal(status: &JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
    )
}

/// Events that change a job's work accounting (and so mark it dirty for the
/// job-store flush).
fn is_accounting_event(event: &CrawlEvent) -> bool {
    matches!(
        event,
        CrawlEvent::PageCrawled { .. }
            | CrawlEvent::PageFailed { .. }
            | CrawlEvent::PageSkipped { .. }
            | CrawlEvent::PageRetried { .. }
            | CrawlEvent::DocumentIndexed { .. }
            | CrawlEvent::DocumentSkipped { .. }
            | CrawlEvent::DocumentFailed { .. }
            | CrawlEvent::SitemapPublished { .. }
            | CrawlEvent::FrontierProgress { .. }
    )
}

/// The deduplicated accounting counters mirrored onto `JobState`.
#[derive(Debug, Clone, Copy)]
struct AccountedCounters {
    pages_crawled_ok: u64,
    pages_failed: u64,
    documents_indexed: u64,
    bytes_downloaded: u64,
    /// Of `pages_crawled_ok`, how many were actually rendered with a
    /// browser (`PageCrawled.js_rendered`) — D4/R4: billing and reporting
    /// must reflect what was delivered, not what the job's config enabled.
    pages_browser: u64,
    /// Of the indexed documents, how many were actually AI-enriched
    /// (`DocumentIndexed.ai_enriched`) — see `pages_browser`.
    pages_ai: u64,
    /// Pages recognized by OCR across indexed documents
    /// (`DocumentIndexed.ocr_pages`), billed at the OCR page rate.
    pages_ocr: u64,
}

impl From<&JobAccounting> for AccountedCounters {
    fn from(acc: &JobAccounting) -> Self {
        Self {
            pages_crawled_ok: acc.pages_crawled_ok,
            pages_failed: acc.pages_failed,
            documents_indexed: acc.documents_indexed,
            bytes_downloaded: acc.bytes_downloaded,
            pages_browser: acc.pages_browser,
            pages_ai: acc.pages_ai,
            pages_ocr: acc.pages_ocr,
        }
    }
}

/// Convert a content-worker `AiUsage` event into a ClickHouse `ai_usage`
/// row (R9). `fallback_account` is the job's account, used when the event
/// carries none.
fn crawl_event_to_ai_usage(
    job_id: &str,
    event: &CrawlEvent,
    fallback_account: Option<String>,
) -> Option<ClickHouseAiUsageEvent> {
    let CrawlEvent::AiUsage {
        account_id,
        provider,
        model,
        prompt_tokens,
        completion_tokens,
        duration_ms,
        feature,
        url,
        timestamp,
        ..
    } = event
    else {
        return None;
    };
    Some(ClickHouseAiUsageEvent {
        provider: provider.clone(),
        model: model.clone(),
        operation: feature.clone(),
        prompt_tokens: *prompt_tokens,
        completion_tokens: *completion_tokens,
        total_tokens: prompt_tokens.saturating_add(*completion_tokens),
        duration_ms: u32::try_from(*duration_ms).unwrap_or(u32::MAX),
        job_id: job_id.to_string(),
        account_id: account_id.clone().or(fallback_account).unwrap_or_default(),
        url: url.clone(),
        timestamp: if *timestamp > 0 {
            offset_datetime_from_millis(*timestamp)
        } else {
            time::OffsetDateTime::now_utc()
        },
    })
}

/// Replace-strategy post-crawl cleanup, run only on true completion (R5):
/// delete stale documents from previous crawls and return the verified
/// Meilisearch document count. Non-Replace jobs return `documents_indexed`
/// unchanged. `Err` carries the job failure message.
async fn replace_cleanup(job: &JobState, documents_indexed: u64) -> Result<u64, String> {
    let Some(ref replace_url) = job.swap_meilisearch_url else {
        return Ok(documents_indexed);
    };
    let ms_key = job.swap_meilisearch_api_key.as_deref();
    let job_id = &job.job_id;
    let index_uid = &job.index_uid;

    let key_preview = ms_key
        .map(|k| {
            if k.len() > 8 {
                format!("{}...", &k[..8])
            } else {
                k.to_string()
            }
        })
        .unwrap_or_else(|| "(none)".to_string());
    info!(
        job_id = %job_id,
        index = %index_uid,
        meilisearch_url = %replace_url,
        api_key_prefix = %key_preview,
        "Deleting stale documents before completing Replace job"
    );

    // Check for failed indexing tasks before cleanup — these indicate that
    // fire-and-forget document submissions were rejected by Meilisearch.
    let failed_tasks = scrapix_storage::meilisearch::MeilisearchStorage::log_failed_tasks(
        replace_url,
        ms_key,
        index_uid,
    )
    .await;
    if failed_tasks > 0 {
        warn!(
            job_id = %job_id,
            index = %index_uid,
            failed_tasks,
            "Meilisearch had failed indexing tasks — index may be incomplete"
        );
    }

    if let Err(e) = scrapix_storage::meilisearch::MeilisearchStorage::delete_stale_documents(
        replace_url,
        ms_key,
        index_uid,
        job_id,
    )
    .await
    {
        error!(
            job_id = %job_id,
            error = %e,
            index = %index_uid,
            "Stale document cleanup failed, marking job as failed"
        );
        return Err(format!("Stale document cleanup failed: {}", e));
    }
    info!(
        job_id = %job_id,
        index = %index_uid,
        "Stale document cleanup completed successfully"
    );

    // Query the actual document count from Meilisearch rather than trusting
    // the accounted counter (documents submitted to the batch buffer, not
    // confirmed Meilisearch task results).
    Ok(
        match scrapix_storage::meilisearch::MeilisearchStorage::get_actual_document_count(
            replace_url,
            ms_key,
            index_uid,
        )
        .await
        {
            Some(actual_count) => {
                if actual_count != documents_indexed {
                    warn!(
                        job_id = %job_id,
                        index = %index_uid,
                        reported = documents_indexed,
                        actual = actual_count,
                        "Document count mismatch: reported count differs from actual Meilisearch count. Meilisearch indexing tasks may have failed silently."
                    );
                }
                actual_count
            }
            None => documents_indexed,
        },
    )
}

/// Extract domain from URL
fn extract_domain(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
}

/// Try to extract HTTP status code from error message
fn extract_status_code(error: &str) -> Option<u16> {
    use std::sync::OnceLock;

    // Compile regexes once and cache for the lifetime of the process
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        // Common patterns: "404 Not Found", "HTTP 500", "status: 503"
        [r"^(\d{3})\s", r"HTTP\s+(\d{3})", r"status[:\s]+(\d{3})"]
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect()
    });

    for re in patterns {
        if let Some(caps) = re.captures(error) {
            if let Some(m) = caps.get(1) {
                if let Ok(code) = m.as_str().parse::<u16>() {
                    return Some(code);
                }
            }
        }
    }
    None
}

/// Convert a millisecond epoch timestamp to OffsetDateTime.
fn offset_datetime_from_millis(millis: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(millis / 1000)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
}

/// Convert a CrawlEvent to a ClickHouse JobEvent. Only lifecycle events are persisted.
fn crawl_event_to_job_event(job_id: &str, event: &CrawlEvent) -> Option<ClickHouseJobEvent> {
    match event {
        CrawlEvent::JobStarted {
            index_uid,
            account_id,
            start_urls,
            timestamp,
            ..
        } => Some(ClickHouseJobEvent {
            event_type: "JobStarted".to_string(),
            job_id: job_id.to_string(),
            account_id: account_id.clone().unwrap_or_default(),
            index_uid: index_uid.clone(),
            start_urls: start_urls.clone(),
            operation: "crawl".to_string(),
            timestamp: offset_datetime_from_millis(*timestamp),
            ..Default::default()
        }),
        CrawlEvent::JobCompleted {
            account_id,
            pages_crawled,
            documents_indexed,
            errors,
            bytes_downloaded,
            duration_secs,
            timestamp,
            ..
        } => Some(ClickHouseJobEvent {
            event_type: "JobCompleted".to_string(),
            job_id: job_id.to_string(),
            account_id: account_id.clone().unwrap_or_default(),
            pages_crawled: *pages_crawled,
            documents_indexed: *documents_indexed,
            errors: *errors,
            bytes_downloaded: *bytes_downloaded,
            duration_secs: *duration_secs,
            timestamp: offset_datetime_from_millis(*timestamp),
            ..Default::default()
        }),
        CrawlEvent::JobFailed {
            account_id,
            error,
            timestamp,
            ..
        } => Some(ClickHouseJobEvent {
            event_type: "JobFailed".to_string(),
            job_id: job_id.to_string(),
            account_id: account_id.clone().unwrap_or_default(),
            error: error.clone(),
            timestamp: offset_datetime_from_millis(*timestamp),
            ..Default::default()
        }),
        _ => None, // Only lifecycle events go to job_events
    }
}

/// Convert an intermediate CrawlEvent to a ClickHousePageEvent for persistence.
/// Returns None for lifecycle events (JobStarted/Completed/Failed) which are stored in job_events.
fn crawl_event_to_page_event(job_id: &str, event: &CrawlEvent) -> Option<ClickHousePageEvent> {
    let now = time::OffsetDateTime::now_utc();
    let account_id = match event {
        CrawlEvent::PageCrawled { account_id, .. }
        | CrawlEvent::PageFailed { account_id, .. }
        | CrawlEvent::DocumentIndexed { account_id, .. } => account_id.clone().unwrap_or_default(),
        _ => String::new(),
    };

    match event {
        CrawlEvent::PageCrawled {
            url,
            status,
            content_length,
            duration_ms,
            ..
        } => Some(ClickHousePageEvent {
            job_id: job_id.to_string(),
            account_id,
            event_type: "page_crawled".to_string(),
            url: url.clone(),
            status_code: *status,
            content_length: *content_length,
            duration_ms: *duration_ms as u32,
            timestamp: now,
            ..Default::default()
        }),
        CrawlEvent::PageFailed {
            url,
            error,
            retry_count,
            ..
        } => Some(ClickHousePageEvent {
            job_id: job_id.to_string(),
            account_id,
            event_type: "page_failed".to_string(),
            url: url.clone(),
            error: error.clone(),
            retry_count: *retry_count as u8,
            timestamp: now,
            ..Default::default()
        }),
        CrawlEvent::DocumentIndexed {
            url, document_id, ..
        } => Some(ClickHousePageEvent {
            job_id: job_id.to_string(),
            account_id,
            event_type: "document_indexed".to_string(),
            url: url.clone(),
            document_id: document_id.clone(),
            timestamp: now,
            ..Default::default()
        }),
        CrawlEvent::UrlsDiscovered {
            source_url, count, ..
        } => Some(ClickHousePageEvent {
            job_id: job_id.to_string(),
            event_type: "urls_discovered".to_string(),
            source_url: source_url.clone(),
            urls_count: *count as u32,
            timestamp: now,
            ..Default::default()
        }),
        CrawlEvent::PageSkipped { url, reason, .. } => Some(ClickHousePageEvent {
            job_id: job_id.to_string(),
            event_type: "page_skipped".to_string(),
            url: url.clone(),
            reason: reason.clone(),
            timestamp: now,
            ..Default::default()
        }),
        CrawlEvent::RateLimited {
            domain, wait_ms, ..
        } => Some(ClickHousePageEvent {
            job_id: job_id.to_string(),
            event_type: "rate_limited".to_string(),
            domain: domain.clone(),
            wait_ms: *wait_ms,
            timestamp: now,
            ..Default::default()
        }),
        // Lifecycle events are handled by job_event_batcher — skip here
        _ => None,
    }
}

/// API error response
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ApiError {
    pub(crate) error: String,
    pub(crate) code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Box<serde_json::Value>>,
    /// `Retry-After` seconds (a header, never in the body).
    #[serde(skip)]
    retry_after: Option<u32>,
}

impl ApiError {
    pub(crate) fn new(error: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: code.into(),
            details: None,
            retry_after: None,
        }
    }

    /// Send `Retry-After: <secs>` with the response.
    pub(crate) fn with_retry_after(mut self, secs: u32) -> Self {
        self.retry_after = Some(secs);
        self
    }

    fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(Box::new(details));
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.code.as_str() {
            "not_found" => StatusCode::NOT_FOUND,
            "bad_request" | "validation_error" | "ocr_required" => StatusCode::BAD_REQUEST,
            "unsupported_document" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "parse_error" => StatusCode::UNPROCESSABLE_ENTITY,
            "file_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            "ocr_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            "unauthorized" => StatusCode::UNAUTHORIZED,
            "conflict" => StatusCode::CONFLICT,
            "insufficient_credits" => StatusCode::PAYMENT_REQUIRED,
            "action_error" => StatusCode::UNPROCESSABLE_ENTITY,
            "spend_limit_exceeded" => StatusCode::FORBIDDEN,
            "service_unavailable" | "render_js_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            "forbidden" => StatusCode::FORBIDDEN,
            "quota_exceeded" => StatusCode::TOO_MANY_REQUESTS,
            "fetch_error" => StatusCode::BAD_GATEWAY,
            "timeout" => StatusCode::GATEWAY_TIMEOUT,
            "analytics_unavailable" => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let retry_after = self.retry_after;
        let mut resp = (status, Json(self)).into_response();
        if let Some(secs) = retry_after {
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, secs.into());
        }
        resp
    }
}

// ============================================================================
// Account context helpers
// ============================================================================

use crate::auth::AuthenticatedAccount;

/// Resolved account context for an authenticated request (any credential)
pub(crate) struct AccountContext {
    pub account_id: String,
    pub api_key_id: Option<String>,
    pub tier: String,
    /// User role in this account (None for API keys and service calls — they are account-scoped).
    pub user_role: Option<String>,
    /// The plan's limits from the Lab (`None`: a Lab that sent none).
    pub limits: Option<scrapix_auth::Limits>,
}

/// Account context for the request: API key, OAuth, session or service call — the auth
/// middleware resolves them all into `AuthenticatedAccount`. `None` when auth is off (standalone).
async fn extract_account_context(
    account_ext: &Option<Extension<AuthenticatedAccount>>,
) -> Option<AccountContext> {
    account_ext.as_ref().map(|Extension(acct)| AccountContext {
        account_id: acct.account_id.clone(),
        api_key_id: acct.api_key_id.clone(),
        tier: acct.tier.clone(),
        user_role: acct.role.clone(),
        limits: acct.limits.clone(),
    })
}

/// Check that the user's role allows write operations (scrape, map, search, crawl).
/// API key auth and auth-disabled scenarios are always allowed.
/// Session users must be owner, admin, or member (viewers are denied).
fn check_write_permission(account_ctx: &Option<AccountContext>) -> Result<(), ApiError> {
    if let Some(ctx) = account_ctx {
        if let Some(ref role) = ctx.user_role {
            if role == "viewer" {
                return Err(ApiError::new(
                    "Insufficient permissions: viewers cannot perform this action",
                    "forbidden",
                ));
            }
        }
    }
    Ok(())
}

/// Check that the authenticated account owns the given job.
/// Returns Ok(()) when: auth is disabled, job has no account_id (legacy), or account matches.
/// Returns Err(not_found) when account_id doesn't match (don't leak job existence).
fn check_job_ownership(
    job: &JobState,
    account_ctx: &Option<AccountContext>,
) -> Result<(), ApiError> {
    if let Some(ctx) = account_ctx {
        if let Some(ref job_account) = job.account_id {
            if job_account != &ctx.account_id {
                return Err(ApiError::new("Job not found", "not_found"));
            }
        }
    }
    Ok(())
}

/// Create crawl response
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct CreateCrawlResponse {
    job_id: String,
    status: String,
    index_uid: String,
    start_urls_count: usize,
    message: String,
    /// Non-fatal warnings about config fields that were accepted but cannot
    /// be honored per-job (worker-level settings). Empty when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// Bulk crawl response
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct BulkCrawlResponse {
    /// Successfully created jobs
    jobs: Vec<CreateCrawlResponse>,
    /// Number of jobs that failed to create
    errors: Vec<BulkCrawlError>,
    /// Total jobs submitted
    total: usize,
}

/// Error for a single config in a bulk submission
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct BulkCrawlError {
    index: usize,
    error: String,
}

/// Job status response
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct JobStatusResponse {
    job_id: String,
    /// `crawl`, `batch_scrape` or `extract`
    job_type: job_kind::JobKind,
    /// One of `pending`, `running`, `paused`, `completed`, `failed`,
    /// `cancelled`
    #[schema(value_type = JobStatus)]
    status: String,
    index_uid: String,
    pages_crawled: u64,
    pages_indexed: u64,
    documents_sent: u64,
    errors: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    crawl_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    eta_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    start_urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_pages: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<serde_json::Value>,
    /// Job-level warnings raised by workers while running the job (e.g. a
    /// requested feature a worker cannot honor), deduplicated. Omitted when
    /// there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

impl From<JobState> for JobStatusResponse {
    fn from(job: JobState) -> Self {
        let duration_seconds = job.duration_seconds();
        let job_type = job_kind::JobKind::of(&job);
        // A finished job has nothing left to estimate.
        let eta_seconds = job.eta_seconds.filter(|_| !is_terminal(&job.status));
        Self {
            job_id: job.job_id,
            job_type,
            status: format!("{:?}", job.status).to_lowercase(),
            index_uid: job.index_uid,
            pages_crawled: job.pages_crawled,
            pages_indexed: job.pages_indexed,
            documents_sent: job.documents_sent,
            errors: job.errors,
            started_at: job.started_at.map(|t| t.to_rfc3339()),
            completed_at: job.completed_at.map(|t| t.to_rfc3339()),
            duration_seconds,
            error_message: job.error_message,
            crawl_rate: job.crawl_rate,
            eta_seconds,
            start_urls: job.start_urls,
            max_pages: job.max_pages,
            config: job.config,
            warnings: job.warnings,
        }
    }
}

/// List jobs query parameters
#[derive(Debug, Deserialize, utoipa::IntoParams)]
struct ListJobsQuery {
    /// Page size (default 50, at most 200)
    #[serde(default = "default_limit")]
    limit: usize,
    /// Jobs to skip (newest first)
    #[serde(default)]
    offset: usize,
    /// Only jobs with this status: `pending`, `running`, `paused`,
    /// `completed`, `failed` or `cancelled`
    #[serde(default)]
    status: Option<String>,
}

fn default_limit() -> usize {
    50
}

/// Largest `GET /jobs` page.
const MAX_LIST_JOBS_LIMIT: usize = 200;

/// `DELETE /job/{id}` query parameters
#[derive(Debug, Deserialize, utoipa::IntoParams)]
struct DeleteJobQuery {
    /// `true`: delete a finished job instead of cancelling a running one
    #[serde(default)]
    purge: bool,
}

/// Health check response
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct HealthResponse {
    status: String,
    version: String,
    kafka_connected: bool,
}

// ============================================================================
// Scrape Endpoint Types
// ============================================================================

/// Request body for /scrape endpoint
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct ScrapeRequest {
    /// URL to scrape
    url: String,

    /// Formats to return (default: all)
    #[serde(default)]
    formats: Vec<ScrapeFormat>,

    /// Whether to only return the main content (excludes nav, footer, etc.)
    #[serde(default = "default_true_bool")]
    only_main_content: bool,

    /// Include links found on the page
    #[serde(default)]
    include_links: bool,

    /// Render JavaScript before extracting content (requires Chrome/Chromium)
    #[serde(default)]
    render_js: bool,

    /// Timeout in milliseconds (default: 30000)
    #[serde(default = "default_timeout")]
    timeout_ms: u64,

    /// Custom headers to send with the request
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,

    /// CSS selectors to remove before extraction
    #[serde(default)]
    exclude_selectors: Vec<String>,

    /// CSS selectors to keep (only extract from these)
    #[serde(default)]
    include_selectors: Vec<String>,

    /// Custom CSS selector extraction: field name -> a CSS selector, a list
    /// of selectors (the first that matches wins) or a selector definition
    /// (`{"selector": "...", "mode": "list", ...}`)
    #[serde(default)]
    extract: HashMap<String, ScrapeSelector>,

    /// AI enrichment options
    #[serde(default)]
    ai: Option<AiOptions>,

    /// Screenshot options, used when `formats` includes `"screenshot"`
    #[serde(default)]
    screenshot: Option<ScreenshotRequestOptions>,

    /// Browser actions run after the page loads and before content (and
    /// any screenshot) is captured: wait, click, scroll, write, press,
    /// execute_javascript. Forces browser rendering. At most 50; all
    /// actions together must finish within 30s.
    #[serde(default)]
    actions: Vec<Action>,

    /// Emulate a phone (mobile viewport, touch, Android Chrome user agent)
    /// to get the mobile layout of responsive sites. Forces browser
    /// rendering.
    #[serde(default)]
    mobile: bool,

    /// Cookies sent with the request (both the HTTP and the browser path),
    /// scoped to the target site: a cookie's `domain` must be the target
    /// host or a parent domain of it. At most 50.
    #[serde(default)]
    cookies: Vec<RequestCookie>,

    /// Document parsing options, used when the URL serves a PDF or an
    /// office document (OCR of scanned pages, page limits).
    #[serde(default)]
    parsers: documents::ParserOptions,
}

impl Default for ScrapeRequest {
    /// The request the API would deserialize from `{"url": ""}`: every
    /// field at its serde default. Lets internal callers (batch scrape,
    /// extract) build requests with `..Default::default()`.
    fn default() -> Self {
        Self {
            url: String::new(),
            formats: Vec::new(),
            only_main_content: default_true_bool(),
            include_links: false,
            render_js: false,
            timeout_ms: default_timeout(),
            headers: HashMap::new(),
            exclude_selectors: Vec::new(),
            include_selectors: Vec::new(),
            extract: HashMap::new(),
            ai: None,
            screenshot: None,
            actions: Vec::new(),
            mobile: false,
            cookies: Vec::new(),
            parsers: documents::ParserOptions::default(),
        }
    }
}

/// One `/scrape` `extract` field, in any of the shapes crawl's
/// `custom_selectors` accepts plus a full selector definition.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub(crate) enum ScrapeSelector {
    /// A CSS selector: the trimmed text of its first match
    Selector(String),
    /// CSS selectors, tried in order: the trimmed text of the first match
    Selectors(Vec<String>),
    /// A selector definition (extraction mode, attribute, transforms, ...)
    Definition(SelectorDefinition),
}

impl ScrapeSelector {
    fn to_definition(&self) -> SelectorDefinition {
        let text = |selector| SelectorDefinition {
            selector,
            mode: scrapix_extractor::ExtractionMode::Text,
            attribute: None,
            default: None,
            transform: vec![scrapix_extractor::Transform::Trim],
            fields: HashMap::new(),
        };
        match self {
            Self::Selector(s) => text(scrapix_extractor::SelectorInput::Single {
                selector: s.clone(),
            }),
            Self::Selectors(v) => text(scrapix_extractor::SelectorInput::Multiple {
                selectors: v.clone(),
            }),
            Self::Definition(d) => d.clone(),
        }
    }
}

/// Screenshot options for /scrape
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
struct ScreenshotRequestOptions {
    /// Capture the whole scrollable page (default) instead of only the
    /// viewport. Very long pages are cropped to 16384 px.
    #[serde(default = "default_true_bool")]
    full_page: bool,
}

/// AI enrichment options for /scrape
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct AiOptions {
    /// Generate a TL;DR summary
    #[serde(default)]
    summary: bool,

    /// Extract structured data (prompt-based or schema-based)
    #[serde(default)]
    extract: Option<AiExtractOptions>,
}

impl AiOptions {
    fn wants_summary(&self) -> bool {
        self.summary
    }

    fn wants_extraction(&self) -> bool {
        self.extract.is_some()
    }
}

/// `ai` asks for AI work (a summary or an extraction) but no AI provider is
/// configured: refuse up front, before anything is fetched or billed.
fn require_ai_provider(state: &AppState, ai: Option<&AiOptions>) -> Result<(), ApiError> {
    let wanted = ai.is_some_and(|ai| ai.wants_summary() || ai.wants_extraction());
    if wanted && state.ai_service.is_none() {
        return Err(no_ai_provider("AI features (ai.summary, ai.extract)"));
    }
    Ok(())
}

/// The 503 for `what` when the server has no AI provider (same as
/// `POST /extract`).
pub(crate) fn no_ai_provider(what: &str) -> ApiError {
    ApiError::new(
        format!(
            "{what} require an AI provider: set AI_PROVIDER and the matching API key \
             (e.g. ANTHROPIC_API_KEY or OPENAI_API_KEY) on the server"
        ),
        "service_unavailable",
    )
}

/// AI extraction options
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct AiExtractOptions {
    /// Natural language prompt for extraction
    #[serde(default)]
    prompt: String,

    /// Schema with field definitions (alternative to prompt)
    #[serde(default)]
    schema: Option<Vec<AiFieldDef>>,
}

/// Field definition for AI schema-based extraction
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct AiFieldDef {
    name: String,
    description: String,
    #[serde(default = "default_string_type")]
    field_type: String,
    #[serde(default)]
    required: bool,
}

fn default_string_type() -> String {
    "string".to_string()
}

fn default_true_bool() -> bool {
    true
}

fn default_timeout() -> u64 {
    30000
}

/// Output formats for scrape
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScrapeFormat {
    Markdown,
    Html,
    RawHtml,
    Content,
    Links,
    Metadata,
    Screenshot,
    Schema,
    Blocks,
}

/// Response for /scrape endpoint
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ScrapeResponse {
    /// Whether the scrape was successful
    success: bool,

    /// The URL that was scraped (after redirects)
    url: String,

    /// Markdown content (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    markdown: Option<String>,

    /// Cleaned HTML content (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<String>,

    /// Raw HTML content (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_html: Option<String>,

    /// Extracted main content text (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,

    /// Page metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<ScrapeMetadata>,

    /// Links found on the page (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    links: Option<Vec<String>>,

    /// Detected language
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,

    /// JSON-LD and structured data (if format "schema" requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<ExtractedSchema>,

    /// Content blocks split by headings (if format "blocks" requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<Vec<ContentBlock>>,

    /// Custom selector extraction results
    #[serde(skip_serializing_if = "Option::is_none")]
    extract: Option<HashMap<String, serde_json::Value>>,

    /// AI enrichment results
    #[serde(skip_serializing_if = "Option::is_none")]
    ai: Option<AiResult>,

    /// Base64-encoded PNG screenshot (if format "screenshot" requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    screenshot: Option<String>,

    /// Results of the request's `actions` (present when actions were sent)
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<ScrapeActionsResult>,

    /// Warning message (e.g. "AI requires OPENAI_API_KEY")
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,

    /// Document details, when the URL served (or the upload is) a PDF or an
    /// office document: format, pages, and which pages still need OCR.
    #[serde(skip_serializing_if = "Option::is_none")]
    document: Option<documents::DocumentInfo>,

    /// What OCR did, when `parsers.ocr` requested it.
    #[serde(skip_serializing_if = "Option::is_none")]
    ocr: Option<documents::OcrInfo>,

    /// HTTP status code
    status_code: u16,

    /// Time taken to scrape in milliseconds
    scrape_duration_ms: u64,
}

/// Results of the /scrape `actions`
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ScrapeActionsResult {
    /// Values of the `execute_javascript` actions, in order (JSON
    /// round-tripped; `undefined` is `null`)
    javascript_returns: Vec<serde_json::Value>,
}

/// AI enrichment results
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct AiResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extract: Option<serde_json::Value>,
}

/// Metadata from scraped page
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ScrapeMetadata {
    title: Option<String>,
    description: Option<String>,
    author: Option<String>,
    keywords: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_date: Option<String>,
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    open_graph: std::collections::HashMap<String, String>,
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    twitter: std::collections::HashMap<String, String>,
}

impl From<ExtractedMetadata> for ScrapeMetadata {
    fn from(meta: ExtractedMetadata) -> Self {
        Self {
            title: meta.title,
            description: meta.description,
            author: meta.author,
            keywords: meta.keywords,
            canonical_url: meta.canonical_url,
            published_date: meta.published_date,
            open_graph: meta.open_graph,
            twitter: meta.twitter,
        }
    }
}

// ============================================================================
// Route Handlers
// ============================================================================

/// Health check endpoint
#[utoipa::path(get, path = "/health", tag = "health", responses((status = 200, body = HealthResponse)))]
async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    state.warn_auth_disabled(std::time::Instant::now());
    let kafka_connected = state.producer.is_healthy();
    let status = if kafka_connected { "ok" } else { "degraded" };

    Json(HealthResponse {
        status: status.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        kafka_connected,
    })
}

/// Prometheus metrics endpoint — unauthenticated, next to `/health`. Kept out
/// of the OpenAPI spec: it's a scrape target for `qdq-server/monitoring`, not
/// a product/product-management API surface, and its response isn't JSON.
async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    // `scrapix_api_jobs{status}` is computed from the in-memory job map at
    // scrape time rather than kept as a running counter, since job status
    // transitions (not just increments) and the gauge only needs to be
    // right at the moment something scrapes it.
    let mut counts: HashMap<&'static str, i64> = HashMap::new();
    {
        let jobs = state.crawl.jobs.read();
        for job in jobs.values() {
            let label = match job.status {
                JobStatus::Pending => "pending",
                JobStatus::Running => "running",
                JobStatus::Completed => "completed",
                JobStatus::Failed => "failed",
                JobStatus::Cancelled => "cancelled",
                JobStatus::Paused => "paused",
            };
            *counts.entry(label).or_insert(0) += 1;
        }
    }
    let gauge = scrapix_core::metrics::api_jobs();
    for status in [
        "pending",
        "running",
        "completed",
        "failed",
        "cancelled",
        "paused",
    ] {
        gauge
            .with_label_values(&[status])
            .set(*counts.get(status).unwrap_or(&0) as f64);
    }

    let body = scrapix_core::metrics::encode();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            scrapix_core::metrics::CONTENT_TYPE,
        )],
        body,
    )
        .into_response()
}

/// Service health status for each component
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ServiceStatus {
    name: String,
    status: String, // "up", "idle", "down"
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen_secs_ago: Option<u64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ServiceHealthResponse {
    services: Vec<ServiceStatus>,
    /// A browser is available to the API: `render_js`, `mobile`, `actions`
    /// and the `screenshot` format on `/scrape`, `/batch/scrape`, `/map` and
    /// `/extract` (they answer 503 `render_js_unavailable` otherwise).
    browser_available: bool,
    /// The crawlers can render pages (`crawler_type: "browser"` crawls);
    /// `null` when the API does not know (separately deployed crawlers,
    /// browser crawls accepted). When `false`, `POST /crawl` refuses them.
    crawl_browser_available: Option<bool>,
}

/// Service health endpoint — reports liveness of each component
#[utoipa::path(get, path = "/health/services", tag = "health", responses((status = 200, body = ServiceHealthResponse)))]
async fn health_services(State(state): State<Arc<AppState>>) -> Json<ServiceHealthResponse> {
    let now = std::time::Instant::now();
    let seen = state.diagnostics.service_last_seen.read();
    let kafka_connected = state.producer.is_healthy();

    let worker_status = |name: &str| -> ServiceStatus {
        if let Some(last) = seen.get(name) {
            let ago = now.duration_since(*last).as_secs();
            if ago < 60 {
                ServiceStatus {
                    name: name.to_string(),
                    status: "up".to_string(),
                    last_seen_secs_ago: Some(ago),
                }
            } else {
                ServiceStatus {
                    name: name.to_string(),
                    status: "idle".to_string(),
                    last_seen_secs_ago: Some(ago),
                }
            }
        } else {
            ServiceStatus {
                name: name.to_string(),
                status: "down".to_string(),
                last_seen_secs_ago: None,
            }
        }
    };

    let services = vec![
        ServiceStatus {
            name: "api".to_string(),
            status: "up".to_string(),
            last_seen_secs_ago: Some(0),
        },
        ServiceStatus {
            name: "kafka".to_string(),
            status: if kafka_connected { "up" } else { "down" }.to_string(),
            last_seen_secs_ago: None,
        },
        worker_status("crawler"),
        worker_status("content"),
        worker_status("frontier"),
    ];

    Json(ServiceHealthResponse {
        services,
        browser_available: state.browser_renderer.is_some(),
        crawl_browser_available: state.crawl_browser,
    })
}

/// `/scrape` accepts PDFs and office documents (up to
/// `DOCUMENT_MAX_SIZE_MB`) on top of HTML and markdown.
fn document_fetch_options() -> scrapix_crawler::FetchOptions {
    scrapix_crawler::FetchOptions::with_all_documents(Some(documents::max_document_bytes()))
}

/// Preprocess HTML by applying include/exclude CSS selectors.
/// - `include_selectors`: if non-empty, only keep HTML from matching elements
/// - `exclude_selectors`: if non-empty, remove matching elements from the HTML
fn preprocess_html(
    html: &str,
    include_selectors: &[String],
    exclude_selectors: &[String],
) -> String {
    use scraper::{Html, Selector};

    if include_selectors.is_empty() && exclude_selectors.is_empty() {
        return html.to_string();
    }

    let document = Html::parse_document(html);

    // Step 1: If include_selectors are specified, collect matching elements' HTML
    let working_html = if !include_selectors.is_empty() {
        let mut parts = Vec::new();
        for sel_str in include_selectors {
            match Selector::parse(sel_str) {
                Ok(selector) => {
                    for element in document.select(&selector) {
                        parts.push(element.html());
                    }
                }
                Err(e) => {
                    warn!(selector = %sel_str, error = ?e, "Invalid include CSS selector, skipping");
                }
            }
        }
        if parts.is_empty() {
            return html.to_string();
        }
        format!("<html><body>{}</body></html>", parts.join(""))
    } else {
        html.to_string()
    };

    // Step 2: If exclude_selectors are specified, remove matching elements
    if !exclude_selectors.is_empty() {
        let mut result = working_html;
        for sel_str in exclude_selectors {
            match Selector::parse(sel_str) {
                Ok(selector) => {
                    let doc = Html::parse_document(&result);
                    for element in doc.select(&selector) {
                        let outer = element.html();
                        result = result.replacen(&outer, "", 1);
                    }
                }
                Err(e) => {
                    warn!(selector = %sel_str, error = ?e, "Invalid exclude CSS selector, skipping");
                }
            }
        }
        result
    } else {
        working_html
    }
}

/// Scrape a single URL and return content immediately
/// This bypasses the job queue for instant results
#[utoipa::path(post, path = "/scrape", tag = "scrape", request_body = ScrapeRequest, responses((status = 200, body = ScrapeResponse), (status = 400, body = ApiError), (status = 422, description = "A browser action failed (`action_error`; `details` names the action)", body = ApiError)), security(("api_key" = [])))]
async fn scrape_url(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Json(request): Json<ScrapeRequest>,
) -> Result<Json<ScrapeResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    check_write_permission(&account_ctx)?;
    perform_scrape(&state, &account_ctx, &request)
        .await
        .map(Json)
}

impl AppState {
    /// Record one usage event for `ctx` (hosted only; no-op without a Lab).
    /// `credits` is the pre-v2 price (`legacy_credits`) sent next to the
    /// units during the contract v2 transition release.
    pub(crate) async fn record_usage(
        &self,
        ctx: &AccountContext,
        operation: &str,
        credits: i64,
        units: serde_json::Value,
        description: String,
        job_id: Option<&str>,
    ) {
        let event = lab_events::LabEvent::usage(
            &ctx.account_id,
            ctx.api_key_id.as_deref(),
            operation,
            credits,
            units,
            description,
            job_id,
        );
        self.record_events(&[event]).await;
    }

    /// Record several events in one outbox write (hosted only; no-op without
    /// a Lab). Failures are logged by `Lab::record`.
    pub(crate) async fn record_events(&self, events: &[lab_events::LabEvent]) {
        let Some(ref lab) = self.lab else { return };
        let _ = lab.record(events).await;
    }
}

/// Attach an in-memory Lab to `state` (tests); the returned outbox exposes
/// every recorded event.
#[cfg(test)]
pub(crate) fn with_memory_lab(state: &mut AppState) -> Arc<lab_events::MemoryOutbox> {
    let outbox = Arc::new(lab_events::MemoryOutbox::default());
    state.lab = Some(Arc::new(lab_events::Lab::new(outbox.clone())));
    outbox
}

/// Usage event for one successful scrape: one page, served by the browser
/// or over HTTP, plus the AI work that produced a result. `formats` are the
/// requested formats: their feature-format count is the page's
/// `feature_pages` and, with the delivered AI flags, sets the pre-v2
/// `credits`.
async fn record_scrape_usage(
    state: &AppState,
    ctx: &AccountContext,
    formats: &[ScrapeFormat],
    js_rendered: bool,
    ai_summary: bool,
    ai_extraction: bool,
    final_url: &str,
) {
    state
        .record_usage(
            ctx,
            "scrape",
            legacy_credits::scrape_credits(formats, ai_summary, ai_extraction),
            serde_json::json!({
                "pages_http": u8::from(!js_rendered),
                "pages_browser": u8::from(js_rendered),
                "ai_summary": u8::from(ai_summary),
                "ai_extraction": u8::from(ai_extraction),
                "feature_pages": legacy_credits::feature_format_count(formats),
            }),
            final_url.to_string(),
            None,
        )
        .await;
}

/// Usage event for one successful map.
async fn record_map_usage(state: &AppState, ctx: &AccountContext, url: &str, urls_found: usize) {
    state
        .record_usage(
            ctx,
            "map",
            legacy_credits::MAP_CREDITS,
            serde_json::json!({ "requests": 1, "urls_found": urls_found }),
            url.to_string(),
            None,
        )
        .await;
}

/// Usage event for one search; `result` is the Meilisearch response.
async fn record_search_usage(
    state: &AppState,
    ctx: &AccountContext,
    url: &str,
    q: &str,
    result: &serde_json::Value,
) {
    let results = result
        .get("hits")
        .and_then(|h| h.as_array())
        .map_or(0, |a| a.len());
    state
        .record_usage(
            ctx,
            "search",
            legacy_credits::SEARCH_CREDITS,
            serde_json::json!({ "requests": 1, "results": results }),
            format!("{url} q={q}"),
            None,
        )
        .await;
}

/// The full /scrape pipeline for one URL: balance and plan pre-check, fetch
/// (HTTP or browser), extraction, AI enrichment, analytics and usage report.
///
/// Shared by `POST /scrape` and the endpoints that scrape many URLs on the
/// caller's behalf (batch scrape, extract). Permission checks are the
/// caller's job; everything else, including per-URL billing, happens here.
pub(crate) async fn perform_scrape(
    state: &Arc<AppState>,
    account_ctx: &Option<AccountContext>,
    request: &ScrapeRequest,
) -> Result<ScrapeResponse, ApiError> {
    if let Some(ref ctx) = account_ctx {
        debug!(account_id = %ctx.account_id, "Scrape request from account");
    }

    require_ai_provider(state, request.ai.as_ref())?;

    // Pre-flight (hosted): a positive balance, and JS rendering only on a
    // plan that includes it.
    if let (Some(ref lab), Some(ref ctx)) = (&state.lab_api, &account_ctx) {
        billing::check_credits(lab, &ctx.account_id).await?;
        let wants_browser = request.render_js
            || request.formats.contains(&ScrapeFormat::Screenshot)
            || !request.actions.is_empty()
            || request.mobile;
        engine_jobs::enforce_limits(
            ctx,
            0,
            engine_jobs::PlanCheck {
                max_depth: None,
                js_rendering: wants_browser,
            },
        )?;
    }

    let start_time = std::time::Instant::now();

    // Validate URL
    let parsed_url = url::Url::parse(&request.url)
        .map_err(|e| ApiError::new(format!("Invalid URL: {}", e), "validation_error"))?;

    // Only allow http/https
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err(ApiError::new(
            "Only http and https URLs are supported",
            "validation_error",
        ));
    }

    // Block raw IP addresses to prevent SSRF
    if matches!(
        parsed_url.host(),
        Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_))
    ) {
        return Err(ApiError::new(
            "Raw IP addresses are not allowed, use a hostname instead",
            "validation_error",
        ));
    }

    scrapix_core::browser::validate_actions(&request.actions)
        .map_err(|e| ApiError::new(e, "validation_error"))?;
    scrapix_core::browser::validate_cookies(&request.cookies, &parsed_url)
        .map_err(|e| ApiError::new(e, "validation_error"))?;

    // Features that only the browser can provide force the browser path.
    let screenshot_opts = request
        .formats
        .contains(&ScrapeFormat::Screenshot)
        .then(|| ScreenshotOptions {
            full_page: request.screenshot.as_ref().is_none_or(|s| s.full_page),
        });
    let use_browser = request.render_js
        || screenshot_opts.is_some()
        || !request.actions.is_empty()
        || request.mobile;
    if use_browser && state.browser_renderer.is_none() {
        let reason = if request.render_js {
            "JS rendering"
        } else if screenshot_opts.is_some() {
            "The screenshot format"
        } else if !request.actions.is_empty() {
            "Page actions"
        } else {
            "Mobile emulation"
        };
        return Err(ApiError::new(
            format!(
                "{reason} requires a browser, which is not available on this server (Chrome/Chromium not found)"
            ),
            "render_js_unavailable",
        ));
    }

    info!(url = %request.url, render_js = use_browser, "Scraping URL");

    // (a) Fetch using browser renderer or HTTP fetcher
    let crawl_url = CrawlUrl::seed(&request.url);

    let mut screenshot_png: Option<Vec<u8>> = None;
    let mut actions_result: Option<ScrapeActionsResult> = None;
    let raw_page = if let Some(renderer) = state.browser_renderer.as_ref().filter(|_| use_browser) {
        let page_options = PageOptions {
            screenshot: screenshot_opts,
            actions: request.actions.clone(),
            // Caller-supplied actions and scripts must not be able to reach
            // internal addresses: every request the page makes is checked.
            guard_requests: true,
            mobile: request.mobile,
            cookies: request.cookies.clone(),
            // A fresh browser context per request: cookies (supplied or set
            // by the site) and storage never reach another request on the
            // shared browser.
            isolate: true,
            ..Default::default()
        };
        let mut rendered = renderer
            .render_page(&request.url, &page_options)
            .await
            .map_err(|e| match e {
                scrapix_core::ScrapixError::Action {
                    index,
                    action,
                    message,
                } => ApiError::new(
                    format!("actions[{index}] ({action}) failed: {message}"),
                    "action_error",
                )
                .with_details(serde_json::json!({
                    "action_index": index,
                    "action_type": action,
                    "message": message,
                })),
                other => ApiError::new(
                    format!("Failed to render URL with browser: {}", other),
                    "fetch_error",
                ),
            })?;
        screenshot_png = rendered.screenshot.take();
        if !request.actions.is_empty() {
            actions_result = Some(ScrapeActionsResult {
                javascript_returns: std::mem::take(&mut rendered.javascript_returns),
            });
        }
        CdpRenderer::raw_page(&crawl_url, rendered)
    } else if request.headers.is_empty() && request.cookies.is_empty() {
        // Use the shared fetcher (connection pooling, DNS cache, retries).
        // It has no cookie store, so nothing carries over between requests.
        state
            .fetcher
            .fetch_with_options(&crawl_url, document_fetch_options())
            .await
            .map_err(|e| ApiError::new(format!("Failed to fetch URL: {}", e), "fetch_error"))?
    } else {
        // Build a one-off fetcher with custom headers and/or cookies
        let robots_config = RobotsConfig {
            respect_robots: false,
            ..Default::default()
        };
        let robots_cache = Arc::new(RobotsCache::new(robots_config).map_err(|e| {
            ApiError::new(
                format!("Failed to create robots cache: {}", e),
                "internal_error",
            )
        })?);

        // Its cookie store (seeded with the request's cookies) lives only
        // for this request, so session cookies set along a redirect or login
        // flow are kept within the fetch and never shared.
        let mut builder = HttpFetcherBuilder::new()
            .timeout(Duration::from_millis(request.timeout_ms))
            .allow_private_ips(state.fetcher.allows_private_ips())
            .cookie_store(true);
        for cookie in &request.cookies {
            builder = builder.cookie(cookie.to_set_cookie_string(), parsed_url.clone());
        }

        // Add custom headers (block sensitive headers to prevent injection attacks)
        const BLOCKED_HEADERS: &[&str] = &[
            "host",
            "transfer-encoding",
            "content-length",
            "connection",
            "upgrade",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
        ];
        for (key, value) in &request.headers {
            let key_lower = key.to_lowercase();
            if BLOCKED_HEADERS.contains(&key_lower.as_str()) {
                warn!(header = %key, "Blocked sensitive header in scrape request");
                continue;
            }
            builder = builder.header(key, value);
        }

        let fetcher = builder.build(robots_cache).map_err(|e| {
            ApiError::new(
                format!("Failed to create HTTP client: {}", e),
                "internal_error",
            )
        })?;

        fetcher
            .fetch_with_options(&crawl_url, document_fetch_options())
            .await
            .map_err(|e| ApiError::new(format!("Failed to fetch URL: {}", e), "fetch_error"))?
    };
    let js_rendered = raw_page.js_rendered;

    let status_code = raw_page.status;
    let final_url = raw_page.final_url.clone();

    // Check for success status
    if !(200..300).contains(&status_code) {
        // Track failed scrape in ClickHouse request_events
        if let Some(ref batcher) = state.analytics.request_batcher {
            let account_id = account_ctx
                .as_ref()
                .map(|c| c.account_id.clone())
                .unwrap_or_default();
            let api_key_id = account_ctx
                .as_ref()
                .and_then(|c| c.api_key_id.clone())
                .unwrap_or_default();
            log_scrape_request(
                batcher,
                &final_url,
                status_code,
                start_time.elapsed().as_millis() as u64,
                0,
                account_id,
                api_key_id,
                format!("HTTP {}", status_code),
                js_rendered,
                false,
                false,
                0,
                0,
                String::new(),
            );
        }

        return Ok(ScrapeResponse {
            success: false,
            url: final_url,
            markdown: None,
            html: None,
            raw_html: None,
            content: None,
            metadata: None,
            links: None,
            language: None,
            schema: None,
            blocks: None,
            extract: None,
            ai: None,
            screenshot: None,
            actions: actions_result,
            warning: None,
            document: None,
            ocr: None,
            status_code,
            scrape_duration_ms: start_time.elapsed().as_millis() as u64,
        });
    }

    // A PDF or office document: parse it (and OCR it on request) instead of
    // running the HTML pipeline. The fetcher carried its bytes base64.
    if let Some(ct) = raw_page
        .content_type
        .as_deref()
        .filter(|ct| scrapix_core::content_types::is_binary_document(ct))
    {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        let bytes = BASE64
            .decode(raw_page.html.as_bytes())
            .map_err(|e| ApiError::new(format!("Invalid document body: {e}"), "fetch_error"))?;
        let response = documents::document_response(
            state,
            account_ctx,
            documents::DocumentJob {
                operation: "scrape",
                label: final_url.clone(),
                base_url: Some(final_url.clone()),
                fallback_title: scrapix_parser::pdf::title_from_url(&final_url),
                bytes,
                content_type: Some(ct.to_string()),
                formats: request.formats.clone(),
                include_links: request.include_links,
                parsers: request.parsers.clone(),
                ai: request.ai.as_ref(),
                status_code,
                js_rendered,
            },
            start_time,
        )
        .await?;
        return Ok(response);
    }

    let original_html = raw_page.html;
    let original_html_len = original_html.len() as u64;

    // (b) Preprocess HTML with include/exclude selectors
    let processed_html = preprocess_html(
        &original_html,
        &request.include_selectors,
        &request.exclude_selectors,
    );

    // Determine which formats to return (default: markdown + content + metadata)
    let formats = if request.formats.is_empty() {
        vec![
            ScrapeFormat::Markdown,
            ScrapeFormat::Content,
            ScrapeFormat::Metadata,
        ]
    } else {
        request.formats.clone()
    };

    // (c) Run extractor for metadata, schema, blocks, and custom selectors
    let needs_extraction = formats.contains(&ScrapeFormat::Metadata)
        || formats.contains(&ScrapeFormat::Schema)
        || formats.contains(&ScrapeFormat::Blocks)
        || !request.extract.is_empty();

    let extraction_result = if needs_extraction {
        let mut extractor = Extractor::new();

        if formats.contains(&ScrapeFormat::Metadata) {
            extractor = extractor.with_metadata();
        }
        if formats.contains(&ScrapeFormat::Schema) {
            extractor = extractor.with_schema();
        }
        if formats.contains(&ScrapeFormat::Blocks) {
            extractor = extractor.with_blocks();
        }
        if !request.extract.is_empty() {
            let sel_extractor = SelectorExtractor::with_definitions(
                request
                    .extract
                    .iter()
                    .map(|(field, sel)| (field.clone(), sel.to_definition()))
                    .collect(),
            );
            extractor = extractor.with_selectors(sel_extractor);
        }

        extractor.extract(&processed_html).ok()
    } else {
        None
    };

    // Pull out extraction results
    let metadata = extraction_result
        .as_ref()
        .and_then(|r| r.metadata.clone())
        .map(ScrapeMetadata::from);

    let schema = extraction_result.as_ref().and_then(|r| r.schema.clone());

    let blocks = extraction_result
        .as_ref()
        .and_then(|r| r.blocks.clone())
        .map(|b| b.blocks);

    let custom_extract = extraction_result
        .as_ref()
        .and_then(|r| r.custom.clone())
        .map(|c| c.values);

    // (d) Run parser functions on processed HTML
    let markdown = if formats.contains(&ScrapeFormat::Markdown) {
        if request.only_main_content {
            Some(html_to_main_content_markdown(&processed_html))
        } else {
            Some(html_to_markdown(&processed_html))
        }
    } else {
        None
    };

    let content = if formats.contains(&ScrapeFormat::Content) {
        if request.only_main_content {
            Some(extract_content(&processed_html))
        } else {
            Some(html_to_markdown(&processed_html))
        }
    } else {
        None
    };

    let html_output = if formats.contains(&ScrapeFormat::Html) {
        if request.only_main_content {
            Some(html_to_main_content_minihtml(&processed_html))
        } else {
            Some(html_to_minihtml(&processed_html))
        }
    } else {
        None
    };

    let return_raw_html = if formats.contains(&ScrapeFormat::RawHtml) {
        Some(original_html)
    } else {
        None
    };

    let links = if formats.contains(&ScrapeFormat::Links) || request.include_links {
        Some(extract_links_from_html(&processed_html, &final_url))
    } else {
        None
    };

    // Detect language from content
    let language = content
        .as_ref()
        .or(markdown.as_ref())
        .and_then(|text| detect_language_info(text))
        .map(|info| info.code);

    // (e) AI enrichment (optional)
    let ai_text = content.as_deref().or(markdown.as_deref()).unwrap_or("");
    let ai_run = run_ai_enrichment(state, request.ai.as_ref(), ai_text).await;
    let AiRun {
        result: ai_result,
        warning,
        prompt_tokens: total_prompt_tokens,
        completion_tokens: total_completion_tokens,
        model: ai_model_name,
    } = ai_run;

    let scrape_duration_ms = start_time.elapsed().as_millis() as u64;

    // Only the AI work that produced a result is reported and billed.
    let has_ai_summary = ai_result.as_ref().is_some_and(|r| r.summary.is_some());
    let has_ai_extraction = ai_result.as_ref().is_some_and(|r| r.extract.is_some());

    // Track successful scrape in ClickHouse request_events
    if let Some(ref batcher) = state.analytics.request_batcher {
        let account_id = account_ctx
            .as_ref()
            .map(|c| c.account_id.clone())
            .unwrap_or_default();
        let api_key_id = account_ctx
            .as_ref()
            .and_then(|c| c.api_key_id.clone())
            .unwrap_or_default();
        log_scrape_request(
            batcher,
            &final_url,
            status_code,
            scrape_duration_ms,
            original_html_len,
            account_id,
            api_key_id,
            String::new(),
            js_rendered,
            has_ai_summary,
            has_ai_extraction,
            total_prompt_tokens,
            total_completion_tokens,
            ai_model_name.clone(),
        );
    }

    // Report the charge for a successful scrape to the Lab.
    if let Some(ref ctx) = account_ctx {
        record_scrape_usage(
            state,
            ctx,
            &request.formats,
            js_rendered,
            has_ai_summary,
            has_ai_extraction,
            &final_url,
        )
        .await;
    }

    info!(
        url = %final_url,
        status_code,
        duration_ms = scrape_duration_ms,
        "Scrape completed"
    );

    Ok(ScrapeResponse {
        success: true,
        url: final_url,
        markdown,
        html: html_output,
        raw_html: return_raw_html,
        content,
        metadata,
        links,
        language,
        schema,
        blocks,
        extract: custom_extract,
        ai: ai_result,
        screenshot: screenshot_png.map(|png| BASE64.encode(png)),
        actions: actions_result,
        warning,
        document: None,
        ocr: None,
        status_code,
        scrape_duration_ms,
    })
}

/// Outcome of optional AI enrichment for `/scrape` and `/parse`.
#[derive(Default)]
struct AiRun {
    result: Option<AiResult>,
    warning: Option<String>,
    prompt_tokens: u32,
    completion_tokens: u32,
    model: String,
}

/// Run the requested AI summary/extraction on `ai_text`.
async fn run_ai_enrichment(state: &AppState, ai: Option<&AiOptions>, ai_text: &str) -> AiRun {
    let mut ai_result = None;
    let mut warning = None;
    let mut total_prompt_tokens: u32 = 0;
    let mut total_completion_tokens: u32 = 0;
    let mut ai_model_name = String::new();

    if let Some(ai_opts) = ai {
        if let Some(ref ai_service) = state.ai_service {
            if !ai_text.is_empty() {
                // Run AI operations concurrently
                let summary_fut = async {
                    if ai_opts.summary {
                        ai_service.summarize(ai_text).await.ok()
                    } else {
                        None
                    }
                };

                let extract_fut = async {
                    if let Some(ref extract_opts) = ai_opts.extract {
                        if let Some(ref schema_fields) = extract_opts.schema {
                            // Schema-based extraction
                            let mut builder = SchemaBuilder::new();
                            for field in schema_fields {
                                builder = builder.field(AiFieldDefinition {
                                    name: field.name.clone(),
                                    description: field.description.clone(),
                                    field_type: field.field_type.clone(),
                                    required: field.required,
                                    default: None,
                                    example: None,
                                });
                            }
                            let schema = builder.build();
                            ai_service.extract_schema(ai_text, &schema).await.ok()
                        } else if !extract_opts.prompt.is_empty() {
                            // Prompt-based extraction
                            ai_service.extract(ai_text, &extract_opts.prompt).await.ok()
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                let (ai_summary_result, ai_extract_result) = tokio::join!(summary_fut, extract_fut);

                // Accumulate AI token usage
                if let Some(ref summary) = ai_summary_result {
                    total_prompt_tokens += summary.prompt_tokens;
                    total_completion_tokens += summary.completion_tokens;
                    if ai_model_name.is_empty() {
                        ai_model_name = summary.model.clone();
                    }
                }
                if let Some(ref extraction) = ai_extract_result {
                    total_prompt_tokens += extraction.prompt_tokens;
                    total_completion_tokens += extraction.completion_tokens;
                    if ai_model_name.is_empty() {
                        ai_model_name = extraction.model.clone();
                    }
                }

                let ai_summary_text = ai_summary_result.map(|r| r.summary);
                let ai_extract_data = ai_extract_result.map(|r| r.data);

                if ai_summary_text.is_some() || ai_extract_data.is_some() {
                    ai_result = Some(AiResult {
                        summary: ai_summary_text,
                        extract: ai_extract_data,
                    });
                }
            }
        }
        // Requested but not produced (provider error, nothing to work on,
        // or no provider — which callers refuse up front): not billed.
        let mut failed = Vec::new();
        if ai_opts.wants_summary() && ai_result.as_ref().is_none_or(|r| r.summary.is_none()) {
            failed.push("summary");
        }
        if ai_opts.wants_extraction() && ai_result.as_ref().is_none_or(|r| r.extract.is_none()) {
            failed.push("extraction");
        }
        if !failed.is_empty() {
            warning = Some(format!(
                "AI {} could not be generated and was not billed",
                failed.join(" and ")
            ));
        }
    }

    AiRun {
        result: ai_result,
        warning,
        prompt_tokens: total_prompt_tokens,
        completion_tokens: total_completion_tokens,
        model: ai_model_name,
    }
}

/// Log a scrape request to ClickHouse request_events (fire-and-forget).
#[allow(clippy::too_many_arguments)]
fn log_scrape_request(
    batcher: &Arc<RequestEventBatcher>,
    url: &str,
    status_code: u16,
    duration_ms: u64,
    content_length: u64,
    account_id: String,
    api_key_id: String,
    error: String,
    js_rendered: bool,
    ai_summary: bool,
    ai_extraction: bool,
    ai_prompt_tokens: u32,
    ai_completion_tokens: u32,
    ai_model: String,
) {
    let domain = extract_domain(url).unwrap_or_default();
    let event = ClickHouseRequestEvent {
        account_id,
        api_key_id,
        job_id: String::new(),
        operation: "scrape".to_string(),
        url: url.to_string(),
        domain,
        status_code,
        duration_ms: duration_ms as u32,
        content_length,
        error,
        js_rendered,
        ai_summary,
        ai_extraction,
        ai_prompt_tokens,
        ai_completion_tokens,
        ai_model,
        urls_found: 0,
        pages_fetched: 1,
        search_query: String::new(),
        results_count: 0,
        ocr_pages: 0,
        timestamp: time::OffsetDateTime::now_utc(),
    };
    let batcher = batcher.clone();
    tokio::spawn(async move {
        if let Err(e) = batcher.add(event).await {
            debug!(error = %e, "Failed to add scrape request event to ClickHouse");
        }
    });
}

/// Extract links from HTML
fn extract_links_from_html(html: &str, base_url: &str) -> Vec<String> {
    use scraper::{Html, Selector};

    let Ok(base) = url::Url::parse(base_url) else {
        return vec![];
    };

    let document = Html::parse_document(html);
    let Ok(selector) = Selector::parse("a[href]") else {
        return vec![];
    };

    let mut urls = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for element in document.select(&selector) {
        if let Some(href) = element.value().attr("href") {
            // Skip javascript:, mailto:, tel:, etc.
            if href.starts_with("javascript:")
                || href.starts_with("mailto:")
                || href.starts_with("tel:")
                || href.starts_with("#")
            {
                continue;
            }

            // Resolve relative URLs
            if let Ok(resolved) = base.join(href) {
                let url_str = resolved.to_string();
                if seen.insert(url_str.clone()) {
                    urls.push(url_str);
                }
            }
        }
    }

    urls
}

/// Fire-and-forget TCP connect to each host in `WORKER_WAKE_HOSTS` (comma-separated
/// `host:port` pairs). On Fly.io, connecting to `scrapix-worker-crawler.internal:8081`
/// triggers the proxy to auto-start the machine from a suspended state. No-op when
/// the env var is unset (local dev, tests).
fn fan_out_worker_wakes() {
    let hosts = match std::env::var("WORKER_WAKE_HOSTS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };
    for host in hosts.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let host = host.to_string();
        tokio::spawn(async move {
            let connect = tokio::net::TcpStream::connect(&host);
            match tokio::time::timeout(Duration::from_millis(500), connect).await {
                Ok(Ok(_)) => {
                    tracing::debug!(host = %host, "Worker wake ping delivered");
                }
                Ok(Err(e)) => {
                    tracing::debug!(host = %host, error = %e, "Worker wake ping failed");
                }
                Err(_) => {
                    tracing::debug!(host = %host, "Worker wake ping timed out");
                }
            }
        });
    }
}

/// Validate a `CrawlConfig`, mapping `validator` errors to a 4xx `ApiError`.
///
/// Extracted from `do_create_crawl` so it can be unit-tested without the
/// surrounding `AppState` (producer, DB pool, etc.) that job creation needs.
pub(crate) fn validate_crawl_config(config: &mut CrawlConfig) -> Result<(), ApiError> {
    use validator::Validate;
    config
        .validate()
        .map_err(|errors| ApiError::new(errors.to_string(), "validation_error"))?;
    for (i, url) in config.start_urls.iter().enumerate() {
        check_http_url(url)
            .map_err(|msg| ApiError::new(format!("start_urls[{i}]: {msg}"), "validation_error"))?;
    }
    check_index_uid(&config.index_uid)
        .map_err(|msg| ApiError::new(format!("index_uid: {msg}"), "validation_error"))?;
    if let Some(ref proxy) = config.proxy {
        validate_proxy_config(proxy)
            .map_err(|msg| ApiError::new(format!("proxy: {msg}"), "validation_error"))?;
    }
    for hook in &mut config.webhooks {
        // Clamp (never reject) an out-of-range timeout: a hook author
        // asking for a 10-minute timeout is a config mistake, not
        // something worth a 400 for — but an unbounded timeout is a shared
        // resource risk (SCR-72 fix round 1), so it's silently brought into
        // range instead.
        hook.timeout_ms = webhooks::clamp_timeout_ms(hook.timeout_ms);
        webhooks::validate_webhook_config(hook)
            .map_err(|msg| ApiError::new(format!("webhooks: {msg}"), "validation_error"))?;
    }
    Ok(())
}

/// `url` is an absolute `http`/`https` URL with a host.
pub(crate) fn check_http_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL `{url}` ({e})"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("`{url}` is not an http or https URL"));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(format!("`{url}` has no host"));
    }
    Ok(())
}

/// A Meilisearch index uid: 1 to 511 ASCII letters, digits, `-` or `_`.
pub(crate) fn check_index_uid(uid: &str) -> Result<(), String> {
    let valid = (1..=511).contains(&uid.len())
        && uid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "`{uid}` is not a valid index uid (1 to 511 characters among a-z, A-Z, 0-9, - and _)"
        ))
    }
}

/// Redact secrets in a `CrawlConfig` before it's persisted (`jobs.config`)
/// or ever handed back over the API (`JobStatusResponse::config`): the
/// Meilisearch API key, any webhook auth secrets (SCR-72), proxy URL
/// credentials and custom header values. The real,
/// unredacted config is never stored — only kept transiently in this
/// request and, for webhooks specifically, in `JobState::webhooks` for
/// delivery.
fn redact_crawl_config_for_storage(config: &CrawlConfig) -> Option<serde_json::Value> {
    let mut v = serde_json::to_value(config).ok()?;
    if let Some(obj) = v.get_mut("meilisearch").and_then(|ms| ms.as_object_mut()) {
        obj.insert(
            "api_key".to_string(),
            serde_json::Value::String("***".to_string()),
        );
    }
    webhooks::redact_webhooks_json(&mut v);
    redact_proxy_and_headers_json(&mut v);
    Some(v)
}

/// Mask proxy URL credentials (userinfo) and every custom header value in
/// a serialized `CrawlConfig`. Header names stay visible; the values are
/// typically auth tokens. The crawler never reads this copy: its `JobSpec`
/// is built from the in-memory config.
fn redact_proxy_and_headers_json(v: &mut serde_json::Value) {
    fn redact_url(u: &mut serde_json::Value) {
        if let Some(raw) = u.as_str() {
            *u = serde_json::Value::String(scrapix_core::redact::redact_userinfo_str(raw));
        }
    }
    if let Some(proxy) = v.get_mut("proxy").and_then(|p| p.as_object_mut()) {
        if let Some(urls) = proxy.get_mut("urls").and_then(|u| u.as_array_mut()) {
            urls.iter_mut().for_each(redact_url);
        }
        if let Some(tiers) = proxy.get_mut("tiered").and_then(|t| t.as_array_mut()) {
            for tier in tiers.iter_mut().filter_map(|t| t.as_array_mut()) {
                tier.iter_mut().for_each(redact_url);
            }
        }
    }
    if let Some(headers) = v.get_mut("headers").and_then(|h| h.as_object_mut()) {
        for value in headers.values_mut() {
            *value = serde_json::Value::String("***".to_string());
        }
    }
}

/// Reject proxy configs the crawler cannot use safely: no proxy at all
/// (it would otherwise fall back to direct connections), URLs that are not
/// `http`/`https` (the only proxy schemes the crawler supports — `socks5`
/// is refused explicitly rather than failing every fetch), or a raw-IP host
/// that is not a public address (SSRF: e.g. `http://169.254.169.254`).
/// Hostnames are resolved and re-checked by the crawler at fetch time.
fn validate_proxy_config(proxy: &scrapix_core::ProxyConfig) -> Result<(), String> {
    let tiered = proxy.tiered.as_deref().unwrap_or_default();
    if proxy.urls.is_empty() && tiered.iter().all(|tier| tier.is_empty()) {
        return Err("at least one proxy URL is required in `urls` or `tiered`".to_string());
    }
    for entry in proxy.urls.iter().chain(tiered.iter().flatten()) {
        // Proxy URLs carry credentials: never echo them back.
        let shown = scrapix_core::redact::redact_userinfo_str(entry);
        let parsed =
            url::Url::parse(entry).map_err(|e| format!("invalid proxy URL '{shown}': {e}"))?;
        match parsed.scheme() {
            "http" | "https" => {}
            "socks5" | "socks5h" => {
                return Err(format!(
                    "socks5 proxies are not supported by the crawler ('{shown}'); use http or https"
                ))
            }
            other => {
                return Err(format!(
                    "unsupported proxy scheme '{other}' in '{shown}' (use http or https)"
                ))
            }
        }
        let ip = match parsed.host() {
            Some(url::Host::Ipv4(ip)) => Some(std::net::IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => Some(std::net::IpAddr::V6(ip)),
            Some(url::Host::Domain(_)) => None,
            None => return Err(format!("proxy URL '{shown}' has no host")),
        };
        if let Some(ip) = ip {
            if !scrapix_crawler::is_public_ip(ip) {
                return Err(format!(
                    "proxy URL '{shown}' points to a non-public address"
                ));
            }
        }
    }
    Ok(())
}

/// Warn about config fields that are accepted but cannot be honored per-job
/// (worker-level settings: `concurrency.browser_pool_size`,
/// `concurrency.dns_concurrency`) or have no effect as configured
/// (`features.pdf.extract_links` / `features.ocr` without the document
/// feature they act on). A warning is emitted only when the field differs
/// from its default.
pub(crate) fn crawl_config_warnings(config: &CrawlConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    let default_concurrency = ConcurrencyConfig::default();

    if config.concurrency.browser_pool_size != default_concurrency.browser_pool_size {
        warnings.push(format!(
            "concurrency.browser_pool_size ({}) is a worker-level setting (set at process \
             startup) and is ignored per-job",
            config.concurrency.browser_pool_size
        ));
    }

    if config.concurrency.dns_concurrency != default_concurrency.dns_concurrency {
        warnings.push(format!(
            "concurrency.dns_concurrency ({}) is a worker-level setting (set at process \
             startup) and is ignored per-job",
            config.concurrency.dns_concurrency
        ));
    }

    if config.crawler_type == CrawlerType::Browser && config.proxy.is_some() {
        warnings.push(
            "proxy is not supported with crawler_type \"browser\": the shared browser has a \
             single worker-level proxy, so browser-rendered pages of this job will fail \
             instead of connecting without the proxy"
                .to_string(),
        );
    }

    if let Some(pdf) = &config.features.pdf {
        if pdf.extract_links && !pdf.enabled {
            warnings.push(
                "features.pdf.extract_links has no effect unless features.pdf.enabled is true"
                    .to_string(),
            );
        }
    }

    if !config.features.ocr_mode().is_off() && !config.features.is_pdf_enabled() {
        warnings.push(
            "features.ocr only applies to PDFs: it has no effect unless features.pdf.enabled \
             is true"
                .to_string(),
        );
    }

    warnings
}

/// Refuse a crawl asking for something this server cannot do, instead of
/// accepting a job that fails (or bills for nothing) later.
pub(crate) fn check_crawl_capabilities(
    config: &CrawlConfig,
    ai_available: bool,
    browser_available: Option<bool>,
) -> Result<(), ApiError> {
    if config.crawler_type == CrawlerType::Browser && browser_available == Some(false) {
        return Err(ApiError::new(
            "crawler_type \"browser\" requires a browser, which is not available on this \
             server's crawlers (Chrome/Chromium with BROWSER_RENDER)",
            "render_js_unavailable",
        ));
    }
    let features = &config.features;
    if (features.ai_extraction_enabled() || features.ai_summary_enabled()) && !ai_available {
        return Err(no_ai_provider(
            "features.ai_extraction and features.ai_summary",
        ));
    }
    Ok(())
}

/// Core crawl creation logic, reusable from the crawl handlers
pub(crate) async fn do_create_crawl(
    state: &Arc<AppState>,
    config: CrawlConfig,
    account_ctx: Option<&AccountContext>,
) -> Result<CreateCrawlResponse, ApiError> {
    // Fan out wake pings to suspended worker apps before any validation — even if
    // validation fails this is cheap and benefits the next successful submit.
    fan_out_worker_wakes();

    // Validate config
    if config.start_urls.is_empty() {
        return Err(ApiError::new(
            "At least one start URL is required",
            "validation_error",
        ));
    }

    // Auto-generate index_uid from first start URL if not provided
    let config = if config.index_uid.is_empty() {
        let mut config = config;
        config.index_uid = scrapix_core::url_to_index_uid(
            config.start_urls.first().map(|s| s.as_str()).unwrap_or(""),
        );
        if config.index_uid.is_empty() {
            return Err(ApiError::new(
                "Could not derive index UID from start URLs",
                "validation_error",
            ));
        }
        config
    } else {
        config
    };

    // Resolve Meilisearch config from account's default engine if not provided
    let mut config = config;
    config.meilisearch = crate::meili::resolve_crawl_meilisearch(
        state.meili.as_ref(),
        account_ctx.map(|c| c.account_id.as_str()),
        std::mem::take(&mut config.meilisearch),
    )
    .await?;

    // Full validation (start_urls, index_uid length, and any future
    // #[validate] rules) — after index_uid auto-derivation and Meilisearch
    // engine resolution so both are populated before the length checks run.
    // `validate_crawl_config` clamps out-of-range webhook `timeout_ms`
    // values in place (SCR-72 fix round 1); `config` is already `mut`.
    validate_crawl_config(&mut config)?;

    // Features the server cannot run fail the request, not the job.
    check_crawl_capabilities(&config, state.ai_service.is_some(), state.crawl_browser)?;

    // Non-fatal warnings for accepted-but-unhonored (worker-level) fields.
    let warnings = crawl_config_warnings(&config);

    // Pre-flight (hosted): a positive balance and the plan's limits.
    engine_jobs::preflight(
        state,
        account_ctx,
        engine_jobs::PlanCheck {
            max_depth: config.max_depth,
            js_rendering: config.crawler_type == CrawlerType::Browser,
        },
    )
    .await?;
    // No depth given: cap it at the plan's limit before the job is stored
    // and published, since the frontier treats `None` as unbounded.
    engine_jobs::cap_unspecified_depth(&mut config.max_depth, account_ctx);

    // Generate job ID
    let job_id = uuid::Uuid::new_v4().to_string();

    // For Replace strategy, workers write directly to the real index.
    // On completion, stale documents (from previous crawls) are deleted by filter.
    let target_index_uid = config.index_uid.clone();
    let replace_index = config.index_strategy.is_replace();
    let pipeline_index_uid = config.index_uid.clone();

    info!(
        job_id = %job_id,
        index_uid = %target_index_uid,
        pipeline_index_uid = %pipeline_index_uid,
        replace_index = replace_index,
        start_urls_count = config.start_urls.len(),
        "Creating new crawl job"
    );

    // Create job state (tracks the target index_uid, not the temp one)
    let mut job = if let Some(ctx) = account_ctx {
        let mut j = JobState::with_account(&job_id, &target_index_uid, &ctx.account_id);
        j.api_key_id = ctx.api_key_id.clone();
        state.insert_job(j)
    } else {
        state.create_job(&job_id, &target_index_uid)
    };
    job.start_urls = config.start_urls.clone();
    job.max_pages = config.max_pages;
    // Redact sensitive fields before persisting config to database
    job.config = redact_crawl_config_for_storage(&config);
    // Real (unredacted) webhooks, kept in memory only, for delivery (SCR-72).
    job.webhooks = config.webhooks.clone();
    if replace_index {
        // Store Meilisearch connection info for post-crawl stale document cleanup
        job.swap_meilisearch_url = Some(config.meilisearch.url.clone());
        job.swap_meilisearch_api_key = Some(config.meilisearch.api_key.clone());
    }
    job.start();

    // Build allowed_domains list:
    // 1. If config.allowed_domains is set, use it (explicit whitelist)
    // 2. Otherwise, if config.url_patterns.allowed_domains is set, use it
    // 3. Otherwise, auto-infer from start_urls (strict: only exact domains from seed URLs)
    let allowed_domains = if !config.allowed_domains.is_empty() {
        config.allowed_domains.clone()
    } else if !config.url_patterns.allowed_domains.is_empty() {
        config.url_patterns.allowed_domains.clone()
    } else {
        // Auto-infer domains from start_urls
        let mut domains: Vec<String> = config
            .start_urls
            .iter()
            .filter_map(|u| url::Url::parse(u).ok())
            .filter_map(|u| u.host_str().map(|h| h.to_lowercase()))
            .collect();
        domains.sort();
        domains.dedup();
        domains
    };

    info!(
        job_id = %job_id,
        allowed_domains = ?allowed_domains,
        "Using domain whitelist for crawl"
    );

    // Auto-generate include patterns from start_urls path prefixes when none specified.
    // e.g. start_url "https://example.com/docs" -> include pattern "https://example.com/docs/*"
    // This prevents crawling the entire site when only a subdirectory was intended.
    // NOTE: these are full-URL glob patterns (`scheme://host/path/*`). Task B4 unifies the
    // sitemap-discovered-URL filter and this include-pattern filter onto one matcher that
    // understands full-URL patterns, so no change is needed here beyond this note.
    let include_patterns = if config.url_patterns.include.is_empty() {
        let mut patterns: Vec<String> = config
            .start_urls
            .iter()
            .filter_map(|u| url::Url::parse(u).ok())
            .filter(|u| u.path() != "/" && u.path() != "")
            .map(|u| {
                let base = format!(
                    "{}://{}{}",
                    u.scheme(),
                    u.host_str().unwrap_or(""),
                    u.path().trim_end_matches('/')
                );
                format!("{}/*", base)
            })
            .collect();
        patterns.sort();
        patterns.dedup();

        if !patterns.is_empty() {
            info!(
                job_id = %job_id,
                patterns = ?patterns,
                "Auto-generated include patterns from start_urls path prefixes"
            );
        }

        patterns
    } else {
        config.url_patterns.include.clone()
    };

    // Build URL patterns with allowed_domains
    let url_patterns = scrapix_core::UrlPatterns {
        include: include_patterns,
        exclude: config.url_patterns.exclude.clone(),
        index_only: config.url_patterns.index_only.clone(),
        allowed_domains,
    };

    // Publish seed URLs to frontier with URL patterns
    let mut urls_published = 0;
    let has_patterns = !url_patterns.include.is_empty()
        || !url_patterns.exclude.is_empty()
        || !url_patterns.allowed_domains.is_empty();

    // Extract per-job Meilisearch config to propagate through the pipeline
    let job_meilisearch_url = Some(config.meilisearch.url.clone());
    let job_meilisearch_key = Some(config.meilisearch.api_key.clone());

    // Job-scoped settings (headers, user agents, proxy, rate limits, sitemap,
    // index_only, Meilisearch primary_key/batch_size/settings/keep_settings)
    // that workers need but that aren't per-URL. Attached to every seed
    // message so it survives the frontier -> crawler -> content pipeline.
    let job_spec = Some(JobSpec::from_config(&config));

    // Exact work accounting (R5). Start from the number of seeds we are about
    // to publish and decrement on each publish failure, so `seeds_published`
    // never under-counts (which could balance the job early) while events
    // for already-published seeds race with this loop.
    {
        let mut acc = JobAccounting::default();
        acc.seeds_published = config.start_urls.len() as u64;
        state.crawl.accounting.write().insert(job_id.clone(), acc);
    }

    // Update job state (write back config, start_urls, max_pages, replace metadata)
    let replace_url = if replace_index {
        Some(config.meilisearch.url.clone())
    } else {
        None
    };
    let replace_key = if replace_index {
        Some(config.meilisearch.api_key.clone())
    } else {
        None
    };
    let snapshot = state.update_job(&job_id, |j| {
        j.status = JobStatus::Running;
        j.start_urls = config.start_urls.clone();
        j.max_pages = config.max_pages;
        // Redact sensitive fields before persisting config to database
        j.config = redact_crawl_config_for_storage(&config);
        // Real (unredacted) webhooks, kept in memory only, for delivery (SCR-72).
        j.webhooks = config.webhooks.clone();
        j.started_at = Some(chrono::Utc::now());
        j.swap_temp_index = None;
        j.swap_meilisearch_url = replace_url;
        j.swap_meilisearch_api_key = replace_key;
    });

    // Persist before any event for this job can be flushed (an update that
    // lands before its insert changes 0 rows and is lost).
    if let (Some(store), Some(ref snapshot)) = (&state.job_store, &snapshot) {
        let _ = store.insert_job(snapshot).await; // logged by the store
    }

    for url in &config.start_urls {
        let crawl_url = CrawlUrl::seed(url);
        // Use pipeline_index_uid (temp index if replace_index, otherwise target)
        let msg = if has_patterns {
            UrlMessage::with_patterns(
                crawl_url,
                &job_id,
                &pipeline_index_uid,
                url_patterns.clone(),
            )
        } else {
            UrlMessage::new(crawl_url, &job_id, &pipeline_index_uid)
        }
        .with_source(config.source.clone())
        .with_meilisearch(job_meilisearch_url.clone(), job_meilisearch_key.clone())
        .with_features(Some(config.features.clone()))
        .with_limits(config.max_depth, config.max_pages)
        .with_incremental(!replace_index)
        .with_job(job_spec.clone());

        // Attach account_id to message for billing attribution
        let msg = if let Some(ctx) = account_ctx {
            msg.account(&ctx.account_id)
        } else {
            msg
        };

        match state
            .producer
            .send(topic_names::URL_FRONTIER, Some(&job_id), &msg)
            .await
        {
            Ok(_) => {
                urls_published += 1;
                debug!(url = %url, job_id = %job_id, "Published seed URL to frontier");
            }
            Err(e) => {
                error!(url = %url, job_id = %job_id, error = %e, "Failed to publish seed URL");
                if let Some(acc) = state.crawl.accounting.write().get_mut(&job_id) {
                    acc.seeds_published = acc.seeds_published.saturating_sub(1);
                }
            }
        }
    }

    if urls_published == 0 {
        // Update job as failed (also in the store: the row was inserted
        // before publishing)
        let failed = state.update_job(&job_id, |j| j.fail("Failed to publish any seed URLs"));
        state.forget_job_tracking(&job_id);
        if let (Some(store), Some(ref failed)) = (&state.job_store, &failed) {
            let _ = store.update_job_full(failed).await; // logged by the store
        }
        return Err(ApiError::new(
            "Failed to publish seed URLs to queue",
            "queue_error",
        ));
    }

    // Publish job started event
    let event = if let Some(ctx) = account_ctx {
        CrawlEvent::job_started_with_account(
            &job_id,
            &target_index_uid,
            &ctx.account_id,
            config.start_urls.clone(),
        )
    } else {
        CrawlEvent::job_started(&job_id, &target_index_uid, config.start_urls.clone())
    };
    if let Err(e) = state
        .producer
        .send(topic_names::EVENTS, Some(&job_id), &event)
        .await
    {
        warn!(job_id = %job_id, error = %e, "Failed to publish job started event");
    }

    // Broadcast event for SSE
    state.broadcast_event(&job_id, event);

    // Remember where the job's documents go (`GET /job/{id}/results`).
    state.results.remember_crawl_target(
        &job_id,
        &config.meilisearch.url,
        &config.meilisearch.api_key,
    );

    info!(
        job_id = %job_id,
        urls_published = urls_published,
        "Crawl job created successfully"
    );

    Ok(CreateCrawlResponse {
        job_id: job_id.clone(),
        status: "running".to_string(),
        index_uid: target_index_uid,
        start_urls_count: urls_published,
        message: format!("Crawl job started with {} seed URLs", urls_published),
        warnings,
    })
}

// ============================================================================
// Map endpoint - discover URLs on a website
// ============================================================================

/// Request body for POST /map
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct MapRequest {
    /// Website URL to map
    url: String,

    /// Maximum number of links to return (default: 5000)
    #[serde(default = "default_map_limit")]
    limit: usize,

    /// How many levels deep to follow links beyond sitemap (default: 0, max: 5)
    #[serde(default)]
    depth: u32,

    /// Filter results to URLs matching this search term
    #[serde(default)]
    search: Option<String>,

    /// Render JavaScript before extracting metadata (requires Chrome/Chromium)
    #[serde(default)]
    render_js: bool,

    /// Whether to use sitemaps for discovery (default: true)
    #[serde(default = "default_true_bool")]
    sitemap: bool,

    /// Fetch <title> from each page's HTML head (default: true)
    #[serde(default = "default_true_bool")]
    get_title: bool,

    /// Fetch <meta description> from each page's HTML head (default: true)
    #[serde(default = "default_true_bool")]
    get_description: bool,

    /// Include lastmod from sitemap data (default: true)
    #[serde(default = "default_true_bool")]
    get_lastmod: bool,

    /// Include priority from sitemap data (default: true)
    #[serde(default = "default_true_bool")]
    get_priority: bool,

    /// Include changefreq from sitemap data (default: true)
    #[serde(default = "default_true_bool")]
    get_changefreq: bool,
}

fn default_map_limit() -> usize {
    5000
}

/// A discovered link with optional metadata
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
struct MapLink {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lastmod: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changefreq: Option<String>,
}

/// Response for POST /map
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct MapResponse {
    success: bool,
    links: Vec<MapLink>,
    /// Total number of links discovered
    total: usize,
    /// Time taken in milliseconds
    duration_ms: u64,
}

/// Result from fetching a single page during mapping
struct MapFetchResult {
    url: String,
    title: Option<String>,
    description: Option<String>,
    /// Newly discovered child links (url, anchor_text)
    child_links: Vec<(String, Option<String>)>,
}

/// Extract links from an HTML document, resolving relative URLs against a base.
/// Only returns same-domain http(s) links.
fn extract_page_links(html: &str, base_url: &url::Url) -> Vec<(String, Option<String>)> {
    use std::sync::OnceLock;
    static LINK_SELECTOR: OnceLock<scraper::Selector> = OnceLock::new();

    let document = scraper::Html::parse_document(html);
    let link_selector =
        LINK_SELECTOR.get_or_init(|| scraper::Selector::parse("a[href]").expect("valid selector"));
    let base_domain = base_url.host_str().unwrap_or("");

    let mut links = Vec::new();
    for element in document.select(link_selector) {
        if let Some(href) = element.value().attr("href") {
            let resolved = base_url.join(href).ok();
            if let Some(resolved_url) = resolved {
                // Only same-domain http(s) links
                if matches!(resolved_url.scheme(), "http" | "https")
                    && resolved_url.host_str().unwrap_or("") == base_domain
                {
                    // Strip fragment
                    let mut clean = resolved_url;
                    clean.set_fragment(None);
                    let url_str = clean.to_string();

                    let anchor_text = element.text().collect::<String>();
                    let anchor = if anchor_text.trim().is_empty() {
                        None
                    } else {
                        Some(anchor_text.trim().to_string())
                    };
                    links.push((url_str, anchor));
                }
            }
        }
    }
    links
}

/// The `<title>` and meta description of a page, entity-decoded
/// (`&amp;` -> `&`). Read from the head via regex (avoids a full DOM parse).
fn head_title_and_description(html: &str) -> (Option<String>, Option<String>) {
    use regex::Regex;
    use std::sync::LazyLock;

    #[allow(clippy::incompatible_msrv)]
    static RE_TITLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
    #[allow(clippy::incompatible_msrv)]
    static RE_DESC: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)<meta[^>]+name\s*=\s*["']description["'][^>]+content\s*=\s*["']([^"']*)["']"#,
        )
        .unwrap()
    });
    #[allow(clippy::incompatible_msrv)]
    static RE_DESC_ALT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)<meta[^>]+content\s*=\s*["']([^"']*)["'][^>]+name\s*=\s*["']description["']"#,
        )
        .unwrap()
    });

    let head_end = html.find("</head>").unwrap_or_else(|| {
        let mut end = 8192.min(html.len());
        while !html.is_char_boundary(end) {
            end -= 1;
        }
        end
    });
    let head = &html[..head_end];
    let clean = |raw: &str| {
        let text: String = scraper::Html::parse_fragment(raw)
            .root_element()
            .text()
            .collect();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!text.is_empty()).then_some(text)
    };
    let title = RE_TITLE
        .captures(head)
        .and_then(|c| c.get(1))
        .and_then(|m| clean(m.as_str()));
    let description = RE_DESC
        .captures(head)
        .or_else(|| RE_DESC_ALT.captures(head))
        .and_then(|c| c.get(1))
        .and_then(|m| clean(m.as_str()));
    (title, description)
}

/// Fetch a single URL and extract title, description, and child links.
async fn map_fetch_page(
    fetcher: Arc<HttpFetcher>,
    browser: Option<Arc<CdpRenderer>>,
    url: String,
    base_url: url::Url,
) -> Option<MapFetchResult> {
    let crawl_url = CrawlUrl::seed(&url);
    let fetch_fut = if let Some(ref renderer) = browser {
        let renderer = renderer.clone();
        let crawl_url = crawl_url.clone();
        Box::pin(async move { renderer.fetch(&crawl_url).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>>
    } else {
        let fetcher = fetcher.clone();
        let crawl_url = crawl_url.clone();
        Box::pin(async move { fetcher.fetch(&crawl_url).await })
    };
    let page = match tokio::time::timeout(Duration::from_secs(30), fetch_fut).await {
        Ok(Ok(page)) if (200..300).contains(&page.status) => page,
        _ => return None,
    };

    let (title, description) = head_title_and_description(&page.html);
    let child_links = extract_page_links(&page.html, &base_url);

    Some(MapFetchResult {
        url,
        title,
        description,
        child_links,
    })
}

/// Map a website: sitemap-first discovery with optional BFS deep crawl
///
/// Discovers URLs via:
/// 1. Sitemap parsing (robots.txt → sitemap.xml → sitemap indexes)
/// 2. Optional BFS link crawling when `depth > 0`
///
/// Each URL can be enriched with title, description (from HTML head) and
/// lastmod, priority, changefreq (from sitemap data) depending on `get_*` flags.
#[utoipa::path(post, path = "/map", tag = "map", request_body = MapRequest, responses((status = 200, body = MapResponse), (status = 400, body = ApiError)), security(("api_key" = [])))]
async fn map_url(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Json(request): Json<MapRequest>,
) -> Result<Json<MapResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    check_write_permission(&account_ctx)?;

    // Pre-flight (hosted): a positive balance, and JS rendering only on a
    // plan that includes it.
    if let (Some(ref lab), Some(ref ctx)) = (&state.lab_api, &account_ctx) {
        billing::check_credits(lab, &ctx.account_id).await?;
        engine_jobs::enforce_limits(
            ctx,
            0,
            engine_jobs::PlanCheck {
                max_depth: None,
                js_rendering: request.render_js,
            },
        )?;
    }

    let start_time = std::time::Instant::now();

    // Validate URL
    let parsed_url = url::Url::parse(&request.url)
        .map_err(|e| ApiError::new(format!("Invalid URL: {}", e), "validation_error"))?;

    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err(ApiError::new(
            "Only http and https URLs are supported",
            "validation_error",
        ));
    }

    // Block raw IP addresses to prevent SSRF
    if matches!(
        parsed_url.host(),
        Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_))
    ) {
        return Err(ApiError::new(
            "Raw IP addresses are not allowed, use a hostname instead",
            "validation_error",
        ));
    }

    if request.render_js && state.browser_renderer.is_none() {
        return Err(ApiError::new(
            "JS rendering is not available (Chrome/Chromium not found on this server)",
            "render_js_unavailable",
        ));
    }

    let limit = request.limit.min(10_000);
    let max_depth = request.depth.min(5);
    let needs_html_fetch = request.get_title || request.get_description;

    info!(url = %request.url, limit, depth = max_depth, render_js = request.render_js, "Mapping website URLs");

    use futures::stream::FuturesUnordered;

    // Track visited URLs and collected results
    let mut visited = HashSet::new();
    let mut results: Vec<MapLink> = Vec::new();

    // Map from URL → sitemap metadata for enrichment
    type SitemapMeta = (Option<String>, Option<f32>, Option<String>);
    let mut sitemap_meta: HashMap<String, SitemapMeta> = HashMap::new();

    let base_url = parsed_url.clone();
    let base_domain = base_url.host_str().unwrap_or("").to_string();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(50));
    let fetcher = state.fetcher.clone();
    let browser: Option<Arc<CdpRenderer>> = if request.render_js {
        state.browser_renderer.clone()
    } else {
        None
    };

    // Total timeout
    let map_deadline = std::time::Instant::now() + Duration::from_secs(60);

    // ── Step 1: Sitemap discovery (blocking, primary source) ──────────────

    if request.sitemap {
        let sitemap_parser = SitemapParser::with_defaults();
        match sitemap_parser.discover_all_urls(&request.url).await {
            Ok(sitemap_urls) => {
                debug!(count = sitemap_urls.len(), "Discovered URLs from sitemaps");
                for su in sitemap_urls {
                    if visited.len() >= limit {
                        break;
                    }
                    // Filter non-page URLs
                    if is_non_page_url(&su.loc) {
                        continue;
                    }
                    // Only same-domain URLs
                    if let Ok(su_parsed) = url::Url::parse(&su.loc) {
                        if su_parsed.host_str().unwrap_or("") != base_domain {
                            continue;
                        }
                    } else {
                        continue;
                    }
                    if visited.insert(su.loc.clone()) {
                        // Store sitemap metadata for later enrichment
                        let lastmod = if request.get_lastmod {
                            su.lastmod.map(|dt| dt.to_rfc3339())
                        } else {
                            None
                        };
                        let priority = if request.get_priority {
                            su.priority
                        } else {
                            None
                        };
                        let changefreq = if request.get_changefreq {
                            su.changefreq.map(|cf| format!("{:?}", cf).to_lowercase())
                        } else {
                            None
                        };
                        sitemap_meta.insert(
                            su.loc.clone(),
                            (lastmod.clone(), priority, changefreq.clone()),
                        );
                        results.push(MapLink {
                            url: su.loc,
                            title: None,
                            description: None,
                            lastmod,
                            priority,
                            changefreq,
                        });
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Sitemap discovery failed, falling back to BFS only");
            }
        }
    }

    // If no sitemap results (or sitemap disabled), seed with the input URL
    if results.is_empty() {
        visited.insert(request.url.clone());
        results.push(MapLink {
            url: request.url.clone(),
            title: None,
            description: None,
            lastmod: None,
            priority: None,
            changefreq: None,
        });
    }

    // ── Step 2: Enrich sitemap URLs with HTML metadata (title/description) ──

    if needs_html_fetch && !results.is_empty() {
        let urls_to_fetch: Vec<String> =
            results.iter().take(limit).map(|r| r.url.clone()).collect();

        debug!(
            count = urls_to_fetch.len(),
            "Fetching HTML metadata for sitemap URLs"
        );

        let get_title = request.get_title;
        let get_description = request.get_description;

        let mut in_flight: FuturesUnordered<_> = urls_to_fetch
            .into_iter()
            .map(|url| {
                let semaphore = semaphore.clone();
                let fetcher = fetcher.clone();
                let browser = browser.clone();
                let base_url = base_url.clone();
                tokio::spawn(async move {
                    let _permit = semaphore.acquire().await.ok()?;
                    map_fetch_page(fetcher, browser, url, base_url).await
                })
            })
            .collect();

        // Build a lookup from fetch results
        let mut meta_map: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
        while let Some(task_result) = in_flight.next().await {
            if std::time::Instant::now() > map_deadline {
                warn!("Map metadata fetch timed out");
                break;
            }
            if let Ok(Some(fetch_result)) = task_result {
                let title = if get_title { fetch_result.title } else { None };
                let desc = if get_description {
                    fetch_result.description
                } else {
                    None
                };
                meta_map.insert(fetch_result.url, (title, desc));
            }
        }

        // Merge metadata into results
        for link in &mut results {
            if let Some((title, description)) = meta_map.remove(&link.url) {
                link.title = title;
                link.description = description;
            }
        }
    }

    // ── Step 3: BFS deep crawl (only if depth > 0) ───────────────────────

    if max_depth > 0 {
        // Build frontier from all currently known URLs
        let mut frontier: Vec<String> = results.iter().map(|r| r.url.clone()).collect();

        for current_depth in 1..=max_depth {
            if frontier.is_empty() || results.len() >= limit {
                break;
            }
            if std::time::Instant::now() > map_deadline {
                warn!(url = %request.url, "Map operation timed out during BFS");
                break;
            }

            let budget = limit.saturating_sub(results.len());
            frontier.truncate(budget);

            debug!(
                depth = current_depth,
                frontier_size = frontier.len(),
                "BFS depth level"
            );

            // Fetch all frontier pages to extract child links
            let mut in_flight: FuturesUnordered<_> = frontier
                .drain(..)
                .map(|url| {
                    let semaphore = semaphore.clone();
                    let fetcher = fetcher.clone();
                    let browser = browser.clone();
                    let base_url = base_url.clone();
                    tokio::spawn(async move {
                        let _permit = semaphore.acquire().await.ok()?;
                        map_fetch_page(fetcher, browser, url, base_url).await
                    })
                })
                .collect();

            let mut next_frontier: Vec<String> = Vec::new();

            while let Some(task_result) = in_flight.next().await {
                if let Ok(Some(fetch_result)) = task_result {
                    // Extract child links for next BFS level
                    for (child_url, _anchor) in fetch_result.child_links {
                        if is_non_page_url(&child_url) {
                            continue;
                        }
                        if visited.len() < limit && visited.insert(child_url.clone()) {
                            results.push(MapLink {
                                url: child_url.clone(),
                                title: None,
                                description: None,
                                lastmod: None,
                                priority: None,
                                changefreq: None,
                            });
                            next_frontier.push(child_url);
                        }
                    }
                }

                if results.len() >= limit {
                    break;
                }
            }

            // Enrich newly discovered URLs with HTML metadata
            if needs_html_fetch && !next_frontier.is_empty() {
                let get_title = request.get_title;
                let get_description = request.get_description;

                let mut enrich_flight: FuturesUnordered<_> = next_frontier
                    .iter()
                    .map(|url| {
                        let semaphore = semaphore.clone();
                        let fetcher = fetcher.clone();
                        let browser = browser.clone();
                        let base_url = base_url.clone();
                        let url = url.clone();
                        tokio::spawn(async move {
                            let _permit = semaphore.acquire().await.ok()?;
                            map_fetch_page(fetcher, browser, url, base_url).await
                        })
                    })
                    .collect();

                let mut meta_map: HashMap<String, (Option<String>, Option<String>)> =
                    HashMap::new();
                while let Some(task_result) = enrich_flight.next().await {
                    if std::time::Instant::now() > map_deadline {
                        break;
                    }
                    if let Ok(Some(fr)) = task_result {
                        let title = if get_title { fr.title } else { None };
                        let desc = if get_description {
                            fr.description
                        } else {
                            None
                        };
                        meta_map.insert(fr.url, (title, desc));
                    }
                }
                for link in &mut results {
                    if link.title.is_none() && link.description.is_none() {
                        if let Some((title, description)) = meta_map.remove(&link.url) {
                            link.title = title;
                            link.description = description;
                        }
                    }
                }
            }

            frontier = next_frontier;
        }
    }

    let mut links = results;

    // ── Step 4: Apply search filter if provided ──────────────────────────

    if let Some(ref search) = request.search {
        let search_lower = search.to_lowercase();
        links.retain(|link| {
            link.url.to_lowercase().contains(&search_lower)
                || link
                    .title
                    .as_ref()
                    .map(|t| t.to_lowercase().contains(&search_lower))
                    .unwrap_or(false)
                || link
                    .description
                    .as_ref()
                    .map(|d| d.to_lowercase().contains(&search_lower))
                    .unwrap_or(false)
        });

        // Sort: prioritize URLs where the search term appears in the URL path
        links.sort_by(|a, b| {
            let a_match = a.url.to_lowercase().contains(&search_lower);
            let b_match = b.url.to_lowercase().contains(&search_lower);
            b_match.cmp(&a_match)
        });
    }

    links.truncate(limit);
    let total = links.len();

    let duration_ms = start_time.elapsed().as_millis() as u64;
    info!(
        url = %request.url,
        total,
        duration_ms,
        "Map completed"
    );

    // Track map request in ClickHouse request_events
    if let Some(ref batcher) = state.analytics.request_batcher {
        let account_id = account_ctx
            .as_ref()
            .map(|c| c.account_id.clone())
            .unwrap_or_default();
        let api_key_id = account_ctx
            .as_ref()
            .and_then(|c| c.api_key_id.clone())
            .unwrap_or_default();
        let domain = extract_domain(&request.url).unwrap_or_default();
        let event = ClickHouseRequestEvent {
            account_id,
            api_key_id,
            job_id: String::new(),
            operation: "map".to_string(),
            url: request.url.clone(),
            domain,
            status_code: 200,
            duration_ms: duration_ms as u32,
            content_length: 0,
            error: String::new(),
            js_rendered: request.render_js,
            ai_summary: false,
            ai_extraction: false,
            ai_prompt_tokens: 0,
            ai_completion_tokens: 0,
            ai_model: String::new(),
            urls_found: total as u32,
            pages_fetched: visited.len() as u32,
            search_query: String::new(),
            results_count: 0,
            ocr_pages: 0,
            timestamp: time::OffsetDateTime::now_utc(),
        };
        let batcher = batcher.clone();
        tokio::spawn(async move {
            if let Err(e) = batcher.add(event).await {
                debug!(error = %e, "Failed to add map request event to ClickHouse");
            }
        });
    }

    // Report the charge for a successful map to the Lab.
    if let Some(ref ctx) = account_ctx {
        record_map_usage(&state, ctx, &request.url, total).await;
    }

    Ok(Json(MapResponse {
        success: true,
        links,
        total,
        duration_ms,
    }))
}

// ============================================================================
// Search endpoint
// ============================================================================

#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct SearchRequest {
    url: String,
    q: String,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    filter: Option<serde_json::Value>,
    #[serde(default)]
    sort: Option<Vec<String>>,
}

/// Proxy search to the account's default Meilisearch engine.
#[utoipa::path(post, path = "/search", tag = "search", request_body = SearchRequest, responses((status = 200, description = "Meilisearch search results"), (status = 400, body = ApiError)), security(("api_key" = [])))]
async fn search_url(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Json(request): Json<SearchRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    check_write_permission(&account_ctx)?;

    // Pre-flight balance check
    if let (Some(ref lab), Some(ref ctx)) = (&state.lab_api, &account_ctx) {
        billing::check_credits(lab, &ctx.account_id).await?;
    }

    let start_time = std::time::Instant::now();

    // Validate URL
    let parsed_url = url::Url::parse(&request.url)
        .map_err(|e| ApiError::new(format!("Invalid URL: {}", e), "validation_error"))?;

    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err(ApiError::new(
            "Only http and https URLs are supported",
            "validation_error",
        ));
    }

    if request.q.is_empty() {
        return Err(ApiError::new(
            "Query parameter 'q' is required",
            "validation_error",
        ));
    }

    // Resolve index UID from URL
    let index_uid = scrapix_core::url_to_index_uid(&request.url);
    if index_uid.is_empty() {
        return Err(ApiError::new(
            "Could not derive index UID from URL",
            "validation_error",
        ));
    }

    // Resolve default Meilisearch engine for this account
    let target = state
        .meili
        .default_target(account_ctx.as_ref().map(|c| c.account_id.as_str()))
        .await?
        .ok_or_else(|| {
            ApiError::new(
                "No default Meilisearch configured (MEILISEARCH_URL, or an engine in Settings > Engines)",
                "not_found",
            )
        })?;
    let engine_url = target.url;
    let engine_api_key = target.api_key.unwrap_or_default();

    // Build Meilisearch search body
    let mut search_body = serde_json::json!({ "q": request.q });
    if let Some(limit) = request.limit {
        search_body["limit"] = serde_json::json!(limit);
    }
    if let Some(offset) = request.offset {
        search_body["offset"] = serde_json::json!(offset);
    }
    if let Some(ref filter) = request.filter {
        search_body["filter"] = filter.clone();
    }
    if let Some(ref sort) = request.sort {
        search_body["sort"] = serde_json::json!(sort);
    }

    // Proxy to Meilisearch
    let client = reqwest::Client::new();
    let mut req = client.post(format!(
        "{}/indexes/{}/search",
        engine_url.trim_end_matches('/'),
        index_uid
    ));
    if !engine_api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {engine_api_key}"));
    }
    req = req.json(&search_body);

    let resp = req.send().await.map_err(|e| {
        ApiError::new(
            format!("Failed to connect to Meilisearch: {e}"),
            "bad_request",
        )
    })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ApiError::new(
            format!("Meilisearch returned {status}: {body}"),
            "bad_request",
        ));
    }

    let result: serde_json::Value = resp.json().await.map_err(|e| {
        ApiError::new(
            format!("Failed to parse Meilisearch response: {e}"),
            "internal_error",
        )
    })?;

    let duration_ms = start_time.elapsed().as_millis() as u64;
    let results_count = result
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|a| a.len() as u32)
        .unwrap_or(0);

    info!(
        url = %request.url,
        q = %request.q,
        index_uid = %index_uid,
        results_count,
        duration_ms,
        "Search completed"
    );

    // Track search in ClickHouse
    if let Some(ref batcher) = state.analytics.request_batcher {
        let account_id = account_ctx
            .as_ref()
            .map(|c| c.account_id.clone())
            .unwrap_or_default();
        let api_key_id = account_ctx
            .as_ref()
            .and_then(|c| c.api_key_id.clone())
            .unwrap_or_default();
        let domain = extract_domain(&request.url).unwrap_or_default();
        let event = ClickHouseRequestEvent {
            account_id,
            api_key_id,
            job_id: String::new(),
            operation: "search".to_string(),
            url: request.url.clone(),
            domain,
            status_code: 200,
            duration_ms: duration_ms as u32,
            content_length: 0,
            error: String::new(),
            js_rendered: false,
            ai_summary: false,
            ai_extraction: false,
            ai_prompt_tokens: 0,
            ai_completion_tokens: 0,
            ai_model: String::new(),
            urls_found: 0,
            pages_fetched: 0,
            search_query: request.q.clone(),
            results_count,
            ocr_pages: 0,
            timestamp: time::OffsetDateTime::now_utc(),
        };
        let batcher = batcher.clone();
        tokio::spawn(async move {
            if let Err(e) = batcher.add(event).await {
                debug!(error = %e, "Failed to add search request event to ClickHouse");
            }
        });
    }

    // Report the charge for a search to the Lab.
    if let Some(ref ctx) = account_ctx {
        record_search_usage(&state, ctx, &request.url, &request.q, &result).await;
    }

    Ok(Json(result))
}

/// Create a new async crawl job
#[utoipa::path(post, path = "/crawl", tag = "crawl", request_body = scrapix_core::CrawlConfig, responses((status = 200, body = CreateCrawlResponse), (status = 400, body = ApiError)), security(("api_key" = [])))]
async fn create_crawl(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Json(config): Json<CrawlConfig>,
) -> Result<Json<CreateCrawlResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    check_write_permission(&account_ctx)?;
    Ok(Json(
        do_create_crawl(&state, config, account_ctx.as_ref()).await?,
    ))
}

/// Create a sync crawl job (waits for completion)
///
/// With `include_results=true`, the response also carries the first page of
/// the job's results (`GET /job/{id}/results`).
#[utoipa::path(post, path = "/crawl/sync", tag = "crawl", request_body = scrapix_core::CrawlConfig, params(results::CrawlSyncQuery), responses((status = 200, body = results::CrawlSyncResponse), (status = 400, body = ApiError)), security(("api_key" = [])))]
async fn create_crawl_sync(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Query(sync_query): Query<results::CrawlSyncQuery>,
    Json(config): Json<CrawlConfig>,
) -> Result<Json<results::CrawlSyncResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    check_write_permission(&account_ctx)?;
    // First create the async job
    let response = do_create_crawl(&state, config, account_ctx.as_ref()).await?;
    let job_id = response.job_id.clone();

    // Subscribe to events
    let mut rx = state.crawl.event_tx.subscribe();

    // Wait for job completion with timeout
    let timeout = Duration::from_secs(3600); // 1 hour timeout
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err(ApiError::new("Job timed out", "timeout"));
        }

        // Check job status
        if let Some(job) = state.get_job(&job_id) {
            match job.status {
                JobStatus::Completed => {
                    return Ok(Json(
                        results::crawl_sync_response(&state, job, &sync_query).await,
                    ));
                }
                JobStatus::Failed | JobStatus::Cancelled => {
                    return Err(ApiError::new(
                        job.error_message
                            .unwrap_or_else(|| "Job failed".to_string()),
                        "job_failed",
                    ));
                }
                _ => {}
            }
        }

        // Wait for next event or timeout
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok((event_job_id, event))) => {
                if event_job_id == job_id {
                    if let CrawlEvent::JobCompleted { .. } | CrawlEvent::JobFailed { .. } = event {
                        // Job finished, get final status
                        if let Some(job) = state.get_job(&job_id) {
                            return Ok(Json(
                                results::crawl_sync_response(&state, job, &sync_query).await,
                            ));
                        }
                    }
                }
            }
            Ok(Err(_)) => {
                // Channel closed, continue polling
            }
            Err(_) => {
                // Timeout, continue loop
            }
        }
    }
}

/// Create multiple crawl jobs from a batch of configs
#[utoipa::path(post, path = "/crawl/bulk", tag = "crawl", request_body = Vec<scrapix_core::CrawlConfig>, responses((status = 200, body = BulkCrawlResponse), (status = 400, body = ApiError)), security(("api_key" = [])))]
async fn create_crawl_bulk(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Json(configs): Json<Vec<CrawlConfig>>,
) -> Result<Json<BulkCrawlResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    check_write_permission(&account_ctx)?;

    let total = configs.len();
    let mut jobs = Vec::with_capacity(total);
    let mut errors = Vec::new();

    for (index, config) in configs.into_iter().enumerate() {
        match do_create_crawl(&state, config, account_ctx.as_ref()).await {
            Ok(response) => jobs.push(response),
            Err(e) => errors.push(BulkCrawlError {
                index,
                error: e.error.clone(),
            }),
        }
    }

    info!(
        total = total,
        succeeded = jobs.len(),
        failed = errors.len(),
        "Bulk crawl submission completed"
    );

    Ok(Json(BulkCrawlResponse {
        jobs,
        errors,
        total,
    }))
}

/// Get job status
#[utoipa::path(get, path = "/job/{id}/status", tag = "jobs", params(("id" = String, Path, description = "Job ID")), responses((status = 200, body = JobStatusResponse), (status = 404, body = ApiError)), security(("api_key" = [])))]
async fn job_status(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;

    // Try in-memory first, fall back to the job store for historical jobs
    let job = if let Some(job) = state.get_job(&job_id) {
        job
    } else if let Some(ref store) = state.job_store {
        store
            .get_job(&job_id, account_ctx.as_ref().map(|c| c.account_id.as_str()))
            .await
            .ok_or_else(|| ApiError::new("Job not found", "not_found"))?
    } else {
        return Err(ApiError::new("Job not found", "not_found"));
    };

    check_job_ownership(&job, &account_ctx)?;

    Ok(Json(job.into()))
}

/// SSE stream for job events
async fn job_events(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Path(job_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;

    // Check if job exists and ownership
    let job = state
        .get_job(&job_id)
        .ok_or_else(|| ApiError::new("Job not found", "not_found"))?;
    check_job_ownership(&job, &account_ctx)?;

    let rx = state.crawl.event_tx.subscribe();
    let target_job_id = job_id.clone();

    // Use futures::StreamExt for sync filter_map
    let stream = FuturesStreamExt::filter_map(BroadcastStream::new(rx), move |result| {
        let target = target_job_id.clone();
        async move {
            match result {
                Ok((event_job_id, event)) if event_job_id == target => {
                    let data = serde_json::to_string(&event).ok()?;
                    Some(Ok(Event::default().data(data)))
                }
                _ => None,
            }
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ============================================================================
// Job Event History (ClickHouse)
// ============================================================================

#[derive(Debug, Deserialize)]
struct JobEventsHistoryParams {
    #[serde(default = "default_history_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
    /// Comma-separated event type filter: "page_crawled,page_failed,document_indexed"
    /// Leave empty for all types.
    #[serde(default)]
    filter: String,
}

fn default_history_limit() -> u32 {
    1000
}

#[derive(Debug, Serialize)]
struct JobEventsHistoryResponse {
    events: Vec<PageEventRow>,
    returned: usize,
    limit: u32,
    offset: u32,
}

#[derive(Debug, Serialize)]
struct PageEventRow {
    event_type: String,
    url: String,
    status_code: u16,
    content_length: u64,
    duration_ms: u32,
    error: String,
    retry_count: u8,
    document_id: String,
    urls_count: u32,
    source_url: String,
    reason: String,
    domain: String,
    wait_ms: u64,
    /// Unix timestamp in milliseconds
    timestamp: i64,
}

impl From<ClickHousePageEvent> for PageEventRow {
    fn from(e: ClickHousePageEvent) -> Self {
        Self {
            event_type: e.event_type,
            url: e.url,
            status_code: e.status_code,
            content_length: e.content_length,
            duration_ms: e.duration_ms,
            error: e.error,
            retry_count: e.retry_count,
            document_id: e.document_id,
            urls_count: e.urls_count,
            source_url: e.source_url,
            reason: e.reason,
            domain: e.domain,
            wait_ms: e.wait_ms,
            timestamp: (e.timestamp.unix_timestamp_nanos() / 1_000_000) as i64,
        }
    }
}

/// Get the full persisted event history for a job from ClickHouse.
async fn get_job_events_history(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Path(job_id): Path<String>,
    Query(params): Query<JobEventsHistoryParams>,
) -> Result<Json<JobEventsHistoryResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;

    // Verify the job exists (check in-memory then the job store)
    let job = if let Some(job) = state.get_job(&job_id) {
        job
    } else if let Some(ref store) = state.job_store {
        store
            .get_job(&job_id, account_ctx.as_ref().map(|c| c.account_id.as_str()))
            .await
            .ok_or_else(|| ApiError::new("Job not found", "not_found"))?
    } else {
        return Err(ApiError::new("Job not found", "not_found"));
    };

    check_job_ownership(&job, &account_ctx)?;

    // Require ClickHouse to be configured
    let analytics = state.analytics_store.as_ref().ok_or_else(|| {
        ApiError::new(
            "Event history requires ClickHouse to be configured",
            "service_unavailable",
        )
    })?;

    // Flush pending page events so recently-written rows are visible
    if let Some(ref batcher) = state.analytics.page_event_batcher {
        if let Err(e) = batcher.flush().await {
            warn!(
                "Failed to flush page_event_batcher before history query: {}",
                e
            );
        }
    }

    // Parse optional filter param
    let filter_types: Vec<&str> = if params.filter.is_empty() {
        vec![]
    } else {
        params
            .filter
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    };

    let raw_events = analytics
        .storage
        .get_page_events(&job_id, params.limit, params.offset, &filter_types)
        .await
        .map_err(|e| {
            error!("get_page_events query failed for job {}: {}", job_id, e);
            ApiError::new(format!("Failed to query event history: {e}"), "query_error")
        })?;

    let returned = raw_events.len();
    let events: Vec<PageEventRow> = raw_events.into_iter().map(PageEventRow::from).collect();

    Ok(Json(JobEventsHistoryResponse {
        events,
        returned,
        limit: params.limit,
        offset: params.offset,
    }))
}

// ============================================================================
// WebSocket Types
// ============================================================================

/// WebSocket message from client
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsClientMessage {
    /// Subscribe to job events
    Subscribe { job_id: String },
    /// Unsubscribe from job events
    Unsubscribe { job_id: String },
    /// Request current job status
    GetStatus { job_id: String },
    /// Ping for keepalive
    Ping,
}

/// WebSocket message to client
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsServerMessage {
    /// Job event notification
    Event { job_id: String, event: CrawlEvent },
    /// Job status response
    Status {
        job_id: String,
        status: Box<JobStatusResponse>,
    },
    /// Subscription confirmed
    Subscribed { job_id: String },
    /// Unsubscription confirmed
    Unsubscribed { job_id: String },
    /// Error message
    Error { message: String, code: String },
    /// Pong response
    Pong { timestamp: i64 },
}

// ============================================================================
// WebSocket Handlers
// ============================================================================

/// WebSocket upgrade handler for real-time events
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
) -> impl IntoResponse {
    let account_ctx = extract_account_context(&account_ext).await;
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state, account_ctx))
}

/// Handle a WebSocket connection
async fn handle_ws_connection(
    socket: WebSocket,
    state: Arc<AppState>,
    account_ctx: Option<AccountContext>,
) {
    let (mut sender, mut receiver) = socket.split();

    // Track subscribed job IDs
    let subscriptions: Arc<RwLock<std::collections::HashSet<String>>> =
        Arc::new(RwLock::new(std::collections::HashSet::new()));

    // Subscribe to broadcast channel for events
    let mut event_rx = state.crawl.event_tx.subscribe();

    // Spawn task to forward events to WebSocket
    let subs = subscriptions.clone();
    let send_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok((job_id, event)) => {
                    // Only send if subscribed to this job
                    if subs.read().contains(&job_id) {
                        let msg = WsServerMessage::Event { job_id, event };
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if sender.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("WebSocket client lagged, skipped {} messages", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Handle incoming messages
    let state_clone = state.clone();
    let subs = subscriptions.clone();
    while let Some(msg) = FuturesStreamExt::next(&mut receiver).await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(client_msg) = serde_json::from_str::<WsClientMessage>(&text) {
                    let response =
                        handle_ws_message(client_msg, &state_clone, &subs, &account_ctx).await;
                    if let Ok(json) = serde_json::to_string(&response) {
                        // We can't send directly here since sender is moved
                        // The response will be handled via the broadcast channel
                        debug!("WS message processed: {}", json);
                    }
                } else {
                    debug!("Invalid WebSocket message: {}", text);
                }
            }
            Ok(Message::Ping(data)) => {
                debug!("WebSocket ping received");
                // Pong is automatically sent by axum
                let _ = data;
            }
            Ok(Message::Close(_)) => {
                info!("WebSocket connection closed by client");
                break;
            }
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }

    // Clean up
    send_task.abort();
    debug!("WebSocket connection handler finished");
}

/// Handle a WebSocket client message. A job the caller does not own is
/// "not found", as on the per-job socket.
async fn handle_ws_message(
    msg: WsClientMessage,
    state: &Arc<AppState>,
    subscriptions: &Arc<RwLock<std::collections::HashSet<String>>>,
    account_ctx: &Option<AccountContext>,
) -> WsServerMessage {
    let owned = |job_id: &str| {
        state
            .get_job(job_id)
            .filter(|job| check_job_ownership(job, account_ctx).is_ok())
    };
    match msg {
        WsClientMessage::Subscribe { job_id } => {
            if owned(&job_id).is_some() {
                subscriptions.write().insert(job_id.clone());
                info!(job_id = %job_id, "WebSocket client subscribed to job");
                WsServerMessage::Subscribed { job_id }
            } else {
                WsServerMessage::Error {
                    message: "Job not found".to_string(),
                    code: "not_found".to_string(),
                }
            }
        }
        WsClientMessage::Unsubscribe { job_id } => {
            subscriptions.write().remove(&job_id);
            info!(job_id = %job_id, "WebSocket client unsubscribed from job");
            WsServerMessage::Unsubscribed { job_id }
        }
        WsClientMessage::GetStatus { job_id } => {
            if let Some(job) = owned(&job_id) {
                WsServerMessage::Status {
                    job_id,
                    status: Box::new(job.into()),
                }
            } else {
                WsServerMessage::Error {
                    message: "Job not found".to_string(),
                    code: "not_found".to_string(),
                }
            }
        }
        WsClientMessage::Ping => WsServerMessage::Pong {
            timestamp: chrono::Utc::now().timestamp_millis(),
        },
    }
}

/// WebSocket handler for a specific job
async fn ws_job_handler(
    ws: WebSocketUpgrade,
    Path(job_id): Path<String>,
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
) -> Result<impl IntoResponse, ApiError> {
    // Check if job exists
    let job = state
        .get_job(&job_id)
        .ok_or_else(|| ApiError::new("Job not found", "not_found"))?;

    // Verify account ownership if auth is enabled
    let account_ctx = extract_account_context(&account_ext).await;
    if let Some(ref ctx) = account_ctx {
        if let Some(ref job_account_id) = job.account_id {
            if job_account_id != &ctx.account_id {
                return Err(ApiError::new("Job not found", "not_found"));
            }
        }
    }

    Ok(ws.on_upgrade(move |socket| handle_job_ws_connection(socket, state, job_id)))
}

/// Handle a WebSocket connection for a specific job
async fn handle_job_ws_connection(socket: WebSocket, state: Arc<AppState>, job_id: String) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe to broadcast channel
    let mut event_rx = state.crawl.event_tx.subscribe();
    let target_job_id = job_id.clone();

    // Send initial status
    if let Some(job) = state.get_job(&job_id) {
        let msg = WsServerMessage::Status {
            job_id: job_id.clone(),
            status: Box::new(job.into()),
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let _ = sender.send(Message::Text(json.into())).await;
        }
    }

    // Spawn task to forward events to WebSocket
    let send_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok((event_job_id, event)) if event_job_id == target_job_id => {
                    let msg = WsServerMessage::Event {
                        job_id: event_job_id,
                        event,
                    };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(_) => {
                    // Event for different job, ignore
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(job_id = %target_job_id, "WebSocket client lagged, skipped {} messages", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Handle incoming messages (mostly for keepalive)
    while let Some(msg) = FuturesStreamExt::next(&mut receiver).await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(client_msg) = serde_json::from_str::<WsClientMessage>(&text) {
                    match client_msg {
                        WsClientMessage::GetStatus { .. } => {
                            // Status requests handled via broadcast
                        }
                        WsClientMessage::Ping => {
                            // Ping handled automatically
                        }
                        _ => {}
                    }
                }
            }
            Ok(Message::Close(_)) => {
                info!(job_id = %job_id, "WebSocket connection closed");
                break;
            }
            Err(e) => {
                error!(job_id = %job_id, error = %e, "WebSocket error");
                break;
            }
            _ => {}
        }
    }

    send_task.abort();
}

/// Cancel or delete a job
///
/// Without `purge`: cancels the job. It stops everywhere (frontier and
/// workers) and the pages crawled so far are billed. Only a pending,
/// running or paused job can be cancelled: a job that already completed,
/// failed or was cancelled returns 409 and keeps its status.
///
/// With `purge=true`: deletes a finished (completed, failed or cancelled)
/// job: it disappears from `GET /jobs`, and its status and stored results
/// are gone (204). The documents a crawl indexed stay in Meilisearch. A job
/// that is not finished returns 409: cancel it first.
#[utoipa::path(delete, path = "/job/{id}", tag = "jobs", params(("id" = String, Path, description = "Job ID"), DeleteJobQuery), responses((status = 200, description = "Cancelled", body = JobStatusResponse), (status = 204, description = "Deleted (`purge=true`)"), (status = 404, body = ApiError), (status = 409, description = "Cancel: the job is already finished. Purge: the job is not finished (or is still being finalized, retry shortly)", body = ApiError)), security(("api_key" = [])))]
async fn cancel_job(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Path(job_id): Path<String>,
    Query(query): Query<DeleteJobQuery>,
) -> Result<Response, ApiError> {
    if query.purge {
        let account_ctx = extract_account_context(&account_ext).await;
        state.delete_finished_job(&job_id, &account_ctx).await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    owned_job(&state, &account_ext, &job_id).await?;
    Ok(Json(JobStatusResponse::from(state.cancel(&job_id)?)).into_response())
}

impl AppState {
    /// Delete the finished job `job_id` the caller owns, from memory and
    /// from the job store (with its results).
    async fn delete_finished_job(
        &self,
        job_id: &str,
        account_ctx: &Option<AccountContext>,
    ) -> Result<(), ApiError> {
        let scope = account_ctx.as_ref().map(|c| c.account_id.as_str());
        let job = match self.get_job(job_id) {
            Some(job) => Some(job),
            None => match &self.job_store {
                Some(store) => store.get_job(job_id, scope).await,
                None => None,
            },
        }
        .ok_or_else(|| ApiError::new("Job not found", "not_found"))?;
        check_job_ownership(&job, account_ctx)?;
        if !is_terminal(&job.status) {
            return Err(ApiError::new(
                format!(
                    "Job is {}: cancel it before deleting it",
                    job_store::status_to_str(&job.status)
                ),
                "conflict",
            ));
        }
        // Its terminal state (and billing events) must be durable first, or
        // the pending write would bring the row back.
        if self.crawl.terminal_pending.read().contains_key(job_id)
            || self.has_pending_lab_events(job_id)
        {
            return Err(
                ApiError::new("Job is still being finalized, retry shortly", "conflict")
                    .with_retry_after(5),
            );
        }
        if let Some(store) = &self.job_store {
            store.delete_job(job_id, scope).await.map_err(|e| {
                warn!(job_id = %job_id, error = %e, "Failed to delete job");
                ApiError::new("Failed to delete job", "internal_error")
            })?;
        }
        self.crawl.jobs.write().remove(job_id);
        self.forget_job_tracking(job_id);
        self.results.forget(job_id);
        info!(job_id = %job_id, "Job deleted");
        Ok(())
    }
}

/// Pause a running job
///
/// The frontier stops dispatching the job's URLs (pages already in flight
/// finish). Only a running job can be paused; any other status returns 409.
#[utoipa::path(post, path = "/job/{id}/pause", tag = "jobs", params(("id" = String, Path, description = "Job ID")), responses((status = 200, body = JobStatusResponse), (status = 404, body = ApiError), (status = 409, description = "The job is not running", body = ApiError)), security(("api_key" = [])))]
async fn pause_job(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    owned_job(&state, &account_ext, &job_id).await?;
    Ok(Json(state.pause(&job_id)?.into()))
}

/// Resume a paused job
///
/// Only a paused job can be resumed; any other status returns 409.
#[utoipa::path(post, path = "/job/{id}/resume", tag = "jobs", params(("id" = String, Path, description = "Job ID")), responses((status = 200, body = JobStatusResponse), (status = 404, body = ApiError), (status = 409, description = "The job is not paused", body = ApiError)), security(("api_key" = [])))]
async fn resume_job(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    owned_job(&state, &account_ext, &job_id).await?;
    Ok(Json(state.resume(&job_id)?.into()))
}

/// The in-memory job `job_id`, if it exists and the caller owns it.
async fn owned_job(
    state: &AppState,
    account_ext: &Option<Extension<AuthenticatedAccount>>,
    job_id: &str,
) -> Result<JobState, ApiError> {
    let account_ctx = extract_account_context(account_ext).await;
    let existing = state
        .get_job(job_id)
        .ok_or_else(|| ApiError::new("Job not found", "not_found"))?;
    check_job_ownership(&existing, &account_ctx)?;
    Ok(existing)
}

/// List jobs
///
/// The caller's jobs, newest first, paginated with `limit` (default 50, at
/// most 200) and `offset`, optionally of one `status`. List items omit the
/// job `config` (see `GET /job/{id}/status`).
#[utoipa::path(get, path = "/jobs", tag = "jobs", params(ListJobsQuery), responses((status = 200, description = "Jobs, newest first", body = Vec<JobStatusResponse>), (status = 400, description = "Unknown `status`", body = ApiError)), security(("api_key" = [])))]
async fn list_jobs(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Query(params): Query<ListJobsQuery>,
) -> Result<Json<Vec<JobStatusResponse>>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    let account = account_ctx.as_ref().map(|c| c.account_id.as_str());
    let limit = params.limit.clamp(1, MAX_LIST_JOBS_LIMIT);
    let status = match params.status.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => Some(parse_job_status(s).ok_or_else(|| {
            ApiError::new(
                format!(
                    "Unknown status `{s}` (pending, running, paused, completed, failed, cancelled)"
                ),
                "validation_error",
            )
        })?),
    };
    let wanted = |j: &JobState| status.as_ref().is_none_or(|s| &j.status == s);

    // With a job store, query it for full history (survives restarts)
    // and overlay in-memory data for running jobs (fresher counters).
    let jobs: Vec<JobState> = if let Some(ref store) = state.job_store {
        let mut db_jobs = store
            .list_jobs(
                account,
                status.as_ref().map(job_store::status_to_str),
                limit as i64,
                params.offset as i64,
            )
            .await;

        // Overlay in-memory state for active jobs (fresher counters)
        let in_memory = state.crawl.jobs.read();
        for job in &mut db_jobs {
            if let Some(mem_job) = in_memory.get(&job.job_id) {
                if matches!(
                    mem_job.status,
                    JobStatus::Running | JobStatus::Pending | JobStatus::Paused
                ) {
                    *job = mem_job.clone();
                }
            }
        }
        db_jobs.retain(|j| wanted(j));
        db_jobs
    } else {
        let mut jobs: Vec<JobState> = state
            .crawl
            .jobs
            .read()
            .values()
            .filter(|j| account.is_none_or(|a| j.account_id.as_deref() == Some(a)))
            .filter(|j| wanted(j))
            .cloned()
            .collect();
        jobs.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.job_id.cmp(&b.job_id))
        });
        jobs.into_iter().skip(params.offset).take(limit).collect()
    };
    Ok(Json(
        jobs.into_iter()
            .map(|j| JobStatusResponse {
                config: None,
                ..j.into()
            })
            .collect(),
    ))
}

/// A `JobStatus` from its API name.
fn parse_job_status(s: &str) -> Option<JobStatus> {
    Some(match s {
        "pending" => JobStatus::Pending,
        "running" => JobStatus::Running,
        "paused" => JobStatus::Paused,
        "completed" => JobStatus::Completed,
        "failed" => JobStatus::Failed,
        "cancelled" => JobStatus::Cancelled,
        _ => return None,
    })
}

// ============================================================================
// Event Consumer
// ============================================================================

/// Rebuild a recovered job's accounting from its persisted snapshot. A job
/// persisted before the `accounting` column existed (`{}` / missing / not
/// parseable) falls back to its seed count, so it still finalizes (or stalls
/// out) instead of staying Running forever.
fn restore_accounting(job: &JobState, persisted: Option<&serde_json::Value>) -> JobAccounting {
    let mut acc: JobAccounting = persisted
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    if acc.seeds_published == 0 {
        acc.seeds_published = job.start_urls.len() as u64;
    }
    acc
}

/// The job an event belongs to.
fn event_job_id(event: &CrawlEvent) -> &str {
    match event {
        CrawlEvent::JobStarted { job_id, .. }
        | CrawlEvent::PageCrawled { job_id, .. }
        | CrawlEvent::PageFailed { job_id, .. }
        | CrawlEvent::DocumentIndexed { job_id, .. }
        | CrawlEvent::UrlsDiscovered { job_id, .. }
        | CrawlEvent::JobCompleted { job_id, .. }
        | CrawlEvent::JobFailed { job_id, .. }
        | CrawlEvent::PageSkipped { job_id, .. }
        | CrawlEvent::RateLimited { job_id, .. }
        | CrawlEvent::PageRetried { job_id, .. }
        | CrawlEvent::SitemapPublished { job_id, .. }
        | CrawlEvent::DocumentSkipped { job_id, .. }
        | CrawlEvent::DocumentFailed { job_id, .. }
        | CrawlEvent::AiUsage { job_id, .. }
        | CrawlEvent::JobWarning { job_id, .. }
        | CrawlEvent::FrontierProgress { job_id, .. } => job_id,
    }
}

/// Start consuming events from a message bus to update job state.
/// Returns a JoinHandle so the caller can await clean shutdown.
fn start_event_consumer(
    consumer: AnyConsumer,
    state: Arc<AppState>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    consumer.subscribe(&[topic_names::EVENTS])?;
    info!("Event consumer subscribed to {} topic", topic_names::EVENTS);

    // Bridge the watch-channel shutdown to the AtomicBool the ack-based
    // consumer polls (it re-checks at least once per second), and to the
    // state flag that unblocks a `settle_ack` waiting at the cap.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stop = stop.clone();
        let state = state.clone();
        let mut shutdown = shutdown;
        tokio::spawn(async move {
            while !*shutdown.borrow() {
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
            state
                .shutting_down
                .store(true, std::sync::atomic::Ordering::Relaxed);
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    // At-least-once (R2): the offset commits only after the event has been
    // applied — and, for accounting events with job-store persistence, only
    // after the accounting flush containing it succeeded (R-19). Concurrency 1
    // keeps events applied in partition order. Only Kafka positions are
    // durable, so only they feed the accounting high-water mark.
    let durable_positions = matches!(consumer, AnyConsumer::Kafka(_));
    let handle = tokio::spawn(async move {
        let result = consumer
            .process_with_ack::<CrawlEvent, _, _>(
                move |event, meta, ack| {
                    let state = state.clone();
                    async move {
                        let job_id = event_job_id(&event).to_string();
                        let pos = durable_positions.then_some(EventPosition {
                            partition: meta.partition,
                            offset: meta.offset,
                        });
                        let outcome = state.process_event_at(&job_id, &event, pos);
                        state.broadcast_event(&job_id, event);
                        state.settle_ack(&job_id, ack, outcome).await;
                    }
                },
                1,
                stop,
            )
            .await;
        match result {
            Ok(()) => info!("Event consumer shut down"),
            Err(e) => error!(error = %e, "Event consumer stopped with an error"),
        }
    });

    Ok(handle)
}

// ============================================================================
// ClickHouse Initialization
// ============================================================================

/// Initialize ClickHouse storage and event batchers.
/// Returns (AnalyticsState for API, RequestEventBatcher, AiUsageBatcher, JobEventBatcher, PageEventBatcher).
async fn init_clickhouse() -> (
    Option<Arc<analytics::AnalyticsState>>,
    Option<Arc<RequestEventBatcher>>,
    Option<Arc<AiUsageBatcher>>,
    Option<Arc<JobEventBatcher>>,
    Option<Arc<PageEventBatcher>>,
) {
    // Check if ClickHouse is configured
    let config = match analytics::AnalyticsConfig::from_env() {
        Some(c) => c,
        None => {
            info!("ClickHouse not configured (CLICKHOUSE_URL not set)");
            return (None, None, None, None, None);
        }
    };

    // Initialize ClickHouse storage
    let ch_config = scrapix_storage::clickhouse::ClickHouseConfig {
        url: config.clickhouse_url.clone(),
        database: config.clickhouse_database.clone(),
        username: config.clickhouse_user.clone(),
        password: config.clickhouse_password.clone(),
        auto_create_tables: true,
        ..Default::default()
    };

    let storage = match ClickHouseStorage::new(ch_config).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "Failed to connect to ClickHouse. Analytics and event persistence disabled.");
            return (None, None, None, None, None);
        }
    };

    info!(
        url = %config.clickhouse_url,
        database = %config.clickhouse_database,
        "Connected to ClickHouse"
    );

    // Create request event batcher (batch size of 50 — 1 row per API call)
    let batcher = Arc::new(RequestEventBatcher::new(
        storage.clone(),
        50,
        "request_events",
    ));

    // Create AI usage batcher (batch size of 50 events)
    let ai_batcher = Arc::new(AiUsageBatcher::new(storage.clone(), 50, "ai_usage"));

    // Create job event batcher (batch size of 50 — lifecycle events only)
    let job_batcher = Arc::new(JobEventBatcher::new(storage.clone(), 50, "job_events"));

    // Create page event batcher (batch size of 100 — one row per crawled/failed page)
    let page_batcher = Arc::new(PageEventBatcher::new(storage.clone(), 100, "page_events"));

    // Create analytics state (sharing the same storage connection)
    let analytics_state = Arc::new(analytics::AnalyticsState::with_storage(storage));

    (
        Some(analytics_state),
        Some(batcher),
        Some(ai_batcher),
        Some(job_batcher),
        Some(page_batcher),
    )
}

// ============================================================================
// Run
// ============================================================================

/// What startup wires per mode (see [`wire_mode`]).
struct ModeWiring {
    auth_mode: auth::AuthMode,
    job_store: Arc<dyn job_store::JobStore>,
    meili: Arc<dyn meili::MeilisearchResolver>,
    /// Hosted only: the engine's lab-event outbox (in the engine's own database).
    lab_outbox: Option<Arc<dyn lab_events::LabOutbox>>,
    /// Hosted only: the Lab's internal API (auth, credit pre-check, Meilisearch).
    lab_api: Option<Arc<lab_client::LabClient>>,
}

/// The engine's own job store (both modes): opens SQLite, or connects to a
/// dedicated Postgres, refuses a Rails (Lab) database and migrates it.
async fn open_store(store: &settings::StoreUrl) -> anyhow::Result<Arc<dyn job_store::JobStore>> {
    Ok(match store {
        settings::StoreUrl::Sqlite(url) => Arc::new(
            job_store::SqliteJobStore::open(url)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        ),
        settings::StoreUrl::Postgres(url) => {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(10)
                .connect(url)
                .await
                .map_err(|e| anyhow::anyhow!("cannot connect to DATABASE_URL: {e}"))?;
            let pg = job_store::PgJobStore::new(pool);
            if pg
                .is_rails_database()
                .await
                .map_err(|e| anyhow::anyhow!("cannot inspect DATABASE_URL: {e}"))?
            {
                anyhow::bail!(
                    "DATABASE_URL points at a Rails (Lab) database; the engine needs its own database"
                );
            }
            pg.migrate()
                .await
                .map_err(|e| anyhow::anyhow!("cannot migrate DATABASE_URL: {e}"))?;
            Arc::new(pg)
        }
    })
}

/// Auth, job store and Meilisearch resolver for `settings.mode`. Every
/// failure aborts startup: an unreachable database never disables auth.
async fn wire_mode(settings: &settings::EngineSettings) -> anyhow::Result<ModeWiring> {
    match (&settings.mode, &settings.auth, &settings.store) {
        (settings::Mode::Hosted, settings::AuthSetting::Lab, store_url) => {
            let lab_cfg = settings.lab.as_ref().expect("hosted has lab settings");
            let lab_api = lab_client::LabClient::new(
                &lab_cfg.url,
                &lab_cfg.instance_id,
                &lab_cfg.instance_secret,
            );
            match lab_api.instances_me().await {
                Ok(me) if me.kind == "hosted" && me.product == "scrapix" => info!(
                    url = %lab_cfg.url,
                    instance_id = me.instance_id.as_deref().unwrap_or("-"),
                    region = me.region.as_deref().unwrap_or("-"),
                    lab_url = %me.lab_url,
                    "Lab reachable; hosted Scrapix engine"
                ),
                Ok(me) => anyhow::bail!(
                    "LAB_INSTANCE_ID {} is a {} {} deployment; this engine needs a hosted scrapix \
                     credential (bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=... \
                     URL=... CREDENTIAL=<LAB_SERVICE_TOKEN> on the Lab)",
                    me.instance_id.as_deref().unwrap_or(&lab_cfg.instance_id),
                    me.kind,
                    me.product
                ),
                Err(lab_client::LabError::CredentialsRejected) => anyhow::bail!(
                    "the Lab at {} rejected LAB_INSTANCE_ID/LAB_INSTANCE_SECRET: re-issue them from the Lab",
                    lab_cfg.url
                ),
                Err(lab_client::LabError::BadResponse(404)) => anyhow::bail!(
                    "the Lab at LAB_URL ({}) has no GET /internal/instances/me: either LAB_URL is wrong \
                     or the Lab predates contract v2",
                    lab_cfg.url
                ),
                Err(lab_client::LabError::BadResponse(code)) => anyhow::bail!(
                    "the Lab at LAB_URL ({}) answered HTTP {code}: check LAB_URL points at the Lab base URL",
                    lab_cfg.url
                ),
                Err(e) => warn!(
                    error = %e,
                    url = %lab_cfg.url,
                    "Lab unreachable at startup; serving 503s until it answers"
                ),
            }
            let store = open_store(store_url).await?;
            info!(backend = store.backend(), "Job store ready (engine-owned)");
            let lab_api = Arc::new(lab_api);
            Ok(ModeWiring {
                auth_mode: auth::AuthMode::Saas(Arc::new(auth::AuthState::new(
                    lab_api.clone(),
                    Some(lab_cfg.service_token.clone()),
                ))),
                lab_outbox: Some(store.lab_outbox()),
                job_store: store,
                meili: Arc::new(meili::LabMeilisearchResolver {
                    lab: lab_api.clone(),
                }),
                lab_api: Some(lab_api),
            })
        }
        (settings::Mode::Standalone, auth_setting, store_url) => {
            let auth_mode = match auth_setting {
                settings::AuthSetting::AdminKey(k) => {
                    auth::AuthMode::AdminKey(auth::AdminKey::new(k.clone()))
                }
                settings::AuthSetting::Disabled => {
                    warn!(
                        "SCRAPIX_AUTH=disabled: every route is UNAUTHENTICATED. \
                         Local development only."
                    );
                    auth::AuthMode::Disabled
                }
                settings::AuthSetting::Lab => {
                    unreachable!("EngineSettings::resolve never pairs Lab auth with standalone")
                }
            };
            let store = open_store(store_url).await?;
            info!(backend = store.backend(), "Job store ready");
            if let Some(ref m) = settings.meilisearch {
                let health = format!("{}/health", m.url);
                match reqwest::Client::new()
                    .get(&health)
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        info!(url = %m.url, "Default Meilisearch reachable")
                    }
                    Ok(r) => warn!(
                        url = %m.url,
                        status = %r.status(),
                        "Default Meilisearch health check failed"
                    ),
                    Err(e) => warn!(url = %m.url, error = %e, "Default Meilisearch unreachable"),
                }
            } else {
                warn!(
                    "MEILISEARCH_URL not set: every crawl must pass meilisearch.url, \
                     and /search is unavailable"
                );
            }
            Ok(ModeWiring {
                auth_mode,
                job_store: store,
                meili: Arc::new(meili::EnvResolver(settings.meilisearch.clone())),
                lab_outbox: None,
                lab_api: None,
            })
        }
        _ => unreachable!("EngineSettings::resolve guarantees a valid mode/auth/store combination"),
    }
}

/// Run the API server with pre-built message bus trait objects.
///
/// This is the primary entry point for both the standalone binary and the `scrapix all`
/// orchestration mode, where the caller injects pre-built `ChannelProducer`/`ChannelConsumer`
/// trait objects instead of Kafka ones.
pub async fn run_with_bus(
    args: Args,
    producer: AnyProducer,
    consumer: AnyConsumer,
) -> anyhow::Result<()> {
    // Resolved once, before anything connects or binds: a misconfigured
    // engine exits instead of serving requests.
    let settings = settings::EngineSettings::resolve(&args)
        .map_err(|e| anyhow::anyhow!("invalid configuration: {e}"))?;
    info!(
        host = %args.host,
        port = args.port,
        mode = ?settings.mode,
        "Starting Scrapix API server"
    );

    // Initialize ClickHouse for analytics (optional)
    let (analytics_state, request_batcher, ai_usage_batcher, job_event_batcher, page_event_batcher) =
        init_clickhouse().await;

    let ModeWiring {
        auth_mode,
        job_store,
        meili,
        lab_outbox,
        lab_api,
    } = wire_mode(&settings).await?;

    // Initialize shared HTTP fetcher for /scrape endpoint
    let robots_config = RobotsConfig {
        respect_robots: false, // /scrape is user-directed, not a crawler
        ..Default::default()
    };
    let robots_cache =
        Arc::new(RobotsCache::new(robots_config).expect("Failed to create robots cache"));
    let fetcher = Arc::new(
        HttpFetcherBuilder::new()
            .with_dns_cache()
            .build(robots_cache)
            .expect("Failed to create HTTP fetcher"),
    );
    info!("Shared HTTP fetcher initialized for /scrape endpoint");

    // Initialize browser renderer for JS rendering in /scrape and /map (optional)
    let browser_renderer = match CdpRendererBuilder::new()
        .headless(true)
        .max_concurrent_pages(5)
        .timeout(Duration::from_secs(30))
        .wait_until(WaitUntil::Load)
        .build()
        .await
    {
        Ok(renderer) => {
            info!("Browser renderer initialized for JS rendering");
            Some(Arc::new(renderer))
        }
        Err(e) => {
            info!(reason = %e, "Browser renderer unavailable (Chrome/Chromium not found). JS rendering disabled for /scrape and /map");
            None
        }
    };

    // Initialize AI service from environment (supports multiple providers via AI_PROVIDER)
    // Use with_tracking when ClickHouse is available for per-call token tracking
    let (ai_service, ai_usage_rx) = if ai_usage_batcher.is_some() {
        match AiClient::from_env_with_tracking() {
            Ok((client, rx)) => {
                let provider =
                    std::env::var("AI_PROVIDER").unwrap_or_else(|_| "anthropic".to_string());
                info!(provider = %provider, "AI service initialized with usage tracking");
                (Some(Arc::new(AiService::new(Arc::new(client)))), Some(rx))
            }
            Err(e) => {
                info!(reason = %e, "AI enrichment disabled");
                (None, None)
            }
        }
    } else {
        match AiClient::from_env() {
            Ok(client) => {
                let provider =
                    std::env::var("AI_PROVIDER").unwrap_or_else(|_| "anthropic".to_string());
                info!(provider = %provider, "AI service initialized");
                (Some(Arc::new(AiService::new(Arc::new(client)))), None)
            }
            Err(e) => {
                info!(reason = %e, "AI enrichment disabled");
                (None, None)
            }
        }
    };

    // Create application state
    let config = AppConfig::from_args(&args);
    if args.allow_private_ips {
        warn!("ALLOW_PRIVATE_IPS is set: SSRF protection for webhook deliveries is off");
    }
    let webhook_client = scrapix_crawler::safe_client_builder(None, args.allow_private_ips)
        .build()
        .expect("failed to build webhook delivery HTTP client");
    let webhook_dispatcher =
        webhooks::WebhookDispatcher::new(webhook_client, args.webhook_max_concurrent_deliveries);
    // OCR for /scrape and /parse (SCR-86). The vision backend reuses the
    // AI client, so its calls land in ai_usage_events with operation `ocr`.
    let ocr = scrapix_ocr::OcrEngine::from_env(ai_service.as_ref().map(|s| s.client().clone()))
        .await
        .map(Arc::new);
    let mut state = AppState::new(
        producer,
        config,
        request_batcher.clone(),
        ai_usage_batcher.clone(),
        job_event_batcher.clone(),
        page_event_batcher.clone(),
        fetcher,
        browser_renderer,
        ai_service,
        lab_api,
        Some(job_store),
        analytics_state.clone(),
        webhook_dispatcher,
    );
    state.ocr = ocr;
    state.meili = meili;
    state.lab = lab_outbox
        .clone()
        .map(|outbox| Arc::new(lab_events::Lab::new(outbox)));
    state.auth_disabled = matches!(auth_mode, auth::AuthMode::Disabled);
    state.crawl_browser = args.crawl_browser_available;
    let state = Arc::new(state);

    // Recover active jobs from the job store on startup
    if let Some(ref store) = state.job_store {
        let recovered = store.load_active_jobs().await;
        // Engine-run jobs (batch scrape, extract) died with the previous
        // process: mark them failed instead of recovering them as running.
        let recovered = engine_jobs::fail_interrupted(store.as_ref(), recovered).await;
        if !recovered.is_empty() {
            let now = std::time::Instant::now();
            let mut jobs = state.crawl.jobs.write();
            let mut activity = state.crawl.job_last_activity.write();
            for job in &recovered {
                jobs.insert(job.job_id.clone(), job.clone());
                if matches!(job.status, JobStatus::Running) {
                    // Restart the stall clock for recovered running jobs
                    activity.insert(job.job_id.clone(), now);
                }
            }
            info!(
                count = recovered.len(),
                "Recovered active jobs from {}",
                store.backend()
            );
        }

        // Recover the work accounting of running/paused jobs (R5). The
        // per-page seen-sets are not persisted, so redelivered events after a
        // restart are not deduplicated against pre-restart ones.
        let persisted = store.load_active_job_accounting().await;
        let persisted: HashMap<String, serde_json::Value> = persisted.into_iter().collect();
        let restored = {
            let jobs = state.crawl.jobs.read();
            let mut accs = state.crawl.accounting.write();
            for job in jobs.values() {
                if !matches!(job.status, JobStatus::Running | JobStatus::Paused) {
                    continue;
                }
                accs.insert(
                    job.job_id.clone(),
                    restore_accounting(job, persisted.get(&job.job_id)),
                );
            }
            accs.len()
        };
        if restored > 0 {
            info!(
                count = restored,
                "Recovered job work accounting from {}",
                store.backend()
            );
        }
    }

    // Shutdown coordination
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Start event consumer for real-time job tracking
    let mut consumer_handle = Some(start_event_consumer(
        consumer,
        state.clone(),
        shutdown_rx.clone(),
    )?);
    info!("Event consumer started for centralized job tracking");

    // Spawn AI usage receiver task: drains events from the AiClient channel into the ClickHouse batcher
    let ai_receiver_handle =
        if let (Some(mut rx), Some(ref batcher)) = (ai_usage_rx, &ai_usage_batcher) {
            let batcher = batcher.clone();
            let handle = tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    // Calls made inside `AI_USAGE_CONTEXT` (e.g. OCR on
                    // /scrape and /parse) carry their attribution; plain
                    // /scrape AI calls have none.
                    let ctx = event.context.unwrap_or_default();
                    let ch_event = ClickHouseAiUsageEvent {
                        provider: event.provider,
                        model: event.model,
                        operation: ctx.feature,
                        prompt_tokens: event.prompt_tokens,
                        completion_tokens: event.completion_tokens,
                        total_tokens: event.total_tokens,
                        duration_ms: event.duration_ms as u32,
                        job_id: ctx.job_id,
                        account_id: ctx.account_id.unwrap_or_default(),
                        url: ctx.url,
                        timestamp: time::OffsetDateTime::now_utc(),
                    };
                    if let Err(e) = batcher.add(ch_event).await {
                        debug!(error = %e, "Failed to add AI usage event to batcher");
                    }
                }
            });
            info!("AI usage tracking receiver started");
            Some(handle)
        } else {
            None
        };

    // Start periodic flush task (ClickHouse batchers + job-store dirty job counters)
    let has_flush_work = request_batcher.is_some()
        || ai_usage_batcher.is_some()
        || job_event_batcher.is_some()
        || state.job_store.is_some();
    let flush_handle = if has_flush_work {
        let req_batcher = request_batcher.clone();
        let ai_batcher = ai_usage_batcher.clone();
        let job_batcher = job_event_batcher.clone();
        let flush_state = state.clone();
        // With a job store, the flush task owns the consumer's shutdown join: it
        // must drain before the final flush (R-19).
        let mut consumer_join = if state.job_store.is_some() {
            consumer_handle.take()
        } else {
            None
        };
        let mut shutdown_rx = shutdown_rx.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Some(ref b) = req_batcher {
                            if let Err(e) = b.flush().await {
                                warn!(error = %e, "Failed to flush ClickHouse request batcher");
                            }
                        }
                        if let Some(ref b) = ai_batcher {
                            if let Err(e) = b.flush().await {
                                warn!(error = %e, "Failed to flush ClickHouse AI usage batcher");
                            }
                        }
                        if let Some(ref b) = job_batcher {
                            if let Err(e) = b.flush().await {
                                warn!(error = %e, "Failed to flush ClickHouse job event batcher");
                            }
                        }
                        // Flush dirty job counters + accounting to the job store,
                        // then release the acks they cover
                        if let Some(ref store) = flush_state.job_store {
                            flush_state.flush_to_db(store.as_ref()).await;
                        }
                    }
                    // A terminal write gated on Lab events became owed:
                    // record its events and persist it now (the quota and
                    // the job history read the job's row).
                    _ = flush_state.crawl.terminal_flush_wake.notified() => {
                        if let Some(ref store) = flush_state.job_store {
                            flush_state.flush_to_db(store.as_ref()).await;
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        info!("Flush task shutting down, performing final flush");
                        if let Some(ref b) = req_batcher {
                            if let Err(e) = b.flush().await {
                                warn!(error = %e, "Failed final ClickHouse request flush");
                            }
                        }
                        if let Some(ref b) = ai_batcher {
                            if let Err(e) = b.flush().await {
                                warn!(error = %e, "Failed final ClickHouse AI usage flush");
                            }
                        }
                        if let Some(ref b) = job_batcher {
                            if let Err(e) = b.flush().await {
                                warn!(error = %e, "Failed final ClickHouse job event flush");
                            }
                        }
                        // Final job-store flush — only once the event consumer
                        // has drained and sync-committed (R-19), so the final
                        // snapshot covers every event it applied.
                        if let Some(ref store) = flush_state.job_store {
                            if let Some(handle) = consumer_join.take() {
                                if let Err(e) = handle.await {
                                    warn!("Consumer task failed during shutdown: {}", e);
                                }
                            }
                            flush_state.flush_to_db(store.as_ref()).await;
                        }
                        break;
                    }
                }
            }
        });
        if request_batcher.is_some() || ai_usage_batcher.is_some() {
            info!("ClickHouse event persistence enabled (flush interval: 5s)");
        }
        if state.job_store.is_some() {
            info!("Job store counter flush enabled (flush interval: 5s)");
        }
        Some(handle)
    } else {
        None
    };

    // Job completion loop (R5): every second, finalize Running jobs whose
    // exact work accounting stayed balanced for the grace period, or that
    // stalled. Emails are scheduled by process_event only.
    let completion_state = state.clone();
    let mut completion_shutdown_rx = shutdown_rx.clone();
    let completion_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let decisions =
                        completion_state.completion_decisions(std::time::Instant::now());
                    for (job_id, decision) in decisions {
                        completion_state
                            .finalize_job(&job_id, decision, std::time::Instant::now())
                            .await;
                    }
                    completion_state.heal_silent_running(std::time::Instant::now());
                }
                _ = completion_shutdown_rx.changed() => {
                    info!("Job completion loop shutting down");
                    break;
                }
            }
        }
    });
    info!(
        grace_ms = state.config.completion_grace.as_millis() as u64,
        stall_timeout_secs = state.config.job_stall_timeout.as_secs(),
        "Job completion loop started (exact work accounting)"
    );

    // Lab event delivery: hosted only (drains the lab outbox to the Lab).
    // The Lab is loopback/private, so not the SSRF-safe client; it never
    // follows a redirect (like `LabClient`).
    let lab_handle = settings.lab.as_ref().zip(lab_outbox).map(|(cfg, outbox)| {
        let sink = lab_sink::LabSink::new(
            outbox,
            lab_sink::http_client(),
            format!("{}/internal/events", cfg.url),
            cfg.instance_id.clone(),
            cfg.instance_secret.clone(),
        );
        info!("Lab event delivery started");
        Arc::new(sink).spawn(shutdown_rx.clone())
    });

    // Routes, auth guards, request tracing, /openapi.json + /docs and the
    // body-size limits (CORS is added below).
    let mut app = router::build_router(state.clone(), &auth_mode);

    // CORS: credential-aware
    // When CORS_ORIGINS is set (comma-separated URLs), use those + *.meilisearch.com wildcard.
    // When unset, fall back to localhost defaults.
    let extra_origins: Vec<axum::http::HeaderValue> =
        if let Ok(origins) = std::env::var("CORS_ORIGINS") {
            origins
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect()
        } else {
            Vec::new()
        };
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            let origin_str = origin.to_str().unwrap_or("");
            // Always allow localhost dev
            if origin_str == "http://localhost:3001" || origin_str == "http://127.0.0.1:3001" {
                return true;
            }
            // Allow any *.meilisearch.com subdomain (https only)
            if let Some(host) = origin_str.strip_prefix("https://") {
                if host == "meilisearch.com" || host.ends_with(".meilisearch.com") {
                    return true;
                }
            }
            // Allow explicitly configured origins
            extra_origins.iter().any(|allowed| allowed == origin)
        }))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::COOKIE,
            axum::http::HeaderName::from_static("x-api-key"),
        ])
        .allow_credentials(true)
        .expose_headers([axum::http::header::SET_COOKIE]);
    app = app.layer(cors);

    // Start server with graceful shutdown
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(|e| {
            anyhow::anyhow!(
                "Invalid server address '{}:{}': {}",
                args.host,
                args.port,
                e
            )
        })?;

    // Retry binding in case the previous process hasn't released the port yet (hot-reload)
    let listener = {
        let mut retries = 0u32;
        loop {
            match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => break l,
                Err(e) if retries < 10 => {
                    retries += 1;
                    warn!(
                        "Port {} busy, retrying in 500ms ({}/10): {}",
                        args.port, retries, e
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    info!("Listening on {}", addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("SIGINT received, draining connections...");
                }
                _ = term.recv() => {
                    info!("SIGTERM received, draining connections...");
                }
            }
            let _ = shutdown_tx.send(true);
        })
        .await?;

    // Wait for background tasks to finish cleanly
    info!("Waiting for background tasks to shut down...");
    if let Some(Err(e)) = match consumer_handle {
        Some(handle) => Some(handle.await),
        None => None,
    } {
        warn!("Consumer task failed during shutdown: {}", e);
    }
    if let Err(e) = completion_handle.await {
        warn!("Completion loop task failed during shutdown: {}", e);
    }
    // Controls requested before shutdown (a cancel, a finalize's Finish)
    // still reach the pipeline, within a bounded wait.
    if !state.drain_controls(Duration::from_secs(5)).await {
        warn!(
            pending = state
                .controls_pending
                .load(std::sync::atomic::Ordering::SeqCst),
            "Shutting down with JobControls still unpublished"
        );
    }
    if let Some(handle) = lab_handle {
        if let Err(e) = handle.await {
            warn!("Lab event delivery task failed during shutdown: {}", e);
        }
    }
    if let Some(handle) = flush_handle {
        if let Err(e) = handle.await {
            warn!("Flush task failed during shutdown: {}", e);
        }
    }
    if let Some(handle) = ai_receiver_handle {
        handle.abort();
    }
    info!("Shutdown complete");

    Ok(())
}

/// Run the API server using Kafka as the message bus (standard standalone mode).
///
/// Builds Kafka producer and consumer from the broker address in `args`, then delegates
/// to [`run_with_bus`].
pub async fn run(args: Args) -> anyhow::Result<()> {
    // Create Kafka producer
    let producer: AnyProducer = ProducerBuilder::new(&args.brokers)
        .client_id("scrapix-api")
        .compression("lz4")
        .build()?
        .into();

    // Create Kafka consumer for event tracking
    let consumer: AnyConsumer = ConsumerBuilder::new(&args.brokers, "scrapix-api-events")
        .client_id("scrapix-api-event-consumer")
        .auto_offset_reset("latest") // Only process new events
        .build()?
        .into();

    info!(brokers = %args.brokers, "Connected to Kafka");

    run_with_bus(args, producer, consumer).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // preprocess_html tests
    // ========================================================================

    #[test]
    fn test_preprocess_html_passthrough() {
        let html = "<html><body><p>Hello</p></body></html>";
        let result = preprocess_html(html, &[], &[]);
        assert_eq!(result, html);
    }

    #[test]
    fn test_preprocess_html_include_selectors() {
        let html = r#"<html><body><nav>Nav</nav><main><p>Content</p></main><footer>Foot</footer></body></html>"#;
        let result = preprocess_html(html, &["main".to_string()], &[]);
        assert!(result.contains("Content"));
        assert!(!result.contains("Nav</nav>"));
        assert!(!result.contains("Foot"));
    }

    #[test]
    fn test_preprocess_html_exclude_selectors() {
        let html = r#"<html><body><p>Keep</p><nav>Remove</nav></body></html>"#;
        let result = preprocess_html(html, &[], &["nav".to_string()]);
        assert!(result.contains("Keep"));
        assert!(!result.contains("Remove"));
    }

    #[test]
    fn test_preprocess_html_include_and_exclude() {
        let html = r#"<html><body><main><p>Keep</p><div class="ad">Ad</div></main><footer>Foot</footer></body></html>"#;
        let result = preprocess_html(html, &["main".to_string()], &[".ad".to_string()]);
        assert!(result.contains("Keep"));
        assert!(!result.contains("Ad</div>"));
        assert!(!result.contains("Foot"));
    }

    #[test]
    fn test_preprocess_html_invalid_selector_skipped() {
        let html = "<html><body><p>Hello</p></body></html>";
        // Invalid selector should be skipped gracefully
        let result = preprocess_html(html, &["[[[invalid".to_string()], &[]);
        // Falls back to original HTML when include selector yields no results
        assert!(result.contains("Hello"));
    }

    #[test]
    fn test_preprocess_html_include_no_match_returns_original() {
        let html = "<html><body><p>Hello</p></body></html>";
        let result = preprocess_html(html, &[".nonexistent".to_string()], &[]);
        // When include selector matches nothing, return original HTML
        assert!(result.contains("Hello"));
    }

    // ========================================================================
    // ApiError tests
    // ========================================================================

    #[test]
    fn test_api_error_status_codes() {
        use axum::http::StatusCode;

        let test_cases = vec![
            ("not_found", StatusCode::NOT_FOUND),
            ("bad_request", StatusCode::BAD_REQUEST),
            ("validation_error", StatusCode::BAD_REQUEST),
            ("unauthorized", StatusCode::UNAUTHORIZED),
            ("conflict", StatusCode::CONFLICT),
            ("insufficient_credits", StatusCode::PAYMENT_REQUIRED),
            ("spend_limit_exceeded", StatusCode::FORBIDDEN),
            ("forbidden", StatusCode::FORBIDDEN),
            ("quota_exceeded", StatusCode::TOO_MANY_REQUESTS),
            ("fetch_error", StatusCode::BAD_GATEWAY),
            ("render_js_unavailable", StatusCode::SERVICE_UNAVAILABLE),
            ("service_unavailable", StatusCode::SERVICE_UNAVAILABLE),
            ("timeout", StatusCode::GATEWAY_TIMEOUT),
            ("analytics_unavailable", StatusCode::NOT_FOUND),
            ("internal_error", StatusCode::INTERNAL_SERVER_ERROR),
            ("unknown_code", StatusCode::INTERNAL_SERVER_ERROR),
        ];

        for (code, expected_status) in test_cases {
            let error = ApiError::new("test error", code);
            let response = error.into_response();
            assert_eq!(
                response.status(),
                expected_status,
                "Code '{}' should map to {}",
                code,
                expected_status
            );
        }
    }

    #[test]
    fn test_api_error_serialization() {
        let error = ApiError::new("Something went wrong", "bad_request");
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["error"], "Something went wrong");
        assert_eq!(json["code"], "bad_request");
        assert!(json.get("details").is_none()); // skip_serializing_if = None
    }

    #[test]
    fn test_api_error_with_details() {
        let error = ApiError::new("Validation failed", "validation_error")
            .with_details(serde_json::json!({"field": "url", "reason": "empty"}));
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["details"]["field"], "url");
    }

    #[test]
    fn crawl_browser_availability_defaults_to_unknown() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["scrapix-api"];
            argv.extend_from_slice(extra);
            Args::try_parse_from(argv).unwrap().crawl_browser_available
        };
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["--crawl-browser-available", "false"]), Some(false));
        assert_eq!(parse(&["--crawl-browser-available", "true"]), Some(true));
    }

    #[test]
    fn map_head_metadata_is_entity_decoded() {
        let html = r#"<html><head><title> Search &amp; AI
            Retrieval &#8211; Docs </title>
            <meta name="description" content="Fast &quot;search&quot; &lt;3"></head>
            <body></body></html>"#;
        let (title, description) = head_title_and_description(html);
        assert_eq!(title.as_deref(), Some("Search & AI Retrieval – Docs"));
        assert_eq!(description.as_deref(), Some("Fast \"search\" <3"));
        let (title, description) =
            head_title_and_description("<html><head><title>  </title></head></html>");
        assert_eq!((title, description), (None, None));
        // No </head>, multi-byte text across the 8 KiB cut: no panic.
        let long = format!("<title>t</title>{}", "é".repeat(5000));
        assert_eq!(head_title_and_description(&long).0.as_deref(), Some("t"));
    }

    #[test]
    fn finished_jobs_report_no_eta() {
        let mut job = JobState::new("j", "idx");
        job.start();
        job.eta_seconds = Some(110);
        assert_eq!(JobStatusResponse::from(job.clone()).eta_seconds, Some(110));
        for status in [
            JobStatus::Completed,
            JobStatus::Failed,
            JobStatus::Cancelled,
        ] {
            job.status = status;
            assert_eq!(JobStatusResponse::from(job.clone()).eta_seconds, None);
        }
    }

    // ========================================================================
    // ScrapeFormat deserialization tests
    // ========================================================================

    #[test]
    fn test_scrape_format_deserialize() {
        let formats: Vec<ScrapeFormat> =
            serde_json::from_str(r#"["markdown","html","rawhtml","content","links","metadata","screenshot","schema","blocks"]"#)
                .unwrap();
        assert_eq!(formats.len(), 9);
        assert_eq!(formats[0], ScrapeFormat::Markdown);
        assert_eq!(formats[7], ScrapeFormat::Schema);
        assert_eq!(formats[8], ScrapeFormat::Blocks);
    }

    #[test]
    fn test_scrape_request_defaults() {
        let json = r#"{"url": "https://example.com"}"#;
        let request: ScrapeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.url, "https://example.com");
        assert!(request.formats.is_empty());
        assert!(request.only_main_content); // default true
        assert!(!request.include_links); // default false
        assert_eq!(request.timeout_ms, 30000); // default 30s
        assert!(request.headers.is_empty());
        assert!(request.exclude_selectors.is_empty());
        assert!(request.include_selectors.is_empty());
        assert!(request.extract.is_empty());
        assert!(request.ai.is_none());
    }

    #[test]
    fn test_scrape_request_full() {
        let json = r#"{
            "url": "https://example.com",
            "formats": ["markdown", "schema", "blocks"],
            "only_main_content": false,
            "include_links": true,
            "timeout_ms": 5000,
            "headers": {"User-Agent": "TestBot"},
            "exclude_selectors": ["nav", "footer"],
            "include_selectors": ["main"],
            "ai": {
                "summary": true,
                "extract": {
                    "prompt": "Extract product info",
                    "schema": [
                        {"name": "price", "description": "Product price", "field_type": "number", "required": true}
                    ]
                }
            }
        }"#;
        let request: ScrapeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.formats.len(), 3);
        assert!(!request.only_main_content);
        assert!(request.include_links);
        assert_eq!(request.timeout_ms, 5000);
        assert_eq!(request.headers.get("User-Agent").unwrap(), "TestBot");
        assert_eq!(request.exclude_selectors, vec!["nav", "footer"]);
        assert_eq!(request.include_selectors, vec!["main"]);

        let ai = request.ai.unwrap();
        assert!(ai.summary);
        let extract = ai.extract.unwrap();
        assert_eq!(extract.prompt, "Extract product info");
        let schema = extract.schema.unwrap();
        assert_eq!(schema[0].name, "price");
        assert_eq!(schema[0].field_type, "number");
        assert!(schema[0].required);
    }

    #[test]
    fn test_ai_field_def_defaults() {
        let json = r#"{"name": "title", "description": "Page title"}"#;
        let field: AiFieldDef = serde_json::from_str(json).unwrap();
        assert_eq!(field.field_type, "string"); // default
        assert!(!field.required); // default false
    }

    // ========================================================================
    // URL validation tests (extracted from scrape_url logic)
    // ========================================================================

    #[test]
    fn test_url_validation_accepts_http() {
        let parsed = url::Url::parse("http://example.com").unwrap();
        assert!(matches!(parsed.scheme(), "http" | "https"));
    }

    #[test]
    fn test_url_validation_accepts_https() {
        let parsed = url::Url::parse("https://example.com").unwrap();
        assert!(matches!(parsed.scheme(), "http" | "https"));
    }

    #[test]
    fn test_url_validation_rejects_ftp() {
        let parsed = url::Url::parse("ftp://example.com").unwrap();
        assert!(!matches!(parsed.scheme(), "http" | "https"));
    }

    #[test]
    fn test_url_validation_rejects_javascript() {
        let parsed = url::Url::parse("javascript:alert(1)");
        // javascript: URLs either fail to parse or fail the scheme check
        if let Ok(p) = parsed {
            assert!(!matches!(p.scheme(), "http" | "https"));
        }
    }

    #[test]
    fn test_url_validation_blocks_raw_ipv4() {
        let parsed = url::Url::parse("http://192.168.1.1/admin").unwrap();
        assert!(matches!(
            parsed.host(),
            Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_))
        ));
    }

    #[test]
    fn test_url_validation_blocks_raw_ipv6() {
        let parsed = url::Url::parse("http://[::1]/admin").unwrap();
        assert!(matches!(
            parsed.host(),
            Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_))
        ));
    }

    #[test]
    fn test_url_validation_allows_hostname() {
        let parsed = url::Url::parse("https://example.com").unwrap();
        assert!(!matches!(
            parsed.host(),
            Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_))
        ));
    }

    #[test]
    fn test_url_validation_allows_subdomain() {
        let parsed = url::Url::parse("https://docs.example.com/guide").unwrap();
        assert!(matches!(parsed.scheme(), "http" | "https"));
        assert!(!matches!(
            parsed.host(),
            Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_))
        ));
    }

    // ========================================================================
    // Blocked headers test (SSRF prevention)
    // ========================================================================

    #[test]
    fn test_blocked_headers() {
        let blocked: &[&str] = &[
            "host",
            "transfer-encoding",
            "connection",
            "upgrade",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
        ];

        // Verify the canonical list of headers that must be blocked
        for header in blocked {
            assert!(
                header.to_lowercase() == *header,
                "Blocked headers should be lowercase for case-insensitive comparison"
            );
        }
    }

    // ========================================================================
    // crawl_config_warnings / validate_crawl_config tests (Task 5 / R4)
    // ========================================================================

    #[test]
    fn warnings_flag_worker_level_fields() {
        let mut cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a",
            "concurrency": {"browser_pool_size": 99}
        }))
        .unwrap();
        let w = crawl_config_warnings(&cfg);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("browser_pool_size"));
        cfg.concurrency = Default::default();
        assert!(crawl_config_warnings(&cfg).is_empty());
    }

    #[test]
    fn warnings_flag_dns_concurrency() {
        let cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a",
            "concurrency": {"dns_concurrency": 7}
        }))
        .unwrap();
        let w = crawl_config_warnings(&cfg);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("dns_concurrency"));
    }

    #[test]
    fn pdf_extract_links_warns_only_without_pdf() {
        let cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a",
            "features": {"pdf": {"enabled": true, "extract_links": true}}
        }))
        .unwrap();
        assert!(crawl_config_warnings(&cfg).is_empty(), "implemented now");

        let cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a",
            "features": {"pdf": {"enabled": false, "extract_links": true}}
        }))
        .unwrap();
        let w = crawl_config_warnings(&cfg);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("extract_links"));
    }

    #[test]
    fn ocr_without_pdf_warns() {
        let cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a",
            "features": {"ocr": {"mode": "auto"}}
        }))
        .unwrap();
        let w = crawl_config_warnings(&cfg);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("features.ocr"));
    }

    #[test]
    fn warnings_empty_for_default_config() {
        let cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a"
        }))
        .unwrap();
        assert!(crawl_config_warnings(&cfg).is_empty());
    }

    #[test]
    fn validate_rejects_empty_start_urls_after_defaulting() {
        let cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": [], "index_uid": "a"
        }))
        .unwrap();
        assert!(validator::Validate::validate(&cfg).is_err());
    }

    #[test]
    fn validate_crawl_config_rejects_invalid_config_with_validation_error() {
        // do_create_crawl needs a full AppState (producer, DB pool, fetcher, ...),
        // so we test the extracted validation step it calls instead.
        let mut cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": [], "index_uid": "a"
        }))
        .unwrap();

        let err = validate_crawl_config(&mut cfg).expect_err("empty start_urls must be rejected");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "validation_error");
        assert_eq!(
            err.into_response().status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }

    fn config_with_proxy(proxy: serde_json::Value) -> CrawlConfig {
        serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a", "proxy": proxy
        }))
        .unwrap()
    }

    #[test]
    fn validate_crawl_config_rejects_unsafe_or_empty_proxies() {
        for proxy in [
            serde_json::json!({"urls": ["http://169.254.169.254:80"]}),
            serde_json::json!({"urls": ["http://10.0.0.1:3128"]}),
            serde_json::json!({"urls": ["http://[::1]:3128"]}),
            serde_json::json!({"urls": [], "tiered": [["http://127.0.0.1:1"]]}),
            serde_json::json!({"urls": ["ftp://proxy.test:21"]}),
            serde_json::json!({"urls": ["socks5://proxy.test:1080"]}),
            serde_json::json!({"urls": ["not a url"]}),
            serde_json::json!({"urls": []}),
            serde_json::json!({"urls": [], "tiered": [[]]}),
        ] {
            let err = validate_crawl_config(&mut config_with_proxy(proxy.clone()))
                .expect_err(&format!("{proxy} must be rejected"));
            let json = serde_json::to_value(&err).unwrap();
            assert_eq!(json["code"], "validation_error", "{proxy}");
        }
    }

    #[test]
    fn validate_crawl_config_accepts_public_proxies() {
        for proxy in [
            serde_json::json!({"urls": ["http://proxy.example.com:8080"]}),
            serde_json::json!({"urls": ["https://user:pass@1.2.3.4:8443"]}),
            serde_json::json!({"urls": [], "tiered": [["http://p1.example.com:1"], ["http://p2.example.com:1"]]}),
        ] {
            assert!(
                validate_crawl_config(&mut config_with_proxy(proxy.clone())).is_ok(),
                "{proxy}"
            );
        }
    }

    /// Final review fix 3: proxy credentials never appear in validation
    /// errors (they are returned to the client and may be logged).
    #[test]
    fn proxy_validation_errors_redact_credentials() {
        for proxy in [
            serde_json::json!({"urls": ["http://alice:s3cret@10.0.0.1:3128"]}),
            serde_json::json!({"urls": ["socks5://alice:s3cret@proxy.test:1080"]}),
            serde_json::json!({"urls": ["ftp://alice:s3cret@proxy.test:21"]}),
            serde_json::json!({"urls": [], "tiered": [["http://alice:s3cret@127.0.0.1:1"]]}),
            serde_json::json!({"urls": ["http://alice:s3cret@exa mple:1"]}),
        ] {
            let err = validate_crawl_config(&mut config_with_proxy(proxy.clone()))
                .expect_err(&format!("{proxy} must be rejected"));
            assert!(
                !err.error.contains("s3cret") && !err.error.contains("alice"),
                "{proxy}: {}",
                err.error
            );
        }
    }

    /// Final review fix 3: the persisted/returned config masks proxy
    /// credentials and custom header values; the in-memory config (from
    /// which the crawler's `JobSpec` is built) keeps the real ones.
    #[test]
    fn stored_config_redacts_proxy_userinfo_and_header_values() {
        let cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a",
            "proxy": {
                "urls": ["http://alice:s3cret@proxy.example.com:8080"],
                "tiered": [["https://bob:hunter2@p2.example.com:1"], ["http://plain.example.com:1"]]
            },
            "headers": {"Authorization": "Bearer tok-123", "X-Tenant": "t1"}
        }))
        .unwrap();
        let stored = redact_crawl_config_for_storage(&cfg).unwrap();
        let text = stored.to_string();
        for secret in ["s3cret", "alice", "hunter2", "tok-123", "t1\""] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(
            stored["proxy"]["urls"][0],
            "http://***@proxy.example.com:8080/"
        );
        assert_eq!(
            stored["proxy"]["tiered"][1][0],
            "http://plain.example.com:1"
        );
        assert_eq!(stored["headers"]["Authorization"], "***");
        assert_eq!(stored["headers"]["X-Tenant"], "***");

        // The crawler gets the real values (JobSpec is built from the
        // in-memory config, never from the stored copy).
        let spec = JobSpec::from_config(&cfg);
        assert_eq!(
            spec.proxy.unwrap().urls[0],
            "http://alice:s3cret@proxy.example.com:8080"
        );
        assert_eq!(spec.headers["Authorization"], "Bearer tok-123");
    }

    #[test]
    fn crawl_config_warnings_flags_browser_with_proxy() {
        let mut cfg = config_with_proxy(serde_json::json!({"urls": ["http://p.example.com:1"]}));
        assert!(crawl_config_warnings(&cfg).is_empty());
        cfg.crawler_type = CrawlerType::Browser;
        let w = crawl_config_warnings(&cfg);
        assert!(w.iter().any(|m| m.contains("proxy")), "{w:?}");
    }

    #[test]
    fn validate_crawl_config_accepts_valid_config() {
        let mut cfg: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a"
        }))
        .unwrap();
        assert!(validate_crawl_config(&mut cfg).is_ok());
    }

    fn crawl_config(v: serde_json::Value) -> CrawlConfig {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn validate_rejects_invalid_start_urls() {
        for bad in ["not a url", "ftp://a.test/", "https://", "/relative"] {
            let mut cfg = crawl_config(serde_json::json!({
                "start_urls": ["https://a.test", bad], "index_uid": "a"
            }));
            let err = validate_crawl_config(&mut cfg)
                .err()
                .unwrap_or_else(|| panic!("{bad} must be refused"));
            assert_eq!(err.code, "validation_error");
            assert!(err.error.starts_with("start_urls[1]: "), "{}", err.error);
        }
    }

    #[test]
    fn validate_rejects_invalid_index_uids() {
        let long = "a".repeat(512);
        for bad in ["bad uid!", "a/b", "é", long.as_str()] {
            let mut cfg = crawl_config(serde_json::json!({
                "start_urls": ["https://a.test"], "index_uid": bad
            }));
            let err = validate_crawl_config(&mut cfg)
                .err()
                .unwrap_or_else(|| panic!("{bad} must be refused"));
            assert!(err.error.starts_with("index_uid: "), "{}", err.error);
        }
        let max = "A-z_0".repeat(102); // 510 chars
        for good in ["docs", "Docs_v2-en", max.as_str()] {
            let mut cfg = crawl_config(serde_json::json!({
                "start_urls": ["https://a.test"], "index_uid": good
            }));
            assert!(validate_crawl_config(&mut cfg).is_ok(), "{good}");
        }
    }

    #[test]
    fn ai_crawl_features_need_an_ai_provider() {
        for features in [
            serde_json::json!({"ai_summary": {"enabled": true}}),
            serde_json::json!({"ai_extraction": {"enabled": true, "prompt": "p"}}),
        ] {
            let cfg = crawl_config(serde_json::json!({
                "start_urls": ["https://a.test"], "index_uid": "a", "features": features
            }));
            let err = check_crawl_capabilities(&cfg, false, None).err().unwrap();
            assert_eq!(err.code, "service_unavailable");
            assert_eq!(
                err.into_response().status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
            assert!(check_crawl_capabilities(&cfg, true, None).is_ok());
        }
        let disabled = crawl_config(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a",
            "features": {"ai_summary": {"enabled": false}}
        }));
        assert!(check_crawl_capabilities(&disabled, false, None).is_ok());
    }

    #[test]
    fn browser_crawls_are_refused_only_without_a_browser() {
        let browser = crawl_config(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a", "crawler_type": "browser"
        }));
        let err = check_crawl_capabilities(&browser, true, Some(false))
            .err()
            .unwrap();
        assert_eq!(err.code, "render_js_unavailable");
        assert_eq!(
            err.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // Unknown (distributed workers): accepted, as before.
        assert!(check_crawl_capabilities(&browser, true, None).is_ok());
        assert!(check_crawl_capabilities(&browser, true, Some(true)).is_ok());
        let http = crawl_config(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a"
        }));
        assert!(check_crawl_capabilities(&http, true, Some(false)).is_ok());
    }

    fn config_with_webhook(webhook: serde_json::Value) -> CrawlConfig {
        serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test"], "index_uid": "a", "webhooks": [webhook]
        }))
        .unwrap()
    }

    #[test]
    fn rejects_private_ip_webhook_url() {
        for url in [
            "http://127.0.0.1/hook",
            "http://10.0.0.5/hook",
            "http://169.254.169.254/hook",
        ] {
            let mut cfg = config_with_webhook(serde_json::json!({
                "url": url, "events": ["crawl_completed"]
            }));
            let err =
                validate_crawl_config(&mut cfg).expect_err(&format!("{url} must be rejected"));
            let json = serde_json::to_value(&err).unwrap();
            assert_eq!(json["code"], "validation_error", "{url}");
        }
    }

    #[test]
    fn accepts_public_webhook_url() {
        let mut cfg = config_with_webhook(serde_json::json!({
            "url": "https://example.com/hook", "events": ["crawl_completed"]
        }));
        assert!(validate_crawl_config(&mut cfg).is_ok());
    }

    #[test]
    fn rejects_non_sha256_hmac_webhook_algorithm() {
        let mut cfg = config_with_webhook(serde_json::json!({
            "url": "https://example.com/hook",
            "events": ["crawl_completed"],
            "auth": {"hmac": {"secret": "s", "algorithm": "sha1", "header": "X-Sig"}}
        }));
        assert!(validate_crawl_config(&mut cfg).is_err());
    }

    #[test]
    fn clamps_out_of_range_webhook_timeout_at_validation() {
        let mut too_short = config_with_webhook(serde_json::json!({
            "url": "https://example.com/hook", "events": ["crawl_completed"], "timeout_ms": 10
        }));
        validate_crawl_config(&mut too_short).unwrap();
        assert_eq!(too_short.webhooks[0].timeout_ms, webhooks::MIN_TIMEOUT_MS);

        let mut too_long = config_with_webhook(serde_json::json!({
            "url": "https://example.com/hook", "events": ["crawl_completed"], "timeout_ms": 600_000
        }));
        validate_crawl_config(&mut too_long).unwrap();
        assert_eq!(too_long.webhooks[0].timeout_ms, webhooks::MAX_TIMEOUT_MS);
    }

    #[test]
    fn webhook_secrets_are_redacted_in_persisted_config() {
        let cfg = config_with_webhook(serde_json::json!({
            "url": "https://example.com/hook",
            "events": ["crawl_completed"],
            "auth": {"bearer": {"token": "top-secret-token"}}
        }));
        let redacted = redact_crawl_config_for_storage(&cfg).expect("config must serialize");
        let hook = &redacted["webhooks"][0];
        assert_eq!(hook["auth"]["bearer"]["token"], "***");
        assert_eq!(
            hook["url"], "https://example.com/hook",
            "non-secret fields survive"
        );
        // The real config (not the redacted JSON) still carries the real
        // secret — this is what gets copied onto JobState::webhooks for
        // in-memory delivery.
        assert_eq!(cfg.webhooks[0].url, "https://example.com/hook");
    }

    // ========================================================================
    // wire_mode
    // ========================================================================

    fn hosted_settings(
        lab_url: &str,
        instance_secret: &str,
        store: settings::StoreUrl,
    ) -> settings::EngineSettings {
        settings::EngineSettings {
            mode: settings::Mode::Hosted,
            auth: settings::AuthSetting::Lab,
            store,
            meilisearch: None,
            lab: Some(settings::LabSettings {
                url: lab_url.into(),
                instance_id: crate::lab_client::testing::INSTANCE_ID.into(),
                instance_secret: instance_secret.into(),
                service_token: "s".repeat(32),
            }),
        }
    }

    #[tokio::test]
    async fn hosted_wiring_runs_on_its_own_sqlite_store() {
        let lab = crate::lab_client::testing::FakeLab::start().await;
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/engine.db", dir.path().display());
        let w = wire_mode(&hosted_settings(
            &lab.url,
            crate::lab_client::testing::SECRET,
            settings::StoreUrl::Sqlite(url),
        ))
        .await
        .unwrap();
        assert!(w.lab_api.is_some());
        assert!(w.lab_outbox.is_some());
    }

    #[tokio::test]
    async fn hosted_wiring_refuses_wrong_instance_credentials() {
        let lab = crate::lab_client::testing::FakeLab::start().await;
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/engine.db", dir.path().display());
        let err = wire_mode(&hosted_settings(
            &lab.url,
            &"cd".repeat(32),
            settings::StoreUrl::Sqlite(url),
        ))
        .await
        .err()
        .unwrap();
        let msg = err.to_string();
        assert!(msg.contains("rejected"), "{msg}");
        assert!(msg.contains("LAB_INSTANCE_ID/LAB_INSTANCE_SECRET"), "{msg}");
    }

    #[tokio::test]
    async fn a_404_on_instances_me_aborts_startup() {
        let lab = crate::lab_client::testing::FakeLab::start().await;
        lab.state
            .status_override
            .store(404, std::sync::atomic::Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let settings = settings::EngineSettings {
            mode: settings::Mode::Hosted,
            auth: settings::AuthSetting::Lab,
            store: settings::StoreUrl::Sqlite(format!(
                "sqlite://{}/engine.db",
                dir.path().display()
            )),
            meilisearch: None,
            lab: Some(settings::LabSettings {
                url: lab.url.clone(),
                instance_id: crate::lab_client::testing::INSTANCE_ID.into(),
                instance_secret: crate::lab_client::testing::SECRET.into(),
                service_token: "0123456789abcdef0123456789abcdef".into(),
            }),
        };
        let err = wire_mode(&settings).await.err().unwrap().to_string();
        assert!(err.contains("/internal/instances/me"), "{err}");
        assert!(err.contains("LAB_URL"), "{err}");
    }

    #[tokio::test]
    async fn hosted_wiring_refuses_a_non_scrapix_instance() {
        let lab = crate::lab_client::testing::FakeLab::start().await;
        let mut me = crate::lab_client::testing::FakeLab::hosted_me();
        me["product"] = serde_json::json!("meilisearch");
        *lab.state.me.lock().unwrap() = me;
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/engine.db", dir.path().display());
        let err = wire_mode(&hosted_settings(
            &lab.url,
            crate::lab_client::testing::SECRET,
            settings::StoreUrl::Sqlite(url),
        ))
        .await
        .err()
        .unwrap();
        let msg = err.to_string();
        assert!(msg.contains("hosted meilisearch deployment"), "{msg}");
        assert!(msg.contains("LAB_INSTANCE_ID"), "{msg}");
    }

    #[tokio::test]
    async fn hosted_wiring_refuses_a_lab_url_that_answers_4xx() {
        let lab = crate::lab_client::testing::FakeLab::start().await;
        lab.state
            .status_override
            .store(403, std::sync::atomic::Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/engine.db", dir.path().display());
        let err = wire_mode(&hosted_settings(
            &lab.url,
            crate::lab_client::testing::SECRET,
            settings::StoreUrl::Sqlite(url),
        ))
        .await
        .err()
        .unwrap();
        let msg = err.to_string();
        assert!(msg.contains("LAB_URL"), "{msg}");
        assert!(msg.contains("403"), "{msg}");
    }

    #[tokio::test]
    async fn hosted_wiring_starts_while_the_lab_is_down() {
        let lab = crate::lab_client::testing::FakeLab::start().await;
        lab.set_down(true);
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/engine.db", dir.path().display());
        assert!(wire_mode(&hosted_settings(
            &lab.url,
            crate::lab_client::testing::SECRET,
            settings::StoreUrl::Sqlite(url)
        ))
        .await
        .is_ok());
    }

    /// Minor 12 (PG-gated): the Rails-schema guard, through `wire_mode` →
    /// `open_store` on a real Postgres. Refused before anything is migrated.
    #[tokio::test]
    async fn hosted_wiring_refuses_a_rails_postgres_database() {
        let Ok(url) = std::env::var("JOBSTORE_TEST_DATABASE_URL") else {
            eprintln!("skipped");
            return;
        };
        let admin = sqlx::PgPool::connect(&url).await.unwrap();
        let scoped = |schema: &str| {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}options=-c%20search_path%3D{schema}")
        };
        let tables = |schema: String| {
            let admin = admin.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT table_name::text FROM information_schema.tables \
                     WHERE table_schema = $1 ORDER BY 1",
                )
                .bind(schema)
                .fetch_all(&admin)
                .await
                .unwrap()
            }
        };
        let lab = crate::lab_client::testing::FakeLab::start().await;
        let wire = |store: String| {
            let lab_url = lab.url.clone();
            async move {
                wire_mode(&hosted_settings(
                    &lab_url,
                    crate::lab_client::testing::SECRET,
                    settings::StoreUrl::Postgres(store),
                ))
                .await
            }
        };

        let rails = format!("t_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {rails}"))
            .execute(&admin)
            .await
            .unwrap();
        sqlx::query(&format!(
            "CREATE TABLE {rails}.schema_migrations (version text)"
        ))
        .execute(&admin)
        .await
        .unwrap();
        let err = wire(scoped(&rails)).await.err().unwrap();
        assert!(err.to_string().contains("Rails (Lab) database"), "{err}");
        assert_eq!(
            tables(rails).await,
            vec!["schema_migrations".to_string()],
            "nothing migrated into it"
        );

        // Control: an empty database of its own is accepted and migrated.
        let own = format!("t_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {own}"))
            .execute(&admin)
            .await
            .unwrap();
        assert!(wire(scoped(&own)).await.is_ok());
        assert!(tables(own).await.contains(&"lab_events".to_string()));
    }

    #[tokio::test]
    async fn standalone_builds_no_lab_client() {
        let dir = tempfile::tempdir().unwrap();
        let s = settings::EngineSettings {
            mode: settings::Mode::Standalone,
            auth: settings::AuthSetting::AdminKey("k".repeat(16)),
            store: settings::StoreUrl::Sqlite(format!("sqlite://{}/s.db", dir.path().display())),
            meilisearch: None,
            lab: None,
        };
        let w = wire_mode(&s).await.unwrap();
        assert!(w.lab_api.is_none());
        assert!(w.lab_outbox.is_none());
    }
}

/// Job lifecycle (R5/R9) tests on a DB-less `AppState` over the in-process bus.
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use scrapix_queue::{ChannelBus, MessageConsumer};
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    fn test_config() -> AppConfig {
        AppConfig {
            max_jobs: 100,
            job_stall_timeout: Duration::from_secs(1800),
            completion_grace: Duration::from_secs(3),
            resume_heal_after: Duration::from_secs(60),
            max_pending_acks: 50_000,
        }
    }

    fn test_fetcher() -> Arc<HttpFetcher> {
        let robots = Arc::new(RobotsCache::new(RobotsConfig::default()).unwrap());
        Arc::new(HttpFetcherBuilder::new().build(robots).unwrap())
    }

    fn test_webhooks() -> webhooks::WebhookDispatcher {
        webhooks::WebhookDispatcher::new(
            scrapix_crawler::safe_client_builder(None, true)
                .build()
                .unwrap(),
            webhooks::DEFAULT_MAX_CONCURRENT_DELIVERIES,
        )
    }

    fn test_state(bus: &ChannelBus) -> AppState {
        AppState::new(
            AnyProducer::channel(bus.producer()),
            test_config(),
            None,
            None,
            None,
            None,
            test_fetcher(),
            None,
            None,
            None,
            None,
            None,
            test_webhooks(),
        )
    }

    #[tokio::test]
    async fn sqlite_store_enables_durable_accounting() {
        let bus = ChannelBus::new();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::job_store::SqliteJobStore::open(&format!(
            "sqlite://{}",
            dir.path().join("a.db").display()
        ))
        .await
        .unwrap();
        let state = AppState::new(
            AnyProducer::channel(bus.producer()),
            test_config(),
            None,
            None,
            None,
            None,
            test_fetcher(),
            None,
            None,
            None, // lab_api
            Some(Arc::new(store) as Arc<dyn crate::job_store::JobStore>),
            None,
            test_webhooks(),
        );
        assert!(state.accounting_persisted());
        assert!(state.lab_api.is_none());
    }

    fn ctx() -> AccountContext {
        AccountContext {
            account_id: "7f1c2a8e-0000-4000-8000-000000000001".into(),
            api_key_id: Some("k".into()),
            tier: "free".into(),
            user_role: None,
            limits: None,
        }
    }

    #[tokio::test]
    async fn record_usage_writes_one_event_with_context() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        state
            .record_usage(
                &ctx(),
                "scrape",
                1,
                serde_json::json!({"pages_http": 1}),
                "https://e.com".into(),
                None,
            )
            .await;
        let events = outbox.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "usage.recorded");
        assert_eq!(events[0].account_id, "7f1c2a8e-0000-4000-8000-000000000001");
        assert_eq!(events[0].api_key_id.as_deref(), Some("k"));
        assert_eq!(events[0].data["operation"], "scrape");
        assert_eq!(events[0].data["units"]["pages_http"], 1);
        assert_eq!(events[0].data["credits"], 1);
        assert!(events[0].data.get("job_id").is_none());
    }

    #[tokio::test]
    async fn record_usage_is_a_noop_without_a_lab() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        let ctx = AccountContext {
            account_id: "a".into(),
            api_key_id: None,
            tier: "free".into(),
            user_role: None,
            limits: None,
        };
        state
            .record_usage(&ctx, "map", 2, serde_json::json!({}), "m".into(), None)
            .await; // must not panic
    }

    #[tokio::test]
    async fn scrape_usage_event_carries_page_kind_and_ai_flags() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        let formats = [ScrapeFormat::Markdown, ScrapeFormat::RawHtml];
        record_scrape_usage(
            &state,
            &ctx(),
            &formats,
            true,
            true,
            false,
            "https://e.com/x",
        )
        .await;
        let events = outbox.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "usage.recorded");
        assert_eq!(events[0].api_key_id.as_deref(), Some("k"));
        assert_eq!(
            events[0].data,
            serde_json::json!({
                "operation": "scrape",
                // f2ab8d2: scrape_credits([markdown, rawhtml], true, false) = 1 + 5.
                "credits": 6,
                "units": {"pages_http": 0, "pages_browser": 1, "ai_summary": 1, "ai_extraction": 0,
                          "feature_pages": 1},
                "provider_cost_micro_usd": 0,
                "description": "https://e.com/x",
            })
        );
    }

    #[tokio::test]
    async fn map_usage_event_counts_urls_found() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        record_map_usage(&state, &ctx(), "https://e.com", 17).await;
        let events = outbox.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].data,
            serde_json::json!({
                "operation": "map",
                "credits": 2,
                "units": {"requests": 1, "urls_found": 17},
                "provider_cost_micro_usd": 0,
                "description": "https://e.com",
            })
        );
    }

    #[tokio::test]
    async fn search_usage_event_counts_hits_and_defaults_to_zero() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        let hits = serde_json::json!({"hits": [{"id": 1}, {"id": 2}, {"id": 3}]});
        record_search_usage(&state, &ctx(), "https://e.com", "rust", &hits).await;
        record_search_usage(
            &state,
            &ctx(),
            "https://e.com",
            "rust",
            &serde_json::json!({}),
        )
        .await;
        let events = outbox.events();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].data,
            serde_json::json!({
                "operation": "search",
                "credits": 2,
                "units": {"requests": 1, "results": 3},
                "provider_cost_micro_usd": 0,
                "description": "https://e.com q=rust",
            })
        );
        assert_eq!(events[1].data["units"]["results"], 0);
        lab_events::assert_contract_valid(&events);
    }

    #[tokio::test]
    async fn auth_disabled_warning_is_rate_limited() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let t0 = Instant::now();
        assert!(!state.warn_auth_disabled(t0), "auth enabled: never warns");

        state.auth_disabled = true;
        assert!(state.warn_auth_disabled(t0));
        assert!(!state.warn_auth_disabled(t0 + Duration::from_secs(59)));
        assert!(state.warn_auth_disabled(t0 + Duration::from_secs(60)));
    }

    /// A Running job with a fresh accounting entry for `seeds` seeds.
    fn running_job(state: &AppState, job_id: &str, seeds: u64) {
        let mut job = JobState::new(job_id, "idx");
        job.start();
        state.insert_job(job);
        let mut acc = JobAccounting::default();
        acc.seeds_published = seeds;
        state
            .crawl
            .accounting
            .write()
            .insert(job_id.to_string(), acc);
    }

    /// A job store that records its job inserts and, for each, whether a
    /// seed URL had already reached the frontier topic.
    struct SeedOrderStore {
        frontier: scrapix_queue::ChannelConsumer,
        inserts: parking_lot::Mutex<Vec<(String, JobStatus, bool)>>,
    }

    #[async_trait::async_trait]
    impl job_store::JobStore for SeedOrderStore {
        fn backend(&self) -> &'static str {
            "test"
        }
        fn lab_outbox(&self) -> Arc<dyn lab_events::LabOutbox> {
            Arc::new(lab_events::MemoryOutbox::default())
        }
        async fn insert_job(&self, job: &JobState) -> Result<(), job_store::StoreError> {
            let seed_published = self
                .frontier
                .poll_one::<serde_json::Value>(Duration::from_millis(50))
                .await
                .ok()
                .flatten()
                .is_some();
            self.inserts
                .lock()
                .push((job.job_id.clone(), job.status.clone(), seed_published));
            Ok(())
        }
        async fn update_job_full(&self, _: &JobState) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn flush_job_counters(&self, _: &[JobState]) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn flush_job_accounting(
            &self,
            _: &[(String, serde_json::Value)],
        ) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn load_active_jobs(&self) -> Vec<JobState> {
            Vec::new()
        }
        async fn load_active_job_accounting(&self) -> Vec<(String, serde_json::Value)> {
            Vec::new()
        }
        async fn get_job(&self, _: &str, _: Option<&str>) -> Option<JobState> {
            None
        }
        async fn delete_job(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<bool, job_store::StoreError> {
            Ok(false)
        }
        async fn list_jobs(
            &self,
            _: Option<&str>,
            _: Option<&str>,
            _: i64,
            _: i64,
        ) -> Vec<JobState> {
            Vec::new()
        }
        async fn active_job_ids(&self, _: &str) -> Result<Vec<String>, job_store::StoreError> {
            Ok(Vec::new())
        }
        async fn store_result_page(
            &self,
            _: &str,
            _: u64,
            _: &str,
            _: bool,
            _: &serde_json::Value,
        ) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn store_result_summary(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn load_result_summary(
            &self,
            _: &str,
        ) -> Result<Option<serde_json::Value>, job_store::StoreError> {
            Ok(None)
        }
        async fn result_pages(
            &self,
            _: &str,
            _: u64,
            _: usize,
        ) -> Result<(Vec<(u64, serde_json::Value)>, u64), job_store::StoreError> {
            Ok((Vec::new(), 0))
        }
    }

    /// A crawl job's row is inserted (awaited) before its first seed URL is
    /// published: the counter flush's UPDATE for an event of the job must
    /// never land before the INSERT (it would change 0 rows and be lost).
    #[tokio::test]
    async fn crawl_job_is_persisted_before_its_first_seed_is_published() {
        let bus = ChannelBus::new();
        let frontier = bus.consumer();
        frontier.subscribe(&[topic_names::URL_FRONTIER]).unwrap();
        let store = Arc::new(SeedOrderStore {
            frontier,
            inserts: parking_lot::Mutex::new(Vec::new()),
        });
        let mut state = test_state(&bus);
        state.job_store = Some(store.clone());
        let state = Arc::new(state);
        let config: CrawlConfig = serde_json::from_value(serde_json::json!({
            "start_urls": ["https://a.test/"],
            "index_uid": "a",
            "meilisearch": {"url": "http://127.0.0.1:7700", "api_key": "k"}
        }))
        .unwrap();

        let created = do_create_crawl(&state, config, None).await.unwrap();

        let inserts = store.inserts.lock().clone();
        assert_eq!(
            inserts,
            vec![(created.job_id, JobStatus::Running, false)],
            "one insert, of the running job, before any seed was published"
        );
    }

    #[tokio::test]
    async fn health_services_reports_browser_availability() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let Json(body) = health_services(State(Arc::new(test_state(&bus)))).await;
        let body = serde_json::to_value(body).unwrap();
        assert_eq!(body["browser_available"], false);
        assert!(body["crawl_browser_available"].is_null(), "unknown");

        state.crawl_browser = Some(false);
        let Json(body) = health_services(State(Arc::new(state))).await;
        let body = serde_json::to_value(body).unwrap();
        assert_eq!(body["crawl_browser_available"], false);
    }

    #[tokio::test]
    async fn global_ws_only_serves_the_callers_jobs() {
        let bus = ChannelBus::new();
        let state = Arc::new(test_state(&bus));
        running_job(&state, "mine", 1);
        with_account(&state, "mine");
        running_job(&state, "theirs", 1);
        state.update_job("theirs", |j| {
            j.account_id = Some("7f1c2a8e-0000-4000-8000-0000000000ff".into())
        });
        let caller = Some(ctx());
        let subs: Arc<RwLock<HashSet<String>>> = Arc::default();
        let send = |job_id: &str, get: bool| {
            let msg = if get {
                WsClientMessage::GetStatus {
                    job_id: job_id.into(),
                }
            } else {
                WsClientMessage::Subscribe {
                    job_id: job_id.into(),
                }
            };
            handle_ws_message(msg, &state, &subs, &caller)
        };
        assert!(matches!(
            send("mine", false).await,
            WsServerMessage::Subscribed { .. }
        ));
        assert!(matches!(
            send("mine", true).await,
            WsServerMessage::Status { .. }
        ));
        for get in [false, true] {
            match send("theirs", get).await {
                WsServerMessage::Error { code, .. } => assert_eq!(code, "not_found"),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(*subs.read(), HashSet::from(["mine".to_string()]));
    }

    /// A state over a fresh SQLite job store.
    async fn sqlite_state(bus: &ChannelBus) -> (Arc<AppState>, Arc<dyn job_store::JobStore>) {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("j.db").display());
        std::mem::forget(dir); // keep the file for the test's lifetime
        let store: Arc<dyn job_store::JobStore> =
            Arc::new(job_store::SqliteJobStore::open(&url).await.unwrap());
        let mut state = test_state(bus);
        state.job_store = Some(store.clone());
        (Arc::new(state), store)
    }

    /// A job of `ACCT` with `status`, in memory and in the store.
    async fn stored_job(
        state: &AppState,
        store: &dyn job_store::JobStore,
        job_id: &str,
        status: JobStatus,
    ) {
        let mut job = JobState::with_account(job_id, "idx", ACCT);
        job.start();
        job.status = status;
        job.config = Some(serde_json::json!({"start_urls": ["https://a.test"]}));
        store.insert_job(&job).await.unwrap();
        store.update_job_full(&job).await.unwrap();
        state.insert_job(job);
    }

    fn tenant() -> Option<Extension<AuthenticatedAccount>> {
        Some(Extension(AuthenticatedAccount {
            account_id: ACCT.into(),
            tier: "free".into(),
            api_key_id: None,
            role: None,
            limits: None,
        }))
    }

    async fn purge(state: &Arc<AppState>, job_id: &str) -> Result<StatusCode, ApiError> {
        cancel_job(
            State(state.clone()),
            tenant(),
            Path(job_id.to_string()),
            Query(DeleteJobQuery { purge: true }),
        )
        .await
        .map(|r| r.status())
    }

    #[tokio::test]
    async fn purge_deletes_a_finished_job_everywhere() {
        let bus = ChannelBus::new();
        let (state, store) = sqlite_state(&bus).await;
        stored_job(&state, store.as_ref(), "done", JobStatus::Completed).await;
        store
            .store_result_page("done", 1, "https://a.test", true, &serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(purge(&state, "done").await.unwrap(), StatusCode::NO_CONTENT);
        assert!(state.get_job("done").is_none());
        assert!(store.get_job("done", None).await.is_none());
        assert_eq!(store.result_pages("done", 0, 10).await.unwrap().1, 0);
        // Gone: a second delete (or status) is a 404.
        assert_eq!(purge(&state, "done").await.unwrap_err().code, "not_found");
    }

    #[tokio::test]
    async fn purge_finds_jobs_only_in_the_store() {
        let bus = ChannelBus::new();
        let (state, store) = sqlite_state(&bus).await;
        stored_job(&state, store.as_ref(), "old", JobStatus::Failed).await;
        state.crawl.jobs.write().remove("old"); // e.g. after a restart
        assert_eq!(purge(&state, "old").await.unwrap(), StatusCode::NO_CONTENT);
        assert!(store.get_job("old", None).await.is_none());
    }

    #[tokio::test]
    async fn purge_refuses_unfinished_unowned_and_unsettled_jobs() {
        let bus = ChannelBus::new();
        let (state, store) = sqlite_state(&bus).await;
        stored_job(&state, store.as_ref(), "live", JobStatus::Running).await;
        let err = purge(&state, "live").await.unwrap_err();
        assert_eq!(err.code, "conflict");
        assert!(err.error.contains("cancel it"), "{}", err.error);
        assert!(state.get_job("live").is_some());

        stored_job(&state, store.as_ref(), "theirs", JobStatus::Completed).await;
        state.update_job("theirs", |j| {
            j.account_id = Some("7f1c2a8e-0000-4000-8000-0000000000ff".into())
        });
        assert_eq!(purge(&state, "theirs").await.unwrap_err().code, "not_found");
        assert!(state.get_job("theirs").is_some());

        stored_job(&state, store.as_ref(), "settling", JobStatus::Completed).await;
        let snapshot = state.get_job("settling").unwrap();
        state
            .crawl
            .terminal_pending
            .write()
            .insert("settling".into(), snapshot);
        let err = purge(&state, "settling").await.unwrap_err();
        assert_eq!(err.code, "conflict");
        assert_eq!(err.retry_after, Some(5));
    }

    #[tokio::test]
    async fn cancel_without_purge_still_cancels() {
        let bus = ChannelBus::new();
        let state = Arc::new(test_state(&bus));
        running_job(&state, "j", 1);
        with_account(&state, "j");
        let res = cancel_job(
            State(state.clone()),
            tenant(),
            Path("j".into()),
            Query(DeleteJobQuery { purge: false }),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(state.get_job("j").unwrap().status, JobStatus::Cancelled);
    }

    async fn list(
        state: &Arc<AppState>,
        query: serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, ApiError> {
        let query: ListJobsQuery = serde_json::from_value(query).unwrap();
        list_jobs(State(state.clone()), tenant(), Query(query))
            .await
            .map(|Json(jobs)| {
                jobs.into_iter()
                    .map(|j| serde_json::to_value(j).unwrap())
                    .collect()
            })
    }

    #[tokio::test]
    async fn list_jobs_filters_by_status_and_omits_configs() {
        let bus = ChannelBus::new();
        let (state, store) = sqlite_state(&bus).await;
        stored_job(&state, store.as_ref(), "a-done", JobStatus::Completed).await;
        stored_job(&state, store.as_ref(), "a-live", JobStatus::Running).await;

        let all = list(&state, serde_json::json!({})).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|j| j.get("config").is_none()), "{all:?}");

        let done = list(&state, serde_json::json!({"status": "completed"}))
            .await
            .unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["job_id"], "a-done");

        let page = list(&state, serde_json::json!({"limit": 1, "offset": 1}))
            .await
            .unwrap();
        assert_eq!(page.len(), 1);

        let err = list(&state, serde_json::json!({"status": "done"}))
            .await
            .unwrap_err();
        assert_eq!(err.code, "validation_error");

        // The status endpoint still has the config.
        let Json(status) = job_status(State(state.clone()), tenant(), Path("a-done".into()))
            .await
            .unwrap();
        assert!(status.config.is_some());
    }

    #[tokio::test]
    async fn list_jobs_without_a_store_is_scoped_filtered_and_bounded() {
        let bus = ChannelBus::new();
        let state = Arc::new(test_state(&bus));
        for i in 0..(MAX_LIST_JOBS_LIMIT + 5) {
            running_job(&state, &format!("j{i}"), 1);
            with_account(&state, &format!("j{i}"));
        }
        running_job(&state, "unowned", 1);
        let page = list(&state, serde_json::json!({"limit": 10_000}))
            .await
            .unwrap();
        assert_eq!(page.len(), MAX_LIST_JOBS_LIMIT);
        assert!(page.iter().all(|j| j["job_id"] != "unowned"));
        let none = list(&state, serde_json::json!({"status": "paused"}))
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    /// R10/SCR-22: `/metrics` reports `scrapix_api_jobs{status}` computed
    /// from the in-memory job map at scrape time.
    #[tokio::test]
    async fn metrics_route_reports_job_counts_by_status() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "job-running", 3);
        let mut done = JobState::new("job-done", "idx");
        done.status = JobStatus::Completed;
        state.insert_job(done);

        let response = metrics(State(Arc::new(state))).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(scrapix_core::metrics::CONTENT_TYPE)
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("scrapix_api_jobs"),
            "response must contain the scrapix_api_jobs gauge: {text}"
        );
        assert!(
            text.contains("status=\"running\"") && text.contains("} 1"),
            "expected a running=1 sample: {text}"
        );
        assert!(
            text.contains("status=\"completed\""),
            "expected a completed sample: {text}"
        );
    }

    /// Bounded poll for a wiremock server's request log to reach `expected`
    /// entries, instead of a fixed `sleep` and hoping it was long enough
    /// (SCR-72 fix round 1, item 6).
    async fn wait_until_received(server: &wiremock::MockServer, expected: usize) {
        for _ in 0..200 {
            if server.received_requests().await.unwrap().len() >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for {expected} requests, got {}",
            server.received_requests().await.unwrap().len()
        );
    }

    fn progress(job_id: &str, received: u64, dispatched: u64, queued: u64) -> CrawlEvent {
        CrawlEvent::FrontierProgress {
            job_id: job_id.into(),
            instance_id: "f1".into(),
            received,
            admitted: received,
            dispatched,
            rejected: 0,
            dropped: 0,
            queued,
            timestamp: received as i64,
        }
    }

    fn crawled(job_id: &str, id: &str) -> CrawlEvent {
        CrawlEvent::PageCrawled {
            job_id: job_id.into(),
            account_id: None,
            url: "https://a.test/".into(),
            status: 200,
            content_length: 100,
            duration_ms: 5,
            timestamp: 0,
            links_published: 0,
            url_message_id: id.into(),
            js_rendered: false,
            sitemap_pending: false,
        }
    }

    fn indexed(job_id: &str, id: &str) -> CrawlEvent {
        CrawlEvent::DocumentIndexed {
            job_id: job_id.into(),
            account_id: None,
            url: "https://a.test/".into(),
            document_id: "d".into(),
            timestamp: 0,
            url_message_id: id.into(),
            ai_enriched: false,
            ocr_pages: 0,
        }
    }

    fn failed(job_id: &str, id: &str) -> CrawlEvent {
        CrawlEvent::PageFailed {
            job_id: job_id.into(),
            account_id: None,
            url: "https://a.test/x".into(),
            error: "404 Not Found".into(),
            retry_count: 0,
            timestamp: 0,
            status: Some(404),
            url_message_id: id.into(),
        }
    }

    fn emails(state: &AppState) -> u64 {
        state
            .diagnostics
            .job_emails_requested
            .load(Ordering::Relaxed)
    }

    const ACCT: &str = "7f1c2a8e-0000-4000-8000-000000000001";

    /// Give `job_id` a (valid uuid) account, as hosted jobs have.
    fn with_account(state: &AppState, job_id: &str) {
        state.update_job(job_id, |j| j.account_id = Some(ACCT.to_string()));
    }

    fn events_of(outbox: &lab_events::MemoryOutbox, kind: &str) -> Vec<lab_events::LabEvent> {
        outbox
            .events()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }

    /// A job store recording each terminal write (`update_job_full`) and
    /// how many events the outbox held at that moment.
    #[derive(Default)]
    struct TerminalStore {
        outbox: Option<Arc<lab_events::MemoryOutbox>>,
        writes: parking_lot::Mutex<Vec<(String, JobStatus, usize)>>,
        /// Statuses received by `flush_job_counters`.
        counters: parking_lot::Mutex<Vec<(String, JobStatus)>>,
        /// What `active_job_ids` returns.
        active: parking_lot::Mutex<Vec<String>>,
    }

    impl TerminalStore {
        fn watching(outbox: &Arc<lab_events::MemoryOutbox>) -> Self {
            Self {
                outbox: Some(outbox.clone()),
                ..Default::default()
            }
        }

        fn writes_of(&self, job_id: &str) -> Vec<(JobStatus, usize)> {
            self.writes
                .lock()
                .iter()
                .filter(|(id, _, _)| id == job_id)
                .map(|(_, s, n)| (s.clone(), *n))
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl job_store::JobStore for TerminalStore {
        fn backend(&self) -> &'static str {
            "test"
        }
        fn lab_outbox(&self) -> Arc<dyn lab_events::LabOutbox> {
            Arc::new(lab_events::MemoryOutbox::default())
        }
        async fn insert_job(&self, _: &JobState) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn update_job_full(&self, job: &JobState) -> Result<(), job_store::StoreError> {
            let recorded = self.outbox.as_ref().map_or(0, |o| o.events().len());
            self.writes
                .lock()
                .push((job.job_id.clone(), job.status.clone(), recorded));
            Ok(())
        }
        async fn flush_job_counters(
            &self,
            snapshots: &[JobState],
        ) -> Result<(), job_store::StoreError> {
            self.counters.lock().extend(
                snapshots
                    .iter()
                    .map(|j| (j.job_id.clone(), j.status.clone())),
            );
            Ok(())
        }
        async fn flush_job_accounting(
            &self,
            _: &[(String, serde_json::Value)],
        ) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn load_active_jobs(&self) -> Vec<JobState> {
            Vec::new()
        }
        async fn load_active_job_accounting(&self) -> Vec<(String, serde_json::Value)> {
            Vec::new()
        }
        async fn get_job(&self, _: &str, _: Option<&str>) -> Option<JobState> {
            None
        }
        async fn delete_job(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<bool, job_store::StoreError> {
            Ok(false)
        }
        async fn list_jobs(
            &self,
            _: Option<&str>,
            _: Option<&str>,
            _: i64,
            _: i64,
        ) -> Vec<JobState> {
            Vec::new()
        }
        async fn active_job_ids(&self, _: &str) -> Result<Vec<String>, job_store::StoreError> {
            Ok(self.active.lock().clone())
        }
        async fn store_result_page(
            &self,
            _: &str,
            _: u64,
            _: &str,
            _: bool,
            _: &serde_json::Value,
        ) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn store_result_summary(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<(), job_store::StoreError> {
            Ok(())
        }
        async fn load_result_summary(
            &self,
            _: &str,
        ) -> Result<Option<serde_json::Value>, job_store::StoreError> {
            Ok(None)
        }
        async fn result_pages(
            &self,
            _: &str,
            _: u64,
            _: usize,
        ) -> Result<(Vec<(u64, serde_json::Value)>, u64), job_store::StoreError> {
            Ok((Vec::new(), 0))
        }
    }

    /// An outbox whose first `enqueue` fails (the Lab outbox is down once).
    struct FailingOnceOutbox {
        inner: Arc<lab_events::MemoryOutbox>,
        failed: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl lab_events::LabOutbox for FailingOnceOutbox {
        async fn enqueue(
            &self,
            events: &[lab_events::LabEvent],
        ) -> Result<(), job_store::StoreError> {
            if !self.failed.swap(true, Ordering::SeqCst) {
                return Err(job_store::StoreError::Other("down".into()));
            }
            self.inner.enqueue(events).await
        }
        async fn due(
            &self,
            limit: i64,
        ) -> Result<Vec<lab_events::LabEvent>, job_store::StoreError> {
            self.inner.due(limit).await
        }
        async fn mark_delivered(&self, ids: &[uuid::Uuid]) -> Result<(), job_store::StoreError> {
            self.inner.mark_delivered(ids).await
        }
        async fn reschedule(&self, ids: &[uuid::Uuid]) -> Result<(), job_store::StoreError> {
            self.inner.reschedule(ids).await
        }
        async fn abandon(&self, ids: &[uuid::Uuid]) -> Result<u64, job_store::StoreError> {
            self.inner.abandon(ids).await
        }
        async fn pending_stats(
            &self,
        ) -> Result<(i64, Option<chrono::DateTime<chrono::Utc>>), job_store::StoreError> {
            self.inner.pending_stats().await
        }
        async fn purge_delivered(
            &self,
            older_than_secs: i64,
        ) -> Result<u64, job_store::StoreError> {
            self.inner.purge_delivered(older_than_secs).await
        }
    }

    /// Drive `job_id` (one seed, one page) to a balanced, completed state.
    async fn complete_one_page_job(state: &AppState, job_id: &str) {
        running_job(state, job_id, 1);
        with_account(state, job_id);
        for e in [
            progress(job_id, 1, 1, 0),
            crawled(job_id, "m1"),
            indexed(job_id, "m1"),
        ] {
            state.process_event(job_id, &e);
        }
        state
            .finalize_job(job_id, Finalize::Complete, Instant::now())
            .await;
        assert_eq!(state.get_job(job_id).unwrap().status, JobStatus::Completed);
    }

    /// The events and the terminal write are recorded first, then the
    /// terminal status: a failed record leaves the terminal write owed, and
    /// the next flush records the events and only then writes it.
    #[tokio::test]
    async fn terminal_status_is_not_persisted_before_its_events_are_recorded() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = Arc::new(lab_events::MemoryOutbox::default());
        state.lab = Some(Arc::new(lab_events::Lab::new(Arc::new(
            FailingOnceOutbox {
                inner: outbox.clone(),
                failed: std::sync::atomic::AtomicBool::new(false),
            },
        ))));
        let store = Arc::new(TerminalStore::watching(&outbox));
        state.job_store = Some(store.clone());

        complete_one_page_job(&state, "j1").await;
        // Give a (wrongly) spawned direct terminal write a chance to run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(store.writes_of("j1").is_empty(), "no terminal write yet");

        state.flush_to_db(store.as_ref()).await; // enqueue fails
        assert!(outbox.events().is_empty());
        assert!(
            store.writes_of("j1").is_empty(),
            "terminal status must not be persisted before its events"
        );

        state.flush_to_db(store.as_ref()).await; // enqueue succeeds
        assert_eq!(
            store.writes_of("j1"),
            vec![(JobStatus::Completed, 2)],
            "one terminal write, after both events were recorded"
        );
        assert_eq!(events_of(&outbox, "usage.recorded").len(), 1);
        assert_eq!(events_of(&outbox, "job.completed").len(), 1);

        state.flush_to_db(store.as_ref()).await;
        assert_eq!(store.writes_of("j1").len(), 1, "nothing owed any more");
        assert_eq!(outbox.events().len(), 2);
    }

    fn failing_once_lab(state: &mut AppState) -> Arc<lab_events::MemoryOutbox> {
        let outbox = Arc::new(lab_events::MemoryOutbox::default());
        state.lab = Some(Arc::new(lab_events::Lab::new(Arc::new(
            FailingOnceOutbox {
                inner: outbox.clone(),
                failed: std::sync::atomic::AtomicBool::new(false),
            },
        ))));
        outbox
    }

    /// Fix round 1 (1): a job still (or again) dirty when it is terminal in
    /// memory — e.g. a PageCrawled racing a cancel re-marks it dirty — never
    /// reaches the counter flush with its terminal status: that write would
    /// persist the terminal status before the job's events are recorded.
    #[tokio::test]
    async fn counter_flush_never_carries_a_terminal_status() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = failing_once_lab(&mut state);
        let store = Arc::new(TerminalStore::watching(&outbox));
        running_job(&state, "j1", 2);
        with_account(&state, "j1");
        state.process_event("j1", &progress("j1", 2, 2, 0));
        state.process_event("j1", &crawled("j1", "m1"));
        state.cancel("j1").expect("running job cancels");
        // The consumer applied a late event of the job and re-marked it.
        state.crawl.dirty_jobs.write().insert("j1".to_string());

        state.flush_to_db(store.as_ref()).await; // outbox down
        assert!(outbox.events().is_empty());
        assert!(store.writes_of("j1").is_empty(), "no terminal write yet");
        assert!(
            store.counters.lock().iter().all(|(_, s)| !is_terminal(s)),
            "counter flush got a terminal status: {:?}",
            store.counters.lock()
        );

        state.crawl.dirty_jobs.write().insert("j1".to_string());
        state.flush_to_db(store.as_ref()).await; // outbox back
        assert_eq!(store.writes_of("j1"), vec![(JobStatus::Cancelled, 1)]);
        assert!(store.counters.lock().iter().all(|(_, s)| !is_terminal(s)));
    }

    /// Fix round 1 (2): a job terminal in memory whose terminal write is
    /// still owed (its row still reads active, e.g. the outbox is down) does
    /// not count toward the concurrent-job quota.
    #[tokio::test]
    async fn quota_ignores_jobs_terminal_in_memory_with_their_write_owed() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let _outbox = failing_once_lab(&mut state);
        let store = Arc::new(TerminalStore::default());
        state.job_store = Some(store.clone());

        complete_one_page_job(&state, "done").await;
        state.flush_to_db(store.as_ref()).await; // outbox down: still owed
        assert!(store.writes_of("done").is_empty());
        running_job(&state, "live", 1);
        // The store still reads "done" as active; "other" is unknown here.
        *store.active.lock() = vec!["done".into(), "live".into(), "other".into()];

        assert_eq!(state.active_job_count(ACCT).await, 2);

        // Evicted from memory with the write still owed: still not counted.
        state.crawl.jobs.write().remove("done");
        assert_eq!(state.active_job_count(ACCT).await, 2);
    }

    /// Fix round 1 (2): an events-gated terminal write wakes the flush loop.
    #[tokio::test]
    async fn owed_terminal_write_wakes_the_flush_loop() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let _outbox = with_memory_lab(&mut state);
        complete_one_page_job(&state, "j1").await;
        tokio::time::timeout(
            Duration::from_millis(100),
            state.crawl.terminal_flush_wake.notified(),
        )
        .await
        .expect("flush loop woken");
    }

    /// A job finalized again (as after a crash between the event record and
    /// the terminal write: recovered as running, finalized by the normal
    /// paths) produces the same event ids: still one charge and one email.
    #[tokio::test]
    async fn refinalizing_a_job_does_not_duplicate_its_events() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        let store = TerminalStore::watching(&outbox);

        complete_one_page_job(&state, "j1").await;
        state.flush_to_db(&store).await;
        assert_eq!(outbox.events().len(), 2);

        // Recovery: the job is back as running with its accounting.
        complete_one_page_job(&state, "j1").await;
        state.flush_to_db(&store).await;

        assert_eq!(
            store.writes_of("j1").len(),
            2,
            "both finalizations persisted"
        );
        let usage = events_of(&outbox, "usage.recorded");
        assert_eq!(usage.len(), 1);
        assert_eq!(
            usage[0].id,
            lab_events::LabEvent::crawl_final_usage(
                "j1",
                ACCT,
                0,
                serde_json::json!({}),
                String::new()
            )
            .id
        );
        assert_eq!(events_of(&outbox, "job.completed").len(), 1);
        assert_eq!(outbox.events().len(), 2);
        assert_eq!(emails(&state), 2, "diagnostics count each finalization");
    }

    /// A terminal event for a job this process does not know is still
    /// charged and emailed (as before): recorded without a terminal write.
    #[tokio::test]
    async fn terminal_event_for_an_unknown_job_records_its_events() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        state.process_event(
            "ghost",
            &CrawlEvent::JobCompleted {
                job_id: "ghost".into(),
                account_id: Some(ACCT.into()),
                pages_crawled: 2,
                documents_indexed: 2,
                errors: 0,
                bytes_downloaded: 0,
                duration_secs: 1,
                timestamp: 0,
            },
        );
        for _ in 0..100 {
            if outbox.events().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(events_of(&outbox, "usage.recorded").len(), 1);
        assert_eq!(events_of(&outbox, "job.completed").len(), 1);
        assert!(state.crawl.pending_lab_events.lock().is_empty());
        assert!(state.crawl.terminal_pending.read().is_empty());
    }

    /// Standalone (no Lab): nothing is recorded, the job still finalizes and
    /// is persisted, and the billing diagnostics are computed as before.
    #[tokio::test]
    async fn standalone_job_records_nothing_and_still_finalizes() {
        let bus = ChannelBus::new();
        let store = Arc::new(TerminalStore::default());
        let state = AppState::new(
            AnyProducer::channel(bus.producer()),
            test_config(),
            None,
            None,
            None,
            None,
            test_fetcher(),
            None,
            None,
            None,
            Some(store.clone() as Arc<dyn job_store::JobStore>),
            None,
            test_webhooks(),
        );
        assert!(state.lab.is_none());

        complete_one_page_job(&state, "j1").await;
        state.flush_to_db(store.as_ref()).await;

        let writes = store.writes_of("j1");
        assert!(!writes.is_empty(), "terminal status persisted");
        assert!(writes.iter().all(|(s, _)| *s == JobStatus::Completed));
        assert!(state.crawl.pending_lab_events.lock().is_empty());
        let d = &state.diagnostics;
        assert_eq!(d.job_bills_requested.load(Ordering::Relaxed), 1);
        assert_eq!(emails(&state), 1);
    }

    #[tokio::test]
    async fn balanced_job_completes_once_after_grace_with_one_email() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        running_job(&state, "j1", 1);
        with_account(&state, "j1");

        for e in [
            progress("j1", 1, 1, 0),
            crawled("j1", "m1"),
            crawled("j1", "m1"), // redelivered: not double counted
            indexed("j1", "m1"),
        ] {
            state.process_event("j1", &e);
        }
        let job = state.get_job("j1").unwrap();
        assert_eq!(job.pages_crawled, 1, "PageCrawled is deduplicated");
        assert_eq!(job.pages_indexed, 1);
        assert_eq!(job.bytes_downloaded, 100);

        let t0 = Instant::now();
        assert!(
            state.completion_decisions(t0).is_empty(),
            "balanced streak just started: wait for the grace period"
        );
        let decisions = state.completion_decisions(t0 + Duration::from_secs(4));
        assert_eq!(decisions, vec![("j1".to_string(), Finalize::Complete)]);

        state
            .finalize_job("j1", Finalize::Complete, Instant::now())
            .await;
        let job = state.get_job("j1").unwrap();
        assert_eq!(job.status, JobStatus::Completed);
        assert_eq!(job.pages_crawled, 1);
        assert_eq!(emails(&state), 1);

        // Accounting (and its seen-sets) is freed once terminal.
        assert!(state.crawl.accounting.read().get("j1").is_none());
        assert!(state.crawl.balanced_since.read().get("j1").is_none());

        // The pipeline is told to release the job.
        let ctl: JobControl = control
            .poll_one(Duration::from_secs(1))
            .await
            .unwrap()
            .expect("JobControl published");
        assert_eq!(ctl.job_id, "j1");
        assert_eq!(ctl.action, JobAction::Finish);

        // Neither the loop nor a redelivered terminal event emails again.
        assert!(state
            .completion_decisions(t0 + Duration::from_secs(10))
            .is_empty());
        state
            .finalize_job("j1", Finalize::Complete, Instant::now())
            .await;
        state.process_event(
            "j1",
            &CrawlEvent::JobCompleted {
                job_id: "j1".into(),
                account_id: None,
                pages_crawled: 1,
                documents_indexed: 1,
                errors: 0,
                bytes_downloaded: 0,
                duration_secs: 1,
                timestamp: 0,
            },
        );
        state.process_event(
            "j1",
            &CrawlEvent::JobFailed {
                job_id: "j1".into(),
                account_id: None,
                error: "late".into(),
                timestamp: 0,
            },
        );
        assert_eq!(emails(&state), 1);
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Completed);

        state.flush_to_db(&TerminalStore::default()).await;
        assert_eq!(
            outbox
                .events()
                .iter()
                .filter(|e| e.kind == "job.completed")
                .count(),
            1
        );
        assert!(events_of(&outbox, "job.failed").is_empty());
    }

    #[tokio::test]
    async fn unbalanced_streak_resets_the_grace_period() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        state.process_event("j1", &progress("j1", 1, 1, 0));
        state.process_event("j1", &crawled("j1", "m1"));
        state.process_event("j1", &indexed("j1", "m1"));

        let t0 = Instant::now();
        assert!(state.completion_decisions(t0).is_empty());
        // New work shows up (link discovered and queued): streak broken.
        state.process_event("j1", &progress("j1", 2, 1, 1));
        assert!(state
            .completion_decisions(t0 + Duration::from_secs(2))
            .is_empty());
        state.process_event("j1", &progress("j1", 2, 2, 0));
        state.process_event("j1", &failed("j1", "m2"));
        // Balanced again at t0+3s: the grace restarts from there.
        assert!(state
            .completion_decisions(t0 + Duration::from_secs(3))
            .is_empty());
        assert!(state
            .completion_decisions(t0 + Duration::from_secs(5))
            .is_empty());
        assert_eq!(
            state.completion_decisions(t0 + Duration::from_secs(6)),
            vec![("j1".to_string(), Finalize::Complete)]
        );
    }

    #[tokio::test]
    async fn all_pages_failed_job_fails_with_no_pages() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 2);
        for e in [
            progress("j1", 2, 2, 0),
            failed("j1", "m1"),
            failed("j1", "m2"),
        ] {
            state.process_event("j1", &e);
        }
        assert_eq!(state.get_job("j1").unwrap().errors, 2);

        let t0 = Instant::now();
        assert!(state.completion_decisions(t0).is_empty());
        let decisions = state.completion_decisions(t0 + Duration::from_secs(4));
        assert_eq!(decisions, vec![("j1".to_string(), Finalize::FailNoPages)]);
        state
            .finalize_job("j1", Finalize::FailNoPages, Instant::now())
            .await;

        let job = state.get_job("j1").unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(
            job.error_message.as_deref(),
            Some("No page could be crawled (2 failures)")
        );
        assert_eq!(emails(&state), 1);
    }

    #[tokio::test]
    async fn silent_unbalanced_job_fails_as_stalled() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        state.process_event("j1", &progress("j1", 1, 1, 0)); // never finishes

        let now = Instant::now();
        assert!(state.completion_decisions(now).is_empty());
        let later = now + Duration::from_secs(1801);
        assert_eq!(
            state.completion_decisions(later),
            vec![("j1".to_string(), Finalize::FailStalled)]
        );
        state.finalize_job("j1", Finalize::FailStalled, later).await;
        let job = state.get_job("j1").unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(
            job.error_message.as_deref(),
            Some("Stalled: no progress for 1800s")
        );
        assert_eq!(emails(&state), 1);
    }

    #[tokio::test]
    async fn cancelled_job_is_never_finalized() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        state.update_job("j1", |j| j.status = JobStatus::Cancelled);
        state.forget_job_tracking("j1");
        assert!(state
            .completion_decisions(Instant::now() + Duration::from_secs(3600))
            .is_empty());
        state
            .finalize_job(
                "j1",
                Finalize::FailStalled,
                Instant::now() + Duration::from_secs(3600),
            )
            .await;
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Cancelled);
        assert_eq!(emails(&state), 0);
    }

    #[tokio::test]
    async fn job_warnings_are_deduplicated_and_exposed_in_status() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        for msg in ["proxy ignored", "proxy ignored", "pdf disabled", ""] {
            state.process_event(
                "j1",
                &CrawlEvent::JobWarning {
                    job_id: "j1".into(),
                    message: msg.into(),
                    timestamp: 0,
                },
            );
        }
        let job = state.get_job("j1").unwrap();
        assert_eq!(job.warnings, vec!["proxy ignored", "pdf disabled"]);
        let status = serde_json::to_value(JobStatusResponse::from(job)).unwrap();
        assert_eq!(
            status["warnings"],
            serde_json::json!(["proxy ignored", "pdf disabled"])
        );

        // Omitted when empty.
        running_job(&state, "j2", 1);
        let status =
            serde_json::to_value(JobStatusResponse::from(state.get_job("j2").unwrap())).unwrap();
        assert!(status.get("warnings").is_none());
    }

    #[tokio::test]
    async fn late_events_for_a_finished_job_do_not_resurrect_tracking() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        state
            .finalize_job(
                "j1",
                Finalize::FailStalled,
                Instant::now() + Duration::from_secs(3600),
            )
            .await;
        state.process_event("j1", &crawled("j1", "m9"));
        assert!(state.crawl.accounting.read().get("j1").is_none());
        assert!(state.crawl.job_last_activity.read().get("j1").is_none());
    }

    #[test]
    fn ai_usage_event_maps_to_clickhouse_row() {
        let event = CrawlEvent::AiUsage {
            job_id: "j1".into(),
            account_id: None,
            provider: "openai".into(),
            model: "gpt".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            duration_ms: 42,
            feature: "ai_summary".into(),
            url: "https://a.test/".into(),
            timestamp: 1_700_000_000_000,
        };
        let row = crawl_event_to_ai_usage("j1", &event, Some("acct".into())).unwrap();
        assert_eq!(row.provider, "openai");
        assert_eq!(row.operation, "ai_summary");
        assert_eq!(row.total_tokens, 15);
        assert_eq!(row.duration_ms, 42);
        assert_eq!(row.job_id, "j1");
        assert_eq!(row.account_id, "acct", "falls back to the job's account");
        assert_eq!(row.url, "https://a.test/");
        assert_eq!(row.timestamp.unix_timestamp(), 1_700_000_000);
        assert!(crawl_event_to_ai_usage("j1", &crawled("j1", "m"), None).is_none());
    }

    #[test]
    fn restored_accounting_falls_back_to_seed_count() {
        let mut job = JobState::new("j1", "idx");
        job.start_urls = vec!["https://a.test".into(), "https://b.test".into()];
        let acc = restore_accounting(&job, Some(&serde_json::json!({})));
        assert_eq!(acc.seeds_published, 2);
        let acc = restore_accounting(&job, None);
        assert_eq!(acc.seeds_published, 2);

        let mut persisted = JobAccounting::default();
        persisted.seeds_published = 5;
        persisted.pages_crawled_ok = 3;
        let acc = restore_accounting(&job, Some(&serde_json::to_value(&persisted).unwrap()));
        assert_eq!(acc.seeds_published, 5);
        assert_eq!(acc.pages_crawled_ok, 3);
    }

    fn at(offset: i64) -> Option<EventPosition> {
        Some(EventPosition {
            partition: 3,
            offset,
        })
    }

    fn counting_ack(counter: &Arc<std::sync::atomic::AtomicUsize>) -> Ack {
        let c = counter.clone();
        Ack::from_fn(move || {
            c.fetch_add(1, Ordering::Relaxed);
        })
    }

    /// Simulate a restart: a fresh state whose job accounting is restored
    /// from `persisted` (the jsonb snapshot of the crashed instance).
    fn restored_state(bus: &ChannelBus, job_id: &str, persisted: &serde_json::Value) -> AppState {
        let state = test_state(bus);
        let mut job = JobState::new(job_id, "idx");
        job.start();
        let acc = restore_accounting(&job, Some(persisted));
        state.insert_job(job);
        state
            .crawl
            .accounting
            .write()
            .insert(job_id.to_string(), acc);
        state
    }

    fn persisted(state: &AppState, job_id: &str) -> serde_json::Value {
        state
            .accounting_snapshots(&[job_id.to_string()])
            .pop()
            .unwrap()
            .1
    }

    /// R-19(c): replaying events at or below the restored high-water mark
    /// does not double count; events past it still apply.
    #[tokio::test]
    async fn replayed_events_after_restore_are_not_double_counted() {
        let bus = ChannelBus::new();
        let before = test_state(&bus);
        running_job(&before, "j1", 2);
        let events = [
            progress("j1", 2, 2, 0),
            crawled("j1", "m1"),
            indexed("j1", "m1"),
        ];
        for (i, e) in events.iter().enumerate() {
            before.process_event_at("j1", e, at(10 + i as i64));
        }
        let snapshot = persisted(&before, "j1");
        assert_eq!(
            snapshot["event_hwm"],
            serde_json::json!({"partition": 3, "offset": 12})
        );

        let after = restored_state(&bus, "j1", &snapshot);
        for (i, e) in events.iter().enumerate() {
            let outcome = after.process_event_at("j1", e, at(10 + i as i64));
            assert!(!outcome.accounting_touched, "replayed event {i} re-applied");
        }
        {
            let accs = after.crawl.accounting.read();
            let acc = accs.get("j1").unwrap();
            assert_eq!(acc.crawl_outcomes, 1);
            assert_eq!(acc.pages_crawled_ok, 1);
            assert_eq!(acc.content_outcomes, 1);
        }
        // New events past the mark apply.
        let outcome = after.process_event_at("j1", &failed("j1", "m2"), at(13));
        assert!(outcome.accounting_touched);
        assert!(after.crawl.accounting.read()["j1"].is_balanced());
    }

    /// R-19(c): without the high-water mark, replaying the tail (m1 crawled
    /// + indexed) after a restore would count m1 twice and balance a job
    /// whose second seed is still outstanding.
    #[tokio::test]
    async fn restored_job_does_not_complete_early_from_replayed_tail() {
        let bus = ChannelBus::new();
        let before = test_state(&bus);
        running_job(&before, "j1", 2);
        before.process_event_at("j1", &progress("j1", 2, 2, 0), at(1));
        before.process_event_at("j1", &crawled("j1", "m1"), at(2));
        before.process_event_at("j1", &indexed("j1", "m1"), at(3));
        let after = restored_state(&bus, "j1", &persisted(&before, "j1"));

        after.process_event_at("j1", &crawled("j1", "m1"), at(2));
        after.process_event_at("j1", &indexed("j1", "m1"), at(3));
        let t0 = Instant::now();
        assert!(after.completion_decisions(t0).is_empty());
        assert!(
            after
                .completion_decisions(t0 + Duration::from_secs(10))
                .is_empty(),
            "m2 is still outstanding"
        );
        assert!(!after.crawl.accounting.read()["j1"].is_balanced());
    }

    fn persist_on(state: &AppState) {
        // as with a job store
        state.accounting_persisted.store(true, Ordering::Relaxed);
    }

    fn ok_flush(state: &AppState) {
        let batch = state.begin_flush();
        state.finish_flush(batch, AccountingFlush::Ok, &HashSet::new());
    }

    /// R-19(b): an accounting event's ack is held until the accounting flush
    /// containing it succeeds; a failed flush keeps it held (and the job
    /// dirty) for the next round. Other events are acked at once.
    #[tokio::test]
    async fn accounting_event_acks_wait_for_a_successful_flush() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        persist_on(&state);
        running_job(&state, "j1", 1);
        let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let outcome = state.process_event_at("j1", &progress("j1", 1, 1, 0), at(1));
        state.settle_ack("j1", counting_ack(&acked), outcome).await;
        let outcome = state.process_event_at("j1", &crawled("j1", "m1"), at(2));
        state.settle_ack("j1", counting_ack(&acked), outcome).await;
        assert_eq!(acked.load(Ordering::Relaxed), 0, "held until flushed");

        // Non-accounting and replayed events are acked immediately.
        let warning = CrawlEvent::JobWarning {
            job_id: "j1".into(),
            message: "w".into(),
            timestamp: 0,
        };
        let outcome = state.process_event_at("j1", &warning, at(3));
        state.settle_ack("j1", counting_ack(&acked), outcome).await;
        let outcome = state.process_event_at("j1", &crawled("j1", "m1"), at(2));
        state.settle_ack("j1", counting_ack(&acked), outcome).await;
        assert_eq!(acked.load(Ordering::Relaxed), 2);

        // Failed accounting flush: nothing acked, job still dirty.
        let batch = state.begin_flush();
        assert_eq!(batch.acks.len(), 2);
        assert_eq!(batch.accounting.len(), 1);
        state.finish_flush(batch, AccountingFlush::Retry, &HashSet::new());
        assert_eq!(acked.load(Ordering::Relaxed), 2);
        assert!(state.crawl.dirty_jobs.read().contains("j1"));

        // Successful retry: both held acks released.
        let batch = state.begin_flush();
        assert_eq!(batch.accounting.len(), 1, "retried with the job's snapshot");
        state.finish_flush(batch, AccountingFlush::Ok, &HashSet::new());
        assert_eq!(acked.load(Ordering::Relaxed), 4);
        assert!(state.crawl.pending_acks.lock().is_empty());
    }

    #[tokio::test]
    async fn without_persistence_acks_are_immediate() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        assert!(!state.accounting_persisted());
        running_job(&state, "j1", 1);
        let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let outcome = state.process_event_at("j1", &crawled("j1", "m1"), at(1));
        assert!(outcome.accounting_touched);
        state.settle_ack("j1", counting_ack(&acked), outcome).await;
        assert_eq!(acked.load(Ordering::Relaxed), 1);
    }

    /// Round 2 (1a): held acks are capped; at the cap `settle_ack` blocks
    /// (backpressure) until a successful flush frees room.
    #[tokio::test]
    async fn held_ack_cap_blocks_until_a_flush_frees_room() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        state.config.max_pending_acks = 2;
        persist_on(&state);
        let state = Arc::new(state);
        running_job(&state, "j1", 1);
        let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for (i, id) in ["m1", "m2"].iter().enumerate() {
            let outcome = state.process_event_at("j1", &crawled("j1", id), at(i as i64));
            state.settle_ack("j1", counting_ack(&acked), outcome).await;
        }
        assert_eq!(state.crawl.pending_acks.lock().len(), 2);

        let outcome = state.process_event_at("j1", &crawled("j1", "m3"), at(2));
        let blocked = {
            let state = state.clone();
            let ack = counting_ack(&acked);
            tokio::spawn(async move { state.settle_ack("j1", ack, outcome).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!blocked.is_finished(), "settle_ack must wait at the cap");
        assert_eq!(state.crawl.pending_acks.lock().len(), 2);

        ok_flush(&state); // frees room
        tokio::time::timeout(Duration::from_secs(2), blocked)
            .await
            .expect("unblocked by the flush")
            .unwrap();
        assert_eq!(acked.load(Ordering::Relaxed), 2);
        assert_eq!(
            state.crawl.pending_acks.lock().len(),
            1,
            "third ack now held"
        );
    }

    /// Round 3: acks taken by an in-progress flush still count against the
    /// cap, and a failed flush can never push the held total over it.
    #[tokio::test]
    async fn in_flight_flush_acks_count_against_the_cap() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        state.config.max_pending_acks = 2;
        persist_on(&state);
        let state = Arc::new(state);
        running_job(&state, "j1", 1);
        let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for (i, id) in ["m1", "m2"].iter().enumerate() {
            let o = state.process_event_at("j1", &crawled("j1", id), at(i as i64));
            state.settle_ack("j1", counting_ack(&acked), o).await;
        }
        assert_eq!(state.held_acks(), 2);

        // A flush is in progress against a down DB.
        let batch = state.begin_flush();
        assert!(state.crawl.pending_acks.lock().is_empty());
        assert_eq!(state.held_acks(), 2, "in-flight acks still held");
        let o = state.process_event_at("j1", &crawled("j1", "m3"), at(2));
        let blocked = {
            let state = state.clone();
            let ack = counting_ack(&acked);
            tokio::spawn(async move { state.settle_ack("j1", ack, o).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !blocked.is_finished(),
            "cap applies while the flush is in flight"
        );

        state.finish_flush(batch, AccountingFlush::Retry, &HashSet::new());
        assert!(state.held_acks() <= 2, "Retry never grows past the cap");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!blocked.is_finished(), "still at the cap after Retry");
        assert_eq!(acked.load(Ordering::Relaxed), 0);

        // Repeated failing rounds stay bounded.
        for _ in 0..3 {
            let batch = state.begin_flush();
            state.finish_flush(batch, AccountingFlush::Retry, &HashSet::new());
            assert!(state.held_acks() <= 2);
        }

        ok_flush(&state);
        tokio::time::timeout(Duration::from_secs(2), blocked)
            .await
            .expect("unblocked by a successful flush")
            .unwrap();
        assert_eq!(acked.load(Ordering::Relaxed), 2);
        assert_eq!(state.held_acks(), 1);
    }

    /// Round 3: acks retained for a failed terminal write stay counted, so
    /// the cap still holds on that path.
    #[tokio::test]
    async fn acks_retained_for_terminal_writes_count_against_the_cap() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        state.config.max_pending_acks = 2;
        persist_on(&state);
        let state = Arc::new(state);
        running_job(&state, "j1", 1);
        let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for (i, id) in ["m1", "m2"].iter().enumerate() {
            let o = state.process_event_at("j1", &crawled("j1", id), at(i as i64));
            state.settle_ack("j1", counting_ack(&acked), o).await;
        }
        state
            .finalize_job(
                "j1",
                Finalize::FailStalled,
                Instant::now() + Duration::from_secs(3600),
            )
            .await;

        running_job(&state, "j2", 1);
        let o = state.process_event_at("j2", &crawled("j2", "m1"), at(5));
        let blocked = {
            let state = state.clone();
            let ack = counting_ack(&acked);
            tokio::spawn(async move { state.settle_ack("j2", ack, o).await })
        };
        let failed: HashSet<String> = ["j1".to_string()].into();
        for _ in 0..3 {
            let batch = state.begin_flush();
            state.finish_flush(batch, AccountingFlush::Ok, &failed);
            assert!(state.held_acks() <= 2, "retained acks stay within the cap");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!blocked.is_finished());
        assert_eq!(acked.load(Ordering::Relaxed), 0);

        ok_flush(&state); // terminal write succeeds
        tokio::time::timeout(Duration::from_secs(2), blocked)
            .await
            .expect("unblocked")
            .unwrap();
        assert_eq!(acked.load(Ordering::Relaxed), 2);
        assert!(state.held_acks() <= 2);
    }

    #[tokio::test]
    async fn held_ack_cap_releases_on_shutdown_without_acking() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        state.config.max_pending_acks = 1;
        persist_on(&state);
        running_job(&state, "j1", 1);
        let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let o = state.process_event_at("j1", &crawled("j1", "m1"), at(1));
        state.settle_ack("j1", counting_ack(&acked), o).await;
        state.shutting_down.store(true, Ordering::Relaxed);
        let o = state.process_event_at("j1", &crawled("j1", "m2"), at(2));
        tokio::time::timeout(
            Duration::from_secs(2),
            state.settle_ack("j1", counting_ack(&acked), o),
        )
        .await
        .expect("shutdown unblocks");
        assert_eq!(
            acked.load(Ordering::Relaxed),
            0,
            "dropped un-acked: redelivered"
        );
    }

    /// Round 2 (1b): a missing `accounting` column is not retryable: ack
    /// deferral and accounting persistence are disabled and held acks are
    /// released.
    #[tokio::test]
    async fn schema_missing_flush_disables_deferral_and_releases_acks() {
        assert_eq!(
            classify_flush_error(&job_store::StoreError::Other("timeout".into())),
            AccountingFlush::Retry
        );
        assert_eq!(
            classify_flush_error(&job_store::StoreError::SchemaMissing("x".into())),
            AccountingFlush::SchemaMissing
        );

        let bus = ChannelBus::new();
        let state = test_state(&bus);
        persist_on(&state);
        running_job(&state, "j1", 1);
        let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let o = state.process_event_at("j1", &crawled("j1", "m1"), at(1));
        state.settle_ack("j1", counting_ack(&acked), o).await;
        let batch = state.begin_flush();
        // An ack held after the batch was taken is released too.
        let o = state.process_event_at("j1", &crawled("j1", "m2"), at(2));
        state.settle_ack("j1", counting_ack(&acked), o).await;
        state.finish_flush(batch, AccountingFlush::SchemaMissing, &HashSet::new());
        assert_eq!(acked.load(Ordering::Relaxed), 2);
        assert!(!state.accounting_persisted());

        // From then on: immediate acks, no accounting snapshots flushed.
        let o = state.process_event_at("j1", &crawled("j1", "m3"), at(3));
        state.settle_ack("j1", counting_ack(&acked), o).await;
        assert_eq!(acked.load(Ordering::Relaxed), 3);
        assert!(state.begin_flush().accounting.is_empty());
    }

    /// Round 2 (2): a job that went terminal keeps its held acks until the
    /// owed checked terminal write succeeded; a failed write is retried.
    #[tokio::test]
    async fn terminal_job_acks_wait_for_its_terminal_write() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        persist_on(&state);
        running_job(&state, "j1", 1);
        running_job(&state, "j2", 1);
        let acked1 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let acked2 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let o = state.process_event_at("j1", &crawled("j1", "m1"), at(1));
        state.settle_ack("j1", counting_ack(&acked1), o).await;
        let o = state.process_event_at("j2", &crawled("j2", "m1"), at(2));
        state.settle_ack("j2", counting_ack(&acked2), o).await;

        state
            .finalize_job(
                "j1",
                Finalize::FailStalled,
                Instant::now() + Duration::from_secs(3600),
            )
            .await;
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Failed);

        let batch = state.begin_flush();
        assert_eq!(batch.terminal.len(), 1);
        assert_eq!(batch.terminal[0].job_id, "j1");
        let failed: HashSet<String> = ["j1".to_string()].into();
        state.finish_flush(batch, AccountingFlush::Ok, &failed);
        assert_eq!(acked1.load(Ordering::Relaxed), 0, "terminal write failed");
        assert_eq!(acked2.load(Ordering::Relaxed), 1, "other jobs unaffected");

        let batch = state.begin_flush();
        assert_eq!(batch.terminal.len(), 1, "terminal write retried");
        state.finish_flush(batch, AccountingFlush::Ok, &HashSet::new());
        assert_eq!(acked1.load(Ordering::Relaxed), 1);
        assert!(state.begin_flush().terminal.is_empty());
    }

    /// Round 2 (3): a NoPages/Complete decision is re-validated against the
    /// crawled-page count.
    #[tokio::test]
    async fn no_pages_decision_is_not_applied_once_a_page_was_crawled() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        for e in [
            progress("j1", 1, 1, 0),
            crawled("j1", "m1"),
            indexed("j1", "m1"),
        ] {
            state.process_event("j1", &e);
        }
        state
            .finalize_job("j1", Finalize::FailNoPages, Instant::now())
            .await;
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Running);

        running_job(&state, "j2", 1);
        for e in [progress("j2", 1, 1, 0), failed("j2", "m1")] {
            state.process_event("j2", &e);
        }
        state
            .finalize_job("j2", Finalize::Complete, Instant::now())
            .await;
        assert_eq!(state.get_job("j2").unwrap().status, JobStatus::Running);
    }

    /// R-20: a stalled job that crawled pages is billed for them.
    #[tokio::test]
    async fn stalled_job_with_crawled_pages_is_billed() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        running_job(&state, "j1", 2);
        with_account(&state, "j1");
        state.process_event("j1", &progress("j1", 2, 2, 0));
        state.process_event("j1", &crawled("j1", "m1"));
        state.process_event("j1", &indexed("j1", "m1")); // m2 never reports
        let later = Instant::now() + Duration::from_secs(1801);
        assert_eq!(
            state.completion_decisions(later),
            vec![("j1".to_string(), Finalize::FailStalled)]
        );
        state.finalize_job("j1", Finalize::FailStalled, later).await;
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Failed);
        let d = &state.diagnostics;
        assert_eq!(d.job_bills_requested.load(Ordering::Relaxed), 1);
        assert_eq!(d.pages_billed.load(Ordering::Relaxed), 1);

        let store = TerminalStore::watching(&outbox);
        state.flush_to_db(&store).await;
        let usage = events_of(&outbox, "usage.recorded");
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].data["operation"], "crawl");
        assert_eq!(usage[0].data["units"]["pages_http"], 1);
        assert_eq!(usage[0].data["units"]["feature_pages"], 0);
        // f2ab8d2: crawl_credits(1, 0, 0, no features) = 1.
        assert_eq!(usage[0].data["credits"], 1);
        assert_eq!(usage[0].data["job_id"], "j1");
        assert_eq!(events_of(&outbox, "job.failed").len(), 1);
        assert_eq!(
            store.writes_of("j1"),
            vec![(JobStatus::Failed, 2)],
            "the stall charge is recorded before the terminal write"
        );
    }

    #[tokio::test]
    async fn completed_job_is_billed_once_and_no_pages_job_not_at_all() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "ok", 1);
        for e in [
            progress("ok", 1, 1, 0),
            crawled("ok", "m1"),
            indexed("ok", "m1"),
        ] {
            state.process_event("ok", &e);
        }
        running_job(&state, "none", 1);
        for e in [progress("none", 1, 1, 0), failed("none", "m1")] {
            state.process_event("none", &e);
        }
        let t0 = Instant::now();
        state.completion_decisions(t0);
        let mut decisions = state.completion_decisions(t0 + Duration::from_secs(4));
        decisions.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, d) in decisions {
            state.finalize_job(&id, d, Instant::now()).await;
        }
        state
            .finalize_job("ok", Finalize::Complete, Instant::now())
            .await;
        let d = &state.diagnostics;
        assert_eq!(d.job_bills_requested.load(Ordering::Relaxed), 1);
        assert_eq!(d.pages_billed.load(Ordering::Relaxed), 1);
    }

    /// D4/R4: a completed job with a mix of plain-HTTP, browser-rendered and
    /// AI-enriched pages reports exactly the units that were delivered — not
    /// the whole job as browser pages, and no AI units for pages that were
    /// never AI-enriched. The Lab prices the units.
    #[tokio::test]
    async fn completed_job_with_mixed_delivery_reports_delivered_units() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        running_job(&state, "mix", 3);
        with_account(&state, "mix");
        let (job_id, acct) = ("mix".to_string(), ACCT.to_string());

        // AI features enabled on the job: `pages_ai` must still count only
        // the pages that were actually AI-enriched.
        let features = scrapix_core::FeaturesConfig::from_cli_args(
            false,
            false,
            false,
            false,
            true,
            true,
            Some("extract".to_string()),
        );
        state.update_job("mix", |j| {
            j.config = Some(serde_json::json!({ "features": features }));
        });

        state.process_event("mix", &progress("mix", 3, 3, 0));
        // m1: plain HTTP, not AI-enriched.
        state.process_event(
            "mix",
            &CrawlEvent::PageCrawled {
                job_id: "mix".into(),
                account_id: None,
                url: "https://a.test/1".into(),
                status: 200,
                content_length: 0,
                duration_ms: 0,
                timestamp: 0,
                links_published: 0,
                url_message_id: "m1".into(),
                js_rendered: false,
                sitemap_pending: false,
            },
        );
        // m2 and m3: browser-rendered; only m3 is AI-enriched.
        for id in ["m2", "m3"] {
            state.process_event(
                "mix",
                &CrawlEvent::PageCrawled {
                    job_id: "mix".into(),
                    account_id: None,
                    url: format!("https://a.test/{id}"),
                    status: 200,
                    content_length: 0,
                    duration_ms: 0,
                    timestamp: 0,
                    links_published: 0,
                    url_message_id: id.into(),
                    js_rendered: true,
                    sitemap_pending: false,
                },
            );
        }
        for (id, ai_enriched) in [("m1", false), ("m2", false), ("m3", true)] {
            state.process_event(
                "mix",
                &CrawlEvent::DocumentIndexed {
                    job_id: "mix".into(),
                    account_id: None,
                    url: format!("https://a.test/{id}"),
                    document_id: format!("d-{id}"),
                    timestamp: 0,
                    url_message_id: id.into(),
                    ai_enriched,
                    ocr_pages: 0,
                },
            );
        }

        state
            .finalize_job("mix", Finalize::Complete, Instant::now())
            .await;
        assert_eq!(state.get_job("mix").unwrap().status, JobStatus::Completed);

        let d = &state.diagnostics;
        assert_eq!(d.job_bills_requested.load(Ordering::Relaxed), 1);
        // 1 http page + 2 browser pages delivered.
        assert_eq!(d.pages_billed.load(Ordering::Relaxed), 3);

        state.flush_to_db(&TerminalStore::default()).await;
        let usage: Vec<_> = outbox
            .events()
            .into_iter()
            .filter(|e| e.kind == "usage.recorded")
            .collect();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].data["operation"], "crawl");
        assert_eq!(
            usage[0].data["units"],
            serde_json::json!({"pages_http": 1, "pages_browser": 2, "pages_ai": 1, "pages_ocr": 0,
                               "feature_pages": 0})
        );
        // f2ab8d2: 1 http (1 credit) + 2 browser (2 credits each = 4) + 1
        // AI-enriched page (10 credits surcharge) = 15. Not 3 * 12 = 36.
        assert_eq!(usage[0].data["credits"], 15);
        lab_events::assert_contract_valid(&usage);
        assert_eq!(
            usage[0].data["units"]["pages_http"].as_u64().unwrap()
                + usage[0].data["units"]["pages_browser"].as_u64().unwrap(),
            3
        );
        assert_eq!(usage[0].data["units"]["pages_ai"], 1);
        assert_eq!(
            usage[0].data["description"],
            "Job mix (1 http + 2 browser pages, 1 AI-enriched)"
        );
        assert_eq!(usage[0].account_id, acct);
        assert_eq!(
            usage[0].id,
            lab_events::LabEvent::crawl_final_usage(
                &job_id,
                &acct,
                0,
                serde_json::json!({}),
                String::new()
            )
            .id
        );
    }

    /// Transition release: a crawl with non-AI features and OCR'd pages
    /// carries the pre-v2 `credits` and reports the per-feature surcharge
    /// as `feature_pages` = pages x enabled non-AI features.
    #[tokio::test]
    async fn crawl_usage_carries_pre_v2_credits_and_feature_pages() {
        let bus = ChannelBus::new();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        running_job(&state, "feat", 3);
        with_account(&state, "feat");
        // metadata + markdown: 2 non-AI features.
        let features = scrapix_core::FeaturesConfig::from_cli_args(
            true, true, false, false, false, false, None,
        );
        state.update_job("feat", |j| {
            j.config = Some(serde_json::json!({ "features": features }));
        });
        state.process_event("feat", &progress("feat", 3, 3, 0));
        for (id, js) in [("m1", false), ("m2", false), ("m3", true)] {
            state.process_event(
                "feat",
                &CrawlEvent::PageCrawled {
                    job_id: "feat".into(),
                    account_id: None,
                    url: format!("https://a.test/{id}"),
                    status: 200,
                    content_length: 0,
                    duration_ms: 0,
                    timestamp: 0,
                    links_published: 0,
                    url_message_id: id.into(),
                    js_rendered: js,
                    sitemap_pending: false,
                },
            );
        }
        for (id, ocr_pages) in [("m1", 0), ("m2", 2), ("m3", 0)] {
            state.process_event(
                "feat",
                &CrawlEvent::DocumentIndexed {
                    job_id: "feat".into(),
                    account_id: None,
                    url: format!("https://a.test/{id}"),
                    document_id: format!("d-{id}"),
                    timestamp: 0,
                    url_message_id: id.into(),
                    ai_enriched: false,
                    ocr_pages,
                },
            );
        }
        state
            .finalize_job("feat", Finalize::Complete, Instant::now())
            .await;
        state.flush_to_db(&TerminalStore::default()).await;
        let usage = events_of(&outbox, "usage.recorded");
        assert_eq!(usage.len(), 1);
        assert_eq!(
            usage[0].data["units"],
            serde_json::json!({"pages_http": 2, "pages_browser": 1, "pages_ai": 0, "pages_ocr": 2,
                               "feature_pages": 6})
        );
        // f2ab8d2: crawl_credits(2, 1, 0, 2 features) = 2 * (1 + 2) +
        // 1 * (2 + 2) = 10, + ocr_credits(2) = 10: 20.
        assert_eq!(usage[0].data["credits"], 20);
        lab_events::assert_contract_valid(&outbox.events());
    }

    /// A decision computed up front is re-validated right before finalizing
    /// (and before any destructive Replace cleanup).
    #[tokio::test]
    async fn stale_complete_decision_is_skipped() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        for e in [
            progress("j1", 1, 1, 0),
            crawled("j1", "m1"),
            indexed("j1", "m1"),
        ] {
            state.process_event("j1", &e);
        }
        let t0 = Instant::now();
        state.completion_decisions(t0);
        let decisions = state.completion_decisions(t0 + Duration::from_secs(4));
        assert_eq!(decisions, vec![("j1".to_string(), Finalize::Complete)]);
        // New work lands before this job's turn in the finalize batch.
        state.process_event("j1", &progress("j1", 2, 1, 1));
        state
            .finalize_job("j1", Finalize::Complete, Instant::now())
            .await;
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Running);
        assert_eq!(emails(&state), 0);

        // A stall decision is re-validated too: activity resumed.
        state
            .finalize_job("j1", Finalize::FailStalled, Instant::now())
            .await;
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Running);
    }

    /// The terminal check-and-set is atomic: only the first terminal event
    /// transitions; a finalize racing a cancel publishes nothing.
    #[tokio::test]
    async fn terminal_transition_happens_once_and_cancel_wins() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        let fail = CrawlEvent::JobFailed {
            job_id: "j1".into(),
            account_id: None,
            error: "x".into(),
            timestamp: 0,
        };
        assert!(matches!(
            state.transition_terminal("j1", &fail),
            TerminalTransition::Applied(_)
        ));
        assert!(matches!(
            state.transition_terminal("j1", &fail),
            TerminalTransition::AlreadyTerminal
        ));
        assert!(!state.process_event("j1", &fail).applied);

        running_job(&state, "j2", 1);
        state.update_job("j2", |j| j.status = JobStatus::Cancelled);
        state
            .finalize_job(
                "j2",
                Finalize::FailStalled,
                Instant::now() + Duration::from_secs(3600),
            )
            .await;
        let got: Option<JobControl> = control.poll_one(Duration::from_millis(300)).await.unwrap();
        assert!(
            got.is_none(),
            "no JobControl for a job that did not transition"
        );
    }

    fn bills(state: &AppState) -> (u64, u64) {
        let d = &state.diagnostics;
        (
            d.job_bills_requested.load(Ordering::Relaxed),
            d.pages_billed.load(Ordering::Relaxed),
        )
    }

    async fn next_control(c: &scrapix_queue::ChannelConsumer) -> Option<JobControl> {
        c.poll_one(Duration::from_secs(1)).await.unwrap()
    }

    /// R5: cancelling a running job bills the pages it crawled, exactly
    /// once, and tells the pipeline to stop it.
    #[tokio::test]
    async fn cancel_bills_crawled_pages_once_and_stops_the_pipeline() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let mut state = test_state(&bus);
        let outbox = with_memory_lab(&mut state);
        running_job(&state, "j1", 5);
        with_account(&state, "j1");
        state.process_event("j1", &progress("j1", 5, 5, 0));
        for id in ["m1", "m2", "m3"] {
            state.process_event("j1", &crawled("j1", id));
        }
        state.process_event("j1", &crawled("j1", "m3")); // redelivered

        let job = state.cancel("j1").expect("running job cancels");
        assert_eq!(job.status, JobStatus::Cancelled);
        assert!(job.completed_at.is_some());
        assert_eq!(bills(&state), (1, 3), "one billing call for 3 pages");
        assert!(state.crawl.accounting.read().get("j1").is_none());
        let ctl = next_control(&control).await.expect("JobControl published");
        assert_eq!((ctl.job_id.as_str(), ctl.action), ("j1", JobAction::Cancel));

        // A second cancel is rejected and bills nothing more.
        assert_eq!(
            state.cancel("j1").unwrap_err(),
            ControlError::Conflict(JobStatus::Cancelled)
        );
        assert_eq!(bills(&state), (1, 3));
        assert!(next_control(&control).await.is_none());
        // Nor does the completion loop touch it.
        assert!(state
            .completion_decisions(Instant::now() + Duration::from_secs(3600))
            .is_empty());
        assert_eq!(emails(&state), 0);

        assert_eq!(state.cancel("nope").unwrap_err(), ControlError::NotFound);

        let store = TerminalStore::watching(&outbox);
        state.flush_to_db(&store).await;
        let usage = events_of(&outbox, "usage.recorded");
        assert_eq!(usage.len(), 1, "one charge");
        assert_eq!(usage[0].data["units"]["pages_http"], 3);
        // f2ab8d2: crawl_credits(3, 0, 0, no features) = 3.
        assert_eq!(usage[0].data["credits"], 3);
        assert_eq!(outbox.events().len(), 1, "no lifecycle email for a cancel");
        assert_eq!(store.writes_of("j1"), vec![(JobStatus::Cancelled, 1)]);
    }

    /// SCR-72: cancelling a job doesn't go through the pipeline's
    /// `CrawlEvent` stream, so `cancel()` must fire a synthetic
    /// `crawl_failed` webhook (`data.error == "cancelled"`) itself for any
    /// hook subscribed to `CrawlFailed`. Strengthened (fix round 1, item 6)
    /// to assert the actual delivered payload/headers, not just that some
    /// request arrived.
    #[tokio::test]
    async fn cancel_fires_crawl_failed_cancelled() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        state.update_job("j1", |j| {
            j.webhooks = vec![scrapix_core::WebhookConfig {
                url: format!("{}/hook", server.uri()),
                events: vec![scrapix_core::WebhookEvent::CrawlFailed],
                auth: None,
                enabled: true,
                timeout_ms: 5_000,
                name: None,
            }];
        });

        state.cancel("j1").expect("running job cancels");

        wait_until_received(&server, 1).await;
        let reqs = server.received_requests().await.unwrap();
        let req = &reqs[0];
        assert_eq!(
            req.headers.get("X-Scrapix-Event").unwrap(),
            "crawl_failed",
            "cancellation must be delivered as a crawl_failed webhook"
        );
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["event"], "crawl_failed");
        assert_eq!(body["job_id"], "j1");
        assert_eq!(
            body["data"]["error"], "cancelled",
            "the synthetic JobFailed's error field must say why: cancellation"
        );
    }

    /// SCR-72 fix round 1, item 6: a real `JobCompleted` delivered through
    /// `process_event_at` (not `enqueue` called directly) reaches a
    /// subscribed webhook — end-to-end coverage of the
    /// `process_event_at` -> `webhook_dispatcher.enqueue` wiring itself,
    /// which the other webhook tests don't exercise (they call `enqueue`
    /// directly, or go through `cancel()` which also calls it directly).
    #[tokio::test]
    async fn job_completed_event_reaches_a_subscribed_webhook() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        state.update_job("j1", |j| {
            j.webhooks = vec![scrapix_core::WebhookConfig {
                url: format!("{}/hook", server.uri()),
                events: vec![scrapix_core::WebhookEvent::CrawlCompleted],
                auth: None,
                enabled: true,
                timeout_ms: 5_000,
                name: None,
            }];
        });
        state.process_event("j1", &progress("j1", 1, 1, 0));
        state.process_event("j1", &crawled("j1", "m1"));
        state.process_event("j1", &indexed("j1", "m1"));

        let completed = CrawlEvent::JobCompleted {
            job_id: "j1".to_string(),
            account_id: None,
            pages_crawled: 1,
            documents_indexed: 1,
            errors: 0,
            bytes_downloaded: 10,
            duration_secs: 1,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        assert!(state.process_event("j1", &completed).applied);

        wait_until_received(&server, 1).await;
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(
            reqs[0].headers.get("X-Scrapix-Event").unwrap(),
            "crawl_completed"
        );
    }

    /// Cancel after the job completed is rejected: the terminal status is
    /// not overwritten and the job is not billed a second time.
    #[tokio::test]
    async fn cancel_after_complete_is_rejected_and_not_rebilled() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        for e in [
            progress("j1", 1, 1, 0),
            crawled("j1", "m1"),
            indexed("j1", "m1"),
        ] {
            state.process_event("j1", &e);
        }
        state
            .finalize_job("j1", Finalize::Complete, Instant::now())
            .await;
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Completed);
        assert_eq!(bills(&state), (1, 1));

        assert_eq!(
            state.cancel("j1").unwrap_err(),
            ControlError::Conflict(JobStatus::Completed)
        );
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Completed);
        assert_eq!(bills(&state), (1, 1), "not billed again");
    }

    /// Pause/resume: Running <-> Paused only (409 otherwise); a paused job
    /// is neither finalized nor stall-failed, and resuming restarts its
    /// stall clock.
    #[tokio::test]
    async fn pause_and_resume_transitions_and_conflicts() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let state = test_state(&bus);
        running_job(&state, "j1", 2);
        state.process_event("j1", &progress("j1", 2, 2, 0));
        state.process_event("j1", &crawled("j1", "m1")); // m2 still in flight
        let long_ago = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .expect("process older than 1h is not required: use a smaller offset");
        state
            .crawl
            .job_last_activity
            .write()
            .insert("j1".into(), long_ago);

        assert_eq!(
            state.resume("j1").unwrap_err(),
            ControlError::Conflict(JobStatus::Running)
        );
        let job = state.pause("j1").unwrap();
        assert_eq!(job.status, JobStatus::Paused);
        let ctl = next_control(&control).await.expect("Pause published");
        assert_eq!(ctl.action, JobAction::Pause);
        assert_eq!(
            state.pause("j1").unwrap_err(),
            ControlError::Conflict(JobStatus::Paused)
        );
        assert!(
            state.completion_decisions(Instant::now()).is_empty(),
            "a paused job is not stall-failed"
        );

        let job = state.resume("j1").unwrap();
        assert_eq!(job.status, JobStatus::Running);
        let ctl = next_control(&control).await.expect("Resume published");
        assert_eq!(ctl.action, JobAction::Resume);
        assert!(
            state.completion_decisions(Instant::now()).is_empty(),
            "resume restarts the stall clock"
        );
        assert_eq!(state.get_job("j1").unwrap().status, JobStatus::Running);

        // A paused job can still be cancelled (and is billed).
        state.pause("j1").unwrap();
        assert_eq!(state.cancel("j1").unwrap().status, JobStatus::Cancelled);
        assert_eq!(bills(&state), (1, 1));
        assert_eq!(
            state.pause("j1").unwrap_err(),
            ControlError::Conflict(JobStatus::Cancelled)
        );
        assert_eq!(
            state.resume("j1").unwrap_err(),
            ControlError::Conflict(JobStatus::Cancelled)
        );
        assert_eq!(state.pause("nope").unwrap_err(), ControlError::NotFound);
    }

    /// R-22: a pipeline event for a job the API has stopped means the
    /// pipeline missed the control: re-publish it (rate-limited per job).
    #[tokio::test]
    async fn events_for_a_cancelled_job_republish_cancel_rate_limited() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let state = test_state(&bus);
        running_job(&state, "j1", 2);
        state.process_event("j1", &progress("j1", 2, 2, 0));
        state.process_event("j1", &crawled("j1", "m1"));
        state.cancel("j1").unwrap();
        assert_eq!(
            next_control(&control).await.unwrap().action,
            JobAction::Cancel
        );

        // The pipeline keeps crawling: re-publish Cancel, once per window.
        state.process_event("j1", &crawled("j1", "m2"));
        let ctl = next_control(&control).await.expect("Cancel re-published");
        assert_eq!((ctl.job_id.as_str(), ctl.action), ("j1", JobAction::Cancel));
        state.process_event("j1", &crawled("j1", "m3"));
        assert!(next_control(&control).await.is_none(), "rate-limited");
        let later = Instant::now() + Duration::from_secs(11);
        state.heal_control("j1", &crawled("j1", "m4"), later);
        assert_eq!(
            next_control(&control).await.unwrap().action,
            JobAction::Cancel
        );

        // Late pages are neither counted nor billed.
        let job = state.get_job("j1").unwrap();
        assert_eq!(job.pages_crawled, 1, "counters frozen at cancel");
        assert_eq!(bills(&state), (1, 1));
    }

    #[tokio::test]
    async fn events_for_a_finished_job_republish_finish() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);
        for e in [
            progress("j1", 1, 1, 0),
            crawled("j1", "m1"),
            indexed("j1", "m1"),
        ] {
            state.process_event("j1", &e);
        }
        state
            .finalize_job("j1", Finalize::Complete, Instant::now())
            .await;
        assert_eq!(
            next_control(&control).await.unwrap().action,
            JobAction::Finish
        );
        state.process_event("j1", &crawled("j1", "m9"));
        assert_eq!(
            next_control(&control).await.unwrap().action,
            JobAction::Finish
        );
        assert_eq!(state.get_job("j1").unwrap().pages_crawled, 1);
    }

    /// A paused job's in-flight pages report for a while: only events
    /// after a 5 s grace re-publish Pause.
    #[tokio::test]
    async fn events_for_a_paused_job_republish_pause_after_grace() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let state = test_state(&bus);
        running_job(&state, "j1", 2);
        state.pause("j1").unwrap();
        assert_eq!(
            next_control(&control).await.unwrap().action,
            JobAction::Pause
        );
        state.process_event("j1", &crawled("j1", "m1"));
        assert!(next_control(&control).await.is_none(), "within the grace");
        let later = Instant::now() + Duration::from_secs(6);
        state.heal_control("j1", &crawled("j1", "m2"), later);
        assert_eq!(
            next_control(&control).await.unwrap().action,
            JobAction::Pause
        );
        // A running job never triggers anything.
        state.resume("j1").unwrap();
        assert_eq!(
            next_control(&control).await.unwrap().action,
            JobAction::Resume
        );
        state.heal_control("j1", &crawled("j1", "m3"), later + Duration::from_secs(60));
        assert!(next_control(&control).await.is_none());
    }

    /// Final review fix 2a: controls are published in the order they were
    /// requested (a Pause and the Resume after it never swap), and a
    /// shutdown drain waits for the queue to empty.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn controls_are_published_in_request_order() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let state = test_state(&bus);
        let action = |i: usize| {
            if i.is_multiple_of(2) {
                JobAction::Pause
            } else {
                JobAction::Resume
            }
        };
        for i in 0..200 {
            state.publish_control(&format!("j{}", i / 2), action(i));
        }
        assert!(
            state.drain_controls(Duration::from_secs(5)).await,
            "queue drained"
        );
        for i in 0..200 {
            let ctl = next_control(&control).await.expect("control published");
            assert_eq!(
                (ctl.job_id.clone(), ctl.action),
                (format!("j{}", i / 2), action(i)),
                "control #{i} out of order"
            );
        }
        assert!(next_control(&control).await.is_none());
    }

    /// Final review fix 2b: a Running job that is not balanced and has had
    /// no event for `resume_heal_after` may have lost its Resume: re-publish
    /// Resume, at most once per `CONTROL_REPUBLISH_EVERY`.
    #[tokio::test]
    async fn silent_running_job_gets_one_resume_per_window() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let state = test_state(&bus);
        running_job(&state, "j1", 2);
        state.process_event("j1", &progress("j1", 2, 2, 0));
        state.process_event("j1", &crawled("j1", "m1")); // m2 in flight
                                                         // A balanced job is never nudged.
        running_job(&state, "done", 1);
        for e in [
            progress("done", 1, 1, 0),
            crawled("done", "m1"),
            indexed("done", "m1"),
        ] {
            state.process_event("done", &e);
        }
        let t0 = Instant::now();
        for id in ["j1", "done"] {
            state.crawl.job_last_activity.write().insert(id.into(), t0);
        }

        state.heal_silent_running(t0 + Duration::from_secs(30));
        assert!(next_control(&control).await.is_none(), "below threshold");

        state.heal_silent_running(t0 + Duration::from_secs(61));
        let ctl = next_control(&control).await.expect("Resume re-published");
        assert_eq!((ctl.job_id.as_str(), ctl.action), ("j1", JobAction::Resume));
        state.heal_silent_running(t0 + Duration::from_secs(65));
        assert!(next_control(&control).await.is_none(), "rate-limited");
        state.heal_silent_running(t0 + Duration::from_secs(72));
        let ctl = next_control(&control).await.expect("next window");
        assert_eq!((ctl.job_id.as_str(), ctl.action), ("j1", JobAction::Resume));
        assert!(next_control(&control).await.is_none(), "exactly one");
    }

    #[test]
    fn control_errors_map_to_404_and_409() {
        let not_found: ApiError = ControlError::NotFound.into();
        assert_eq!(not_found.into_response().status(), StatusCode::NOT_FOUND);
        let conflict: ApiError = ControlError::Conflict(JobStatus::Completed).into();
        assert_eq!(conflict.code, "conflict");
        assert!(conflict.error.contains("completed"), "{}", conflict.error);
        assert_eq!(conflict.into_response().status(), StatusCode::CONFLICT);
    }

    #[test]
    fn every_event_variant_maps_to_its_job() {
        assert_eq!(event_job_id(&crawled("jx", "m")), "jx");
        assert_eq!(event_job_id(&progress("jy", 1, 1, 0)), "jy");
    }
}
