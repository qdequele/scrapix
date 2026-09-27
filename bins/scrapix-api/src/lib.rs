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
pub mod auth;
pub mod billing;
pub mod completion;
pub mod configs;
pub mod documents;
pub mod email_scheduler;
pub mod jobs_db;
pub mod openapi;
pub mod stripe;
pub mod webhooks;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension, Path, Query, State,
    },
    http::StatusCode,
    middleware,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use clap::Parser;
use futures::{stream::Stream, SinkExt, StreamExt as FuturesStreamExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};
use tracing::{debug, error, info, warn};

use scrapix_ai::{AiClient, AiService, FieldDefinition as AiFieldDefinition, SchemaBuilder};
use scrapix_core::{
    ConcurrencyConfig, CrawlConfig, CrawlUrl, CrawlerType, FeaturesConfig, JobSpec, JobState,
    JobStatus,
};
use scrapix_crawler::{
    is_non_page_url, CdpRenderer, CdpRendererBuilder, HttpFetcher, HttpFetcherBuilder, RobotsCache,
    RobotsConfig, SitemapParser, WaitUntil,
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

    /// JWT secret for session tokens (required when DATABASE_URL is set)
    #[arg(long, env = "JWT_SECRET")]
    pub jwt_secret: Option<String>,

    /// Stripe secret key (enables engine-side auto-topup charges)
    #[arg(long, env = "STRIPE_SECRET_KEY")]
    pub stripe_secret_key: Option<String>,

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
}

/// Diagnostics: errors, domain stats, service health
struct DiagnosticsState {
    /// Recent errors ring buffer (for diagnostics)
    recent_errors: RwLock<VecDeque<ErrorRecord>>,
    /// Per-domain counters (for diagnostics)
    domain_counters: RwLock<HashMap<String, DomainCounter>>,
    /// Last time each service type was seen (for health monitoring)
    service_last_seen: RwLock<HashMap<String, std::time::Instant>>,
    /// Number of job completion/failure emails requested (one per terminal
    /// job; test hook for the single-email invariant, R5)
    job_emails_requested: std::sync::atomic::AtomicU64,
    /// Number of job billing requests and total pages billed (test hook /
    /// observability; one request per billed terminal job)
    job_bills_requested: std::sync::atomic::AtomicU64,
    pages_billed: std::sync::atomic::AtomicU64,
    /// Total crawl credits computed by `bill_job` (test hook / observability;
    /// incremented even without a DB pool configured, so tests can assert on
    /// the computed amount without a live Postgres — D4/R4).
    credits_billed: std::sync::atomic::AtomicI64,
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
    /// PostgreSQL connection pool (for saved configs, cron scheduling)
    db_pool: Option<sqlx::PgPool>,
    /// Optional email client for transactional emails
    /// Optional Stripe client for payment-backed auto-topup
    stripe_client: Option<::stripe::Client>,
    /// Optional ClickHouse analytics store (used for event history queries)
    analytics_store: Option<Arc<analytics::AnalyticsState>>,
    /// Delivers `CrawlEvent`s to jobs' subscribed webhooks (SCR-72).
    webhook_dispatcher: webhooks::WebhookDispatcher,
    /// Accounting is persisted and event acks are deferred until the flush
    /// (true when Postgres is configured; turned off for the process if the
    /// `accounting` column turns out to be missing, see `finish_flush`).
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
        db_pool: Option<sqlx::PgPool>,
        stripe_client: Option<::stripe::Client>,
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
            },
            diagnostics: DiagnosticsState {
                recent_errors: RwLock::new(VecDeque::with_capacity(1000)),
                domain_counters: RwLock::new(HashMap::new()),
                service_last_seen: RwLock::new(HashMap::new()),
                job_emails_requested: std::sync::atomic::AtomicU64::new(0),
                job_bills_requested: std::sync::atomic::AtomicU64::new(0),
                pages_billed: std::sync::atomic::AtomicU64::new(0),
                credits_billed: std::sync::atomic::AtomicI64::new(0),
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
            accounting_persisted: std::sync::atomic::AtomicBool::new(db_pool.is_some()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            control_tx,
            control_rx: parking_lot::Mutex::new(Some(control_rx)),
            controls_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            db_pool,
            stripe_client,
            analytics_store,
            webhook_dispatcher,
        }
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
    /// for the Postgres flush.
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

    /// List all jobs
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
    /// Postgres immediately and free its per-job tracking state.
    fn on_terminal(&self, job_id: &str, updated: Option<JobState>) {
        self.forget_job_tracking(job_id);
        if let Some(snapshot) = updated {
            self.write_terminal(snapshot);
        }
    }

    /// Write a terminal job through to Postgres now (best effort, for
    /// latency) and, while acks are deferred, owe a checked write to the
    /// next flush: the job's held acks wait for it.
    fn write_terminal(&self, snapshot: JobState) {
        let job_id = snapshot.job_id.clone();
        self.crawl.dirty_jobs.write().remove(&job_id);
        if self.accounting_persisted() {
            self.crawl
                .terminal_pending
                .write()
                .insert(job_id, snapshot.clone());
        }
        if let Some(ref pool) = self.db_pool {
            let pool = pool.clone();
            tokio::spawn(async move {
                let _ = jobs_db::update_job_full(&pool, &snapshot).await;
            });
        }
    }

    fn accounting_persisted(&self) -> bool {
        self.accounting_persisted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Queue a job notification email (delivered by the Rails app). Called
    /// only from `process_event`'s terminal branches, which run at most once
    /// per job.
    fn request_job_email(
        &self,
        email_type: &'static str,
        account_id: Option<String>,
        payload: serde_json::Value,
    ) {
        self.diagnostics
            .job_emails_requested
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (Some(pool), Some(acct_id)) = (self.db_pool.clone(), account_id) else {
            return;
        };
        tokio::spawn(async move {
            if let Ok(uuid) = uuid::Uuid::parse_str(&acct_id) {
                if let Some(email_addr) =
                    email_scheduler::get_account_email_for_job_notification(&pool, uuid).await
                {
                    email_scheduler::schedule_email_now(&pool, email_type, &email_addr, payload)
                        .await;
                }
            }
        });
    }

    /// One completion-loop tick (R5): refresh each Running job's balanced
    /// streak and return the jobs that must be finalized now.
    fn completion_decisions(&self, now: std::time::Instant) -> Vec<(String, Finalize)> {
        let running: HashSet<String> = self
            .crawl
            .jobs
            .read()
            .iter()
            .filter(|(_, j)| matches!(j.status, JobStatus::Running))
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
                        .map_or(true, |last| {
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

        info!(job_id = %job_id, ?decision, "Finalizing job from work accounting");
        // The terminal transition is atomic in process_event: if the job was
        // cancelled meanwhile (e.g. during the Replace cleanup), nothing is
        // applied and nothing else happens here.
        if !self.process_event(job_id, &event).applied {
            debug!(job_id = %job_id, "Job became terminal before finalize, skipping");
            return;
        }
        self.broadcast_event(job_id, event);

        // R-20: a stalled job still pays for the pages it crawled (the old
        // idle detector completed, and so billed, such jobs). A Replace
        // cleanup failure stays unbilled, as before.
        if decision == Finalize::FailStalled {
            self.bill_job(
                job_id,
                job.account_id.as_ref(),
                acc.pages_crawled_ok.saturating_sub(acc.pages_browser),
                acc.pages_browser,
                acc.pages_ai,
                acc.pages_ocr,
            );
        }

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

    /// Deduct crawl credits for `pages` pages of a finished job
    /// (fire-and-forget). Cost per page depends on the job's crawler_type and
    /// enabled features. The single billing path for terminal jobs.
    /// D4/R4: credits are computed from what was actually delivered, not
    /// from the job's static config — `pages_http`/`pages_browser` split the
    /// crawled-ok page count by whether each page was actually rendered
    /// with a browser (`PageCrawled.js_rendered`), and `pages_ai` counts
    /// only pages that were actually AI-enriched (`DocumentIndexed.ai_enriched`),
    /// regardless of whether the job merely had AI features enabled.
    /// `pages_ocr` (OCR'd document pages, `DocumentIndexed.ocr_pages`) adds
    /// the OCR page surcharge.
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
        if total_pages == 0 {
            return;
        }
        self.diagnostics
            .job_bills_requested
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.diagnostics
            .pages_billed
            .fetch_add(total_pages, std::sync::atomic::Ordering::Relaxed);

        // Extract features from persisted job config (crawler_type is no
        // longer needed here: base rate now follows the per-page split
        // above, not the job's declared crawler_type).
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
        let credits = billing::crawl_credits(pages_http, pages_browser, pages_ai, &features)
            + scrapix_billing::ocr_credits(pages_ocr);
        self.diagnostics
            .credits_billed
            .fetch_add(credits, std::sync::atomic::Ordering::Relaxed);

        let (Some(pool), Some(acct_id)) = (self.db_pool.clone(), account_id.cloned()) else {
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
        let job_id = job_id.to_string();
        let stripe_cl = self.stripe_client.clone();
        tokio::spawn(async move {
            match billing::deduct_crawl_usage(
                &pool,
                &acct_id,
                credits,
                &description,
                stripe_cl.as_ref(),
            )
            .await
            {
                Ok(new_balance) => {
                    info!(
                        account_id = %acct_id,
                        credits_deducted = credits,
                        new_balance,
                        job_id = %job_id,
                        "Crawl credits deducted"
                    );
                }
                Err(e) => {
                    error!(
                        account_id = %acct_id,
                        credits = credits,
                        job_id = %job_id,
                        error = ?e,
                        "Failed to deduct crawl credits"
                    );
                }
            }
        });
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
                         successful accounting flush (Postgres unavailable?)"
                    );
                    *warned_at = Some(std::time::Instant::now());
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Start a Postgres flush: take the held acks first, then the dirty jobs
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
        let snapshots: Vec<JobState> = {
            let jobs = self.crawl.jobs.read();
            dirty_ids
                .iter()
                .filter_map(|id| jobs.get(id).cloned())
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
    /// - Missing `accounting` column (the Rails migration has not run):
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
                    "jobs.accounting column is missing (Rails migration \
                     20260926000001_add_accounting_to_jobs not applied): job accounting is \
                     kept in memory only and events are acked immediately for this process"
                );
                self.accounting_persisted
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                owed.clear();
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

    /// Flush dirty job counters, accounting and owed terminal writes to
    /// Postgres, then release the acks of the events they cover.
    async fn flush_to_db(&self, pool: &sqlx::PgPool) {
        let batch = self.begin_flush();
        if !batch.snapshots.is_empty() {
            jobs_db::flush_job_counters(pool, &batch.snapshots).await;
        }
        let accounting = match jobs_db::flush_job_accounting(pool, &batch.accounting).await {
            Ok(()) => AccountingFlush::Ok,
            Err(e) => classify_flush_error(&e),
        };
        let mut failed_terminal = HashSet::new();
        if accounting == AccountingFlush::Ok {
            for job in &batch.terminal {
                if jobs_db::update_job_full(pool, job).await.is_err() {
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

        // Persist crawl completion to request_events (1 row per crawl job at completion)
        if let Some(ref batcher) = self.analytics.request_batcher {
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
                    let mut counters = self.diagnostics.domain_counters.write();
                    let counter = counters.entry(domain).or_default();
                    counter.requests += 1;
                    counter.successes += 1;
                    counter.total_response_time_ms += *duration_ms;
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
                let error_record = ErrorRecord {
                    url: url.clone(),
                    domain: domain.clone(),
                    error: error.clone(),
                    status_code: status.or_else(|| extract_status_code(error)),
                    job_id: job_id.to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    retry_count: *retry_count,
                };

                {
                    let mut errors = self.diagnostics.recent_errors.write();
                    errors.push_back(error_record);
                    while errors.len() > 1000 {
                        errors.pop_front();
                    }
                }

                // Track domain stats
                {
                    let mut counters = self.diagnostics.domain_counters.write();
                    let counter = counters.entry(domain).or_default();
                    counter.requests += 1;
                    counter.failures += 1;
                }
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
                self.on_terminal(job_id, terminal_snapshot);

                // Queue job completion email (delivered by the Rails app).
                // The only place a completion email is scheduled (R5).
                self.request_job_email(
                    "job_completed",
                    account_id.clone(),
                    serde_json::json!({
                        "job_id": job_id,
                        "index_uid": index_uid,
                        "pages_crawled": pages_crawled,
                        "documents_indexed": documents_indexed,
                        "duration_secs": duration_secs,
                    }),
                );

                // Deduct credits for crawled pages (fire-and-forget). D4/R4:
                // bill for what was actually delivered — `accounted` (folded
                // above, before `on_terminal` frees the accounting entry)
                // carries the browser/AI-enriched page counts.
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
            }
            CrawlEvent::JobFailed { error, .. } => {
                // No temp index cleanup needed — Replace strategy writes directly to the real index.
                // Stale documents from a failed job will be cleaned up by the next successful crawl.

                let (pages_crawled, account_id) = terminal_snapshot
                    .as_ref()
                    .map(|j| (j.pages_crawled, j.account_id.clone()))
                    .unwrap_or((0, None));
                self.on_terminal(job_id, terminal_snapshot);

                // Queue job failure email (delivered by the Rails app).
                // The only place a failure email is scheduled (R5).
                self.request_job_email(
                    "job_failed",
                    account_id,
                    serde_json::json!({
                        "job_id": job_id,
                        "error_message": error,
                        "pages_crawled": pages_crawled,
                    }),
                );
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

/// One Postgres flush round (see `AppState::begin_flush`).
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
    /// Rails migration has not run. Not retryable.
    SchemaMissing,
}

/// Whether a Postgres SQLSTATE means the schema the accounting flush needs is
/// missing (42703 undefined_column, 42P01 undefined_table).
fn is_schema_missing_sqlstate(code: Option<&str>) -> bool {
    matches!(code, Some("42703") | Some("42P01"))
}

fn classify_flush_error(e: &sqlx::Error) -> AccountingFlush {
    match e {
        sqlx::Error::Database(db) if is_schema_missing_sqlstate(db.code().as_deref()) => {
            AccountingFlush::SchemaMissing
        }
        _ => AccountingFlush::Retry,
    }
}

/// `Option<Instant>` helper for rate-limited logging.
trait ElapsedSince {
    fn is_none_or_elapsed(&self, every: Duration) -> bool;
}

impl ElapsedSince for Option<std::time::Instant> {
    fn is_none_or_elapsed(&self, every: Duration) -> bool {
        self.map_or(true, |t| t.elapsed() >= every)
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
/// Postgres flush).
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
    error: String,
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
}

impl ApiError {
    pub(crate) fn new(error: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: code.into(),
            details: None,
        }
    }

    #[allow(dead_code)]
    fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
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
            "spend_limit_exceeded" => StatusCode::FORBIDDEN,
            "service_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self)).into_response()
    }
}

// ============================================================================
// Account context helpers
// ============================================================================

use crate::auth::{AuthenticatedAccount, AuthenticatedUser};

/// Resolved account context from either API key or session auth
pub(crate) struct AccountContext {
    pub account_id: String,
    pub api_key_id: Option<String>,
    pub tier: String,
    /// User role in this account (None for API key auth — API keys are account-scoped).
    pub user_role: Option<String>,
}

/// Extract account context from request extensions.
/// Returns `None` when auth is not configured (no DATABASE_URL), preserving backward compatibility.
async fn extract_account_context(
    db_pool: Option<&sqlx::PgPool>,
    account_ext: &Option<Extension<AuthenticatedAccount>>,
    user_ext: &Option<Extension<AuthenticatedUser>>,
) -> Option<AccountContext> {
    // API key path — no per-user role (API keys are account-scoped)
    if let Some(Extension(acct)) = account_ext {
        return Some(AccountContext {
            account_id: acct.account_id.clone(),
            api_key_id: acct.api_key_id.clone(),
            tier: acct.tier.clone(),
            user_role: None,
        });
    }

    // Session path: look up account_id + tier + role via DB
    if let (Some(Extension(user)), Some(pool)) = (user_ext, db_pool) {
        let query = if let Some(selected_id) = user.selected_account_id {
            sqlx::query(
                "SELECT a.id, a.tier, m.role FROM account_members m \
                 JOIN accounts a ON a.id = m.account_id \
                 WHERE m.user_id = $1 AND m.account_id = $2",
            )
            .bind(user.user_id)
            .bind(selected_id)
            .fetch_optional(pool)
            .await
        } else {
            sqlx::query(
                "SELECT a.id, a.tier, m.role FROM account_members m \
                 JOIN accounts a ON a.id = m.account_id \
                 WHERE m.user_id = $1 LIMIT 1",
            )
            .bind(user.user_id)
            .fetch_optional(pool)
            .await
        };
        let row = query.ok().flatten();

        if let Some(row) = row {
            use sqlx::Row;
            let account_id: uuid::Uuid = row.get("id");
            let tier: String = row.get("tier");
            let role: String = row.get("role");
            return Some(AccountContext {
                account_id: account_id.to_string(),
                api_key_id: None,
                tier,
                user_role: Some(role),
            });
        }
    }

    // No auth configured or no valid credentials
    None
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
        Self {
            job_id: job.job_id,
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
            eta_seconds: job.eta_seconds,
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
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_limit() -> usize {
    50
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

    /// Custom CSS selector extraction (field_name -> selector definition)
    #[serde(default)]
    extract: HashMap<String, SelectorDefinition>,

    /// AI enrichment options
    #[serde(default)]
    ai: Option<AiOptions>,

    /// Document parsing options, used when the URL serves a PDF or an
    /// office document (OCR of scanned pages, page limits).
    #[serde(default)]
    parsers: documents::ParserOptions,
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
// Diagnostic Response Types
// ============================================================================

/// System stats response
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct SystemStatsResponse {
    meilisearch: Option<MeilisearchStats>,
    jobs: JobSummary,
    diagnostics: DiagnosticsStats,
    collected_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct MeilisearchStats {
    available: bool,
    url: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct JobSummary {
    total: usize,
    running: usize,
    completed: usize,
    failed: usize,
    pending: usize,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct DiagnosticsStats {
    recent_errors_count: usize,
    tracked_domains: usize,
    total_requests: u64,
    total_successes: u64,
    total_failures: u64,
}

/// Errors response
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ErrorsResponse {
    errors: Vec<ErrorRecord>,
    total_count: usize,
    by_status: HashMap<u16, u64>,
    by_domain: Vec<(String, u64)>,
    source: String,
}

/// Error record for tracking
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
struct ErrorRecord {
    url: String,
    domain: String,
    error: String,
    status_code: Option<u16>,
    job_id: String,
    timestamp: String,
    retry_count: u32,
}

/// Errors query parameters
#[derive(Debug, Deserialize, utoipa::IntoParams)]
struct ErrorsQuery {
    #[serde(default = "default_last")]
    last: usize,
    job_id: Option<String>,
}

fn default_last() -> usize {
    20
}

/// Domains response
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct DomainsResponse {
    domains: Vec<DomainInfo>,
    total_domains: usize,
    source: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct DomainInfo {
    domain: String,
    total_requests: u64,
    successful_requests: u64,
    failed_requests: u64,
    avg_response_time_ms: Option<f64>,
}

/// Domains query parameters
#[derive(Debug, Deserialize, utoipa::IntoParams)]
struct DomainsQuery {
    #[serde(default = "default_top")]
    top: usize,
    filter: Option<String>,
}

fn default_top() -> usize {
    20
}

/// Per-domain counter for in-memory tracking
#[derive(Debug, Clone, Default)]
struct DomainCounter {
    requests: u64,
    successes: u64,
    failures: u64,
    total_response_time_ms: u64,
}

// ============================================================================
// Route Handlers
// ============================================================================

/// Health check endpoint
#[utoipa::path(get, path = "/health", tag = "health", responses((status = 200, body = HealthResponse)))]
async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
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

    Json(ServiceHealthResponse { services })
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
#[utoipa::path(post, path = "/scrape", tag = "scrape", request_body = ScrapeRequest, responses((status = 200, body = ScrapeResponse), (status = 400, body = ApiError)), security(("api_key" = [])))]
async fn scrape_url(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    Json(request): Json<ScrapeRequest>,
) -> Result<Json<ScrapeResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;
    check_write_permission(&account_ctx)?;

    if let Some(ref ctx) = account_ctx {
        debug!(account_id = %ctx.account_id, "Scrape request from account");
    }

    // Compute credit cost based on requested features
    let has_ai_summary_req = request.ai.as_ref().is_some_and(|ai| ai.summary);
    let has_ai_extraction_req = request.ai.as_ref().is_some_and(|ai| ai.extract.is_some());
    let scrape_cost =
        billing::scrape_credits(&request.formats, has_ai_summary_req, has_ai_extraction_req);

    // Pre-flight credit check (soft UX check; real deduction is atomic below)
    if let (Some(ref pool), Some(ref ctx)) = (&state.db_pool, &account_ctx) {
        billing::check_credits(pool, &ctx.account_id, scrape_cost).await?;
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

    let use_browser = request.render_js;
    if use_browser && state.browser_renderer.is_none() {
        return Err(ApiError::new(
            "JS rendering is not available (Chrome/Chromium not found on this server)",
            "render_js_unavailable",
        ));
    }

    info!(url = %request.url, render_js = use_browser, "Scraping URL");

    // (a) Fetch using browser renderer or HTTP fetcher
    let crawl_url = CrawlUrl::seed(&request.url);

    let raw_page = if use_browser {
        // Use browser renderer for JS rendering
        state
            .browser_renderer
            .as_ref()
            .unwrap()
            .fetch(&crawl_url)
            .await
            .map_err(|e| {
                ApiError::new(
                    format!("Failed to render URL with browser: {}", e),
                    "fetch_error",
                )
            })?
    } else if request.headers.is_empty() {
        // Use the shared fetcher (connection pooling, DNS cache, retries)
        state
            .fetcher
            .fetch_with_options(&crawl_url, document_fetch_options())
            .await
            .map_err(|e| ApiError::new(format!("Failed to fetch URL: {}", e), "fetch_error"))?
    } else {
        // Build a one-off fetcher with custom headers
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

        let mut builder =
            HttpFetcherBuilder::new().timeout(Duration::from_millis(request.timeout_ms));

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

        return Ok(Json(ScrapeResponse {
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
            warning: None,
            document: None,
            ocr: None,
            status_code,
            scrape_duration_ms: start_time.elapsed().as_millis() as u64,
        }));
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
            &state,
            &account_ctx,
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
                base_cost: scrape_cost,
            },
            start_time,
        )
        .await?;
        return Ok(Json(response));
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
            let sel_extractor = SelectorExtractor::with_definitions(request.extract.clone());
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
    let ai_run = run_ai_enrichment(&state, request.ai.as_ref(), ai_text).await;
    let AiRun {
        result: ai_result,
        warning,
        prompt_tokens: total_prompt_tokens,
        completion_tokens: total_completion_tokens,
        model: ai_model_name,
    } = ai_run;

    let scrape_duration_ms = start_time.elapsed().as_millis() as u64;

    // Track successful scrape in ClickHouse request_events
    let has_ai_summary = request.ai.as_ref().is_some_and(|ai| ai.summary);
    let has_ai_extraction = request.ai.as_ref().is_some_and(|ai| ai.extract.is_some());
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

    // Deduct credits for successful scrape (atomic)
    if let (Some(ref pool), Some(ref ctx)) = (&state.db_pool, &account_ctx) {
        if let Err(e) = billing::check_credits_and_deduct(
            pool,
            &ctx.account_id,
            scrape_cost,
            "scrape",
            &format!("{} ({} credits)", final_url, scrape_cost),
            state.stripe_client.as_ref(),
        )
        .await
        {
            warn!(account_id = %ctx.account_id, error = ?e, "Failed to deduct credit for scrape");
        }
    }

    info!(
        url = %final_url,
        status_code,
        duration_ms = scrape_duration_ms,
        "Scrape completed"
    );

    Ok(Json(ScrapeResponse {
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
        warning,
        document: None,
        ocr: None,
        status_code,
        scrape_duration_ms,
    }))
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
        } else {
            warning = Some("AI features require a provider API key (set AI_PROVIDER and corresponding key env var)".to_string());
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

/// Core crawl creation logic, reusable from handler, trigger, and cron scheduler
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
    let config = if config.meilisearch.url.is_empty() {
        if let (Some(ref pool), Some(ctx)) = (&state.db_pool, account_ctx) {
            let account_uuid: uuid::Uuid = ctx
                .account_id
                .parse()
                .map_err(|_| ApiError::new("Invalid account ID", "internal_error"))?;
            let engine_row = sqlx::query(
                "SELECT url, api_key FROM meilisearch_engines WHERE account_id = $1 AND is_default = true LIMIT 1",
            )
            .bind(account_uuid)
            .fetch_optional(pool)
            .await
            .map_err(|e| ApiError::new(format!("Database error: {e}"), "internal_error"))?
            .ok_or_else(|| {
                ApiError::new(
                    "No Meilisearch engine configured. Add one in Settings.",
                    "validation_error",
                )
            })?;
            use sqlx::Row as _;
            let mut config = config;
            config.meilisearch.url = engine_row.get::<String, _>("url");
            config.meilisearch.api_key = engine_row.get::<String, _>("api_key");
            config
        } else {
            return Err(ApiError::new(
                "Meilisearch configuration is required",
                "validation_error",
            ));
        }
    } else {
        config
    };

    // Full validation (start_urls, index_uid length, and any future
    // #[validate] rules) — after index_uid auto-derivation and Meilisearch
    // engine resolution so both are populated before the length checks run.
    // `mut`: `validate_crawl_config` clamps out-of-range webhook
    // `timeout_ms` values in place (SCR-72 fix round 1).
    let mut config = config;
    validate_crawl_config(&mut config)?;

    // Non-fatal warnings for accepted-but-unhonored (worker-level) fields.
    let warnings = crawl_config_warnings(&config);

    // Pre-flight credit check (1 credit minimum to start a crawl)
    if let (Some(ref pool), Some(ctx)) = (&state.db_pool, account_ctx) {
        billing::check_credits(pool, &ctx.account_id, 1).await?;

        // Enforce max concurrent jobs per billing tier
        let tier: scrapix_core::BillingTier = ctx.tier.parse().unwrap_or_default();
        let max_concurrent = tier.max_concurrent_jobs() as i64;
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE account_id = $1 AND status IN ('pending', 'running')",
        )
        .bind(uuid::Uuid::parse_str(&ctx.account_id).ok())
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        if active_count >= max_concurrent {
            return Err(ApiError::new(
                format!(
                    "Maximum concurrent jobs reached ({}/{}). Upgrade your plan for more.",
                    active_count, max_concurrent
                ),
                "quota_exceeded",
            ));
        }
    }

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
        // Update job as failed
        state.update_job(&job_id, |j| j.fail("Failed to publish any seed URLs"));
        state.forget_job_tracking(&job_id);
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

    // Persist new job to Postgres
    if let (Some(ref pool), Some(snapshot)) = (&state.db_pool, snapshot) {
        let pool = pool.clone();
        tokio::spawn(async move { jobs_db::insert_job(&pool, &snapshot).await });
    }

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

/// Fetch a single URL and extract title, description, and child links.
async fn map_fetch_page(
    fetcher: Arc<HttpFetcher>,
    browser: Option<Arc<CdpRenderer>>,
    url: String,
    base_url: url::Url,
) -> Option<MapFetchResult> {
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

    // Extract title and description from the head via regex (avoids a full DOM parse)
    let head_end = page
        .html
        .find("</head>")
        .unwrap_or(8192.min(page.html.len()));
    let head = &page.html[..head_end];

    let title = RE_TITLE
        .captures(head)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|t| !t.is_empty());

    let description = RE_DESC
        .captures(head)
        .or_else(|| RE_DESC_ALT.captures(head))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|d| !d.is_empty());

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
    user_ext: Option<Extension<AuthenticatedUser>>,
    Json(request): Json<MapRequest>,
) -> Result<Json<MapResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;
    check_write_permission(&account_ctx)?;

    // Pre-flight credit check (map costs 2 credits)
    if let (Some(ref pool), Some(ref ctx)) = (&state.db_pool, &account_ctx) {
        billing::check_credits(pool, &ctx.account_id, billing::MAP_CREDITS).await?;
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

    // Deduct 2 credits for successful map (atomic)
    if let (Some(ref pool), Some(ref ctx)) = (&state.db_pool, &account_ctx) {
        if let Err(e) = billing::check_credits_and_deduct(
            pool,
            &ctx.account_id,
            billing::MAP_CREDITS,
            "map",
            &request.url,
            state.stripe_client.as_ref(),
        )
        .await
        {
            warn!(account_id = %ctx.account_id, error = ?e, "Failed to deduct credit for map");
        }
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
    user_ext: Option<Extension<AuthenticatedUser>>,
    Json(request): Json<SearchRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;
    check_write_permission(&account_ctx)?;

    // Pre-flight credit check
    if let (Some(ref pool), Some(ref ctx)) = (&state.db_pool, &account_ctx) {
        billing::check_credits(pool, &ctx.account_id, billing::SEARCH_CREDITS).await?;
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
    let pool = state
        .db_pool
        .as_ref()
        .ok_or_else(|| ApiError::new("Search requires database configuration", "internal_error"))?;

    let account_id = account_ctx
        .as_ref()
        .map(|c| c.account_id.clone())
        .ok_or_else(|| ApiError::new("Authentication required", "unauthorized"))?;

    let account_uuid: uuid::Uuid = account_id
        .parse()
        .map_err(|_| ApiError::new("Invalid account ID", "internal_error"))?;

    let engine_row = sqlx::query(
        "SELECT url, api_key FROM meilisearch_engines WHERE account_id = $1 AND is_default = true LIMIT 1",
    )
    .bind(account_uuid)
    .fetch_optional(pool)
    .await
    .map_err(|e| ApiError::new(format!("Database error: {e}"), "internal_error"))?
    .ok_or_else(|| {
        ApiError::new(
            "No default Meilisearch engine configured. Add one in Settings > Engines.",
            "not_found",
        )
    })?;

    use sqlx::Row;
    let engine_url: String = engine_row.get("url");
    let engine_api_key: String = engine_row.get("api_key");

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

    // Deduct 2 credits for search
    if let (Some(ref pool), Some(ref ctx)) = (&state.db_pool, &account_ctx) {
        if let Err(e) = billing::check_credits_and_deduct(
            pool,
            &ctx.account_id,
            billing::SEARCH_CREDITS,
            "search",
            &format!("{} q={}", request.url, request.q),
            state.stripe_client.as_ref(),
        )
        .await
        {
            warn!(account_id = %ctx.account_id, error = ?e, "Failed to deduct credit for search");
        }
    }

    Ok(Json(result))
}

/// Create a new async crawl job
#[utoipa::path(post, path = "/crawl", tag = "crawl", request_body = scrapix_core::CrawlConfig, responses((status = 200, body = CreateCrawlResponse), (status = 400, body = ApiError)), security(("api_key" = [])))]
async fn create_crawl(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    Json(config): Json<CrawlConfig>,
) -> Result<Json<CreateCrawlResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;
    check_write_permission(&account_ctx)?;
    Ok(Json(
        do_create_crawl(&state, config, account_ctx.as_ref()).await?,
    ))
}

/// Create a sync crawl job (waits for completion)
#[utoipa::path(post, path = "/crawl/sync", tag = "crawl", request_body = scrapix_core::CrawlConfig, responses((status = 200, body = CreateCrawlResponse), (status = 400, body = ApiError)), security(("api_key" = [])))]
async fn create_crawl_sync(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    Json(config): Json<CrawlConfig>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;
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
                    return Ok(Json(job.into()));
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
                            return Ok(Json(job.into()));
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
    user_ext: Option<Extension<AuthenticatedUser>>,
    Json(configs): Json<Vec<CrawlConfig>>,
) -> Result<Json<BulkCrawlResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;
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
    user_ext: Option<Extension<AuthenticatedUser>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;

    // Try in-memory first, fall back to Postgres for historical jobs
    let job = if let Some(job) = state.get_job(&job_id) {
        job
    } else if let Some(ref pool) = state.db_pool {
        if let Some(ctx) = &account_ctx {
            jobs_db::get_job_for_account(pool, &job_id, &ctx.account_id).await
        } else {
            jobs_db::get_job_from_db(pool, &job_id).await
        }
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
    user_ext: Option<Extension<AuthenticatedUser>>,
    Path(job_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;

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
    user_ext: Option<Extension<AuthenticatedUser>>,
    Path(job_id): Path<String>,
    Query(params): Query<JobEventsHistoryParams>,
) -> Result<Json<JobEventsHistoryResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;

    // Verify the job exists (check in-memory then Postgres)
    let job = if let Some(job) = state.get_job(&job_id) {
        job
    } else if let Some(ref pool) = state.db_pool {
        if let Some(ctx) = &account_ctx {
            jobs_db::get_job_for_account(pool, &job_id, &ctx.account_id).await
        } else {
            jobs_db::get_job_from_db(pool, &job_id).await
        }
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
// Diagnostic Handlers
// ============================================================================

/// System stats endpoint
#[utoipa::path(get, path = "/stats", tag = "health", responses((status = 200, body = SystemStatsResponse)))]
async fn handle_stats(State(state): State<Arc<AppState>>) -> Json<SystemStatsResponse> {
    // Compute job summary
    let jobs = state.crawl.jobs.read();
    let mut running = 0;
    let mut completed = 0;
    let mut failed = 0;
    let mut pending = 0;

    for job in jobs.values() {
        match job.status {
            JobStatus::Running => running += 1,
            JobStatus::Completed => completed += 1,
            JobStatus::Failed => failed += 1,
            JobStatus::Pending => pending += 1,
            JobStatus::Cancelled => failed += 1,
            JobStatus::Paused => pending += 1,
        }
    }

    let job_summary = JobSummary {
        total: jobs.len(),
        running,
        completed,
        failed,
        pending,
    };
    drop(jobs);

    // Compute diagnostics stats
    let errors_count = state.diagnostics.recent_errors.read().len();
    let counters = state.diagnostics.domain_counters.read();
    let tracked_domains = counters.len();
    let mut total_requests = 0u64;
    let mut total_successes = 0u64;
    let mut total_failures = 0u64;

    for counter in counters.values() {
        total_requests += counter.requests;
        total_successes += counter.successes;
        total_failures += counter.failures;
    }
    drop(counters);

    let diagnostics = DiagnosticsStats {
        recent_errors_count: errors_count,
        tracked_domains,
        total_requests,
        total_successes,
        total_failures,
    };

    // Meilisearch status (we don't have direct access, just indicate availability from env)
    let meilisearch = std::env::var("MEILISEARCH_URL")
        .ok()
        .map(|url| MeilisearchStats {
            available: true,
            url,
        });

    Json(SystemStatsResponse {
        meilisearch,
        jobs: job_summary,
        diagnostics,
        collected_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Errors endpoint
#[utoipa::path(get, path = "/errors", tag = "health", params(ErrorsQuery), responses((status = 200, body = ErrorsResponse)))]
async fn handle_errors(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ErrorsQuery>,
) -> Json<ErrorsResponse> {
    let errors = state.diagnostics.recent_errors.read();

    // Filter by job_id if specified
    let filtered: Vec<ErrorRecord> = if let Some(ref job_id) = params.job_id {
        errors
            .iter()
            .filter(|e| &e.job_id == job_id)
            .cloned()
            .collect()
    } else {
        errors.iter().cloned().collect()
    };

    let total_count = filtered.len();

    // Take last N errors (most recent)
    let recent: Vec<ErrorRecord> = filtered.into_iter().rev().take(params.last).collect();

    // Compute status code distribution
    let mut by_status: HashMap<u16, u64> = HashMap::new();
    for error in &recent {
        if let Some(code) = error.status_code {
            *by_status.entry(code).or_insert(0) += 1;
        }
    }

    // Compute domain distribution
    let mut domain_counts: HashMap<String, u64> = HashMap::new();
    for error in &recent {
        *domain_counts.entry(error.domain.clone()).or_insert(0) += 1;
    }

    let mut by_domain: Vec<(String, u64)> = domain_counts.into_iter().collect();
    by_domain.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    by_domain.truncate(10);

    Json(ErrorsResponse {
        errors: recent,
        total_count,
        by_status,
        by_domain,
        source: "memory".to_string(),
    })
}

/// Domains endpoint
#[utoipa::path(get, path = "/domains", tag = "health", params(DomainsQuery), responses((status = 200, body = DomainsResponse)))]
async fn handle_domains(
    State(state): State<Arc<AppState>>,
    Query(params): Query<DomainsQuery>,
) -> Json<DomainsResponse> {
    let counters = state.diagnostics.domain_counters.read();

    // Filter by pattern if specified
    let filtered: Vec<(&String, &DomainCounter)> = if let Some(ref filter) = params.filter {
        counters
            .iter()
            .filter(|(domain, _)| domain.contains(filter))
            .collect()
    } else {
        counters.iter().collect()
    };

    let total_domains = filtered.len();

    // Sort by total requests and take top N
    let mut sorted: Vec<_> = filtered;
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1.requests));
    sorted.truncate(params.top);

    let domains: Vec<DomainInfo> = sorted
        .into_iter()
        .map(|(domain, counter)| {
            let avg_time = if counter.successes > 0 {
                Some(counter.total_response_time_ms as f64 / counter.successes as f64)
            } else {
                None
            };

            DomainInfo {
                domain: domain.clone(),
                total_requests: counter.requests,
                successful_requests: counter.successes,
                failed_requests: counter.failures,
                avg_response_time_ms: avg_time,
            }
        })
        .collect();

    Json(DomainsResponse {
        domains,
        total_domains,
        source: "memory".to_string(),
    })
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
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state))
}

/// Handle a WebSocket connection
async fn handle_ws_connection(socket: WebSocket, state: Arc<AppState>) {
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
                    let response = handle_ws_message(client_msg, &state_clone, &subs).await;
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

/// Handle a WebSocket client message
async fn handle_ws_message(
    msg: WsClientMessage,
    state: &Arc<AppState>,
    subscriptions: &Arc<RwLock<std::collections::HashSet<String>>>,
) -> WsServerMessage {
    match msg {
        WsClientMessage::Subscribe { job_id } => {
            if state.get_job(&job_id).is_some() {
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
            if let Some(job) = state.get_job(&job_id) {
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
    user_ext: Option<Extension<AuthenticatedUser>>,
) -> Result<impl IntoResponse, ApiError> {
    // Check if job exists
    let job = state
        .get_job(&job_id)
        .ok_or_else(|| ApiError::new("Job not found", "not_found"))?;

    // Verify account ownership if auth is enabled
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;
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

/// Cancel a job
///
/// Stops the job everywhere (frontier and workers) and bills the pages
/// crawled so far. Only a pending, running or paused job can be cancelled:
/// a job that already completed, failed or was cancelled returns 409 and
/// keeps its status.
#[utoipa::path(delete, path = "/job/{id}", tag = "jobs", params(("id" = String, Path, description = "Job ID")), responses((status = 200, body = JobStatusResponse), (status = 404, body = ApiError), (status = 409, description = "The job is already terminal", body = ApiError)), security(("api_key" = [])))]
async fn cancel_job(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    owned_job(&state, &account_ext, &user_ext, &job_id).await?;
    Ok(Json(state.cancel(&job_id)?.into()))
}

/// Pause a running job
///
/// The frontier stops dispatching the job's URLs (pages already in flight
/// finish). Only a running job can be paused; any other status returns 409.
#[utoipa::path(post, path = "/job/{id}/pause", tag = "jobs", params(("id" = String, Path, description = "Job ID")), responses((status = 200, body = JobStatusResponse), (status = 404, body = ApiError), (status = 409, description = "The job is not running", body = ApiError)), security(("api_key" = [])))]
async fn pause_job(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    owned_job(&state, &account_ext, &user_ext, &job_id).await?;
    Ok(Json(state.pause(&job_id)?.into()))
}

/// Resume a paused job
///
/// Only a paused job can be resumed; any other status returns 409.
#[utoipa::path(post, path = "/job/{id}/resume", tag = "jobs", params(("id" = String, Path, description = "Job ID")), responses((status = 200, body = JobStatusResponse), (status = 404, body = ApiError), (status = 409, description = "The job is not paused", body = ApiError)), security(("api_key" = [])))]
async fn resume_job(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobStatusResponse>, ApiError> {
    owned_job(&state, &account_ext, &user_ext, &job_id).await?;
    Ok(Json(state.resume(&job_id)?.into()))
}

/// The in-memory job `job_id`, if it exists and the caller owns it.
async fn owned_job(
    state: &AppState,
    account_ext: &Option<Extension<AuthenticatedAccount>>,
    user_ext: &Option<Extension<AuthenticatedUser>>,
    job_id: &str,
) -> Result<JobState, ApiError> {
    let account_ctx = extract_account_context(state.db_pool.as_ref(), account_ext, user_ext).await;
    let existing = state
        .get_job(job_id)
        .ok_or_else(|| ApiError::new("Job not found", "not_found"))?;
    check_job_ownership(&existing, &account_ctx)?;
    Ok(existing)
}

/// List all jobs
#[utoipa::path(get, path = "/jobs", tag = "jobs", params(ListJobsQuery), responses((status = 200, description = "List of jobs")), security(("api_key" = [])))]
async fn list_jobs(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    Query(params): Query<ListJobsQuery>,
) -> Json<Vec<JobStatusResponse>> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;

    // When Postgres is available, query DB for full history (survives restarts)
    // and overlay in-memory data for running jobs (fresher counters).
    let jobs: Vec<JobState> = if let Some(ref pool) = state.db_pool {
        let mut db_jobs = if let Some(ctx) = &account_ctx {
            jobs_db::list_jobs_for_account_db(
                pool,
                &ctx.account_id,
                params.limit as i64,
                params.offset as i64,
            )
            .await
        } else {
            jobs_db::list_all_jobs_db(pool, params.limit as i64, params.offset as i64).await
        };

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
        db_jobs
    } else if let Some(ctx) = &account_ctx {
        // No DB — filter in-memory by account
        let all_jobs = state.crawl.jobs.read();
        all_jobs
            .values()
            .filter(|j| j.account_id.as_deref() == Some(&ctx.account_id))
            .skip(params.offset)
            .take(params.limit)
            .cloned()
            .collect()
    } else {
        state.list_jobs(params.limit, params.offset)
    };
    Json(jobs.into_iter().map(|j| j.into()).collect())
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
    // applied — and, for accounting events with Postgres persistence, only
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
    info!(
        host = %args.host,
        port = args.port,
        "Starting Scrapix API server"
    );

    // Initialize ClickHouse for analytics (optional)
    let (analytics_state, request_batcher, ai_usage_batcher, job_event_batcher, page_event_batcher) =
        init_clickhouse().await;

    // Initialize auth state if DATABASE_URL is provided
    let auth_state = if let Some(ref db_url) = args.database_url {
        let jwt_secret = args.jwt_secret.clone().unwrap_or_else(|| {
            if std::env::var("ENVIRONMENT").as_deref() == Ok("production") {
                panic!("JWT_SECRET is required in production. Set the JWT_SECRET environment variable.");
            }
            warn!("JWT_SECRET not set — using insecure default. Set JWT_SECRET in production!");
            "dev-jwt-secret-change-in-production".to_string()
        });
        match auth::AuthState::new(db_url, jwt_secret).await {
            Ok(state) => {
                // The schema is owned by the Rails app (saas/db/migrate,
                // `rails db:prepare`) — the engine no longer applies it.

                info!("Authentication enabled via PostgreSQL");
                Some(Arc::new(state))
            }
            Err(e) => {
                warn!(error = %e, "Failed to connect to PostgreSQL. Auth disabled.");
                None
            }
        }
    } else {
        info!("Authentication disabled (DATABASE_URL not set)");
        None
    };

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
    let db_pool = auth_state.as_ref().map(|a| a.pool.clone());
    let stripe_client = args.stripe_secret_key.as_ref().map(::stripe::Client::new);
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
        db_pool,
        stripe_client,
        analytics_state.clone(),
        webhook_dispatcher,
    );
    state.ocr = ocr;
    let state = Arc::new(state);

    // Recover active jobs from Postgres on startup
    if let Some(ref pool) = state.db_pool {
        let recovered = jobs_db::load_active_jobs(pool).await;
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
                "Recovered active jobs from Postgres"
            );
        }

        // Recover the work accounting of running/paused jobs (R5). The
        // per-page seen-sets are not persisted, so redelivered events after a
        // restart are not deduplicated against pre-restart ones.
        let persisted = jobs_db::load_active_job_accounting(pool).await;
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
                "Recovered job work accounting from Postgres"
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

    // Start periodic flush task (ClickHouse batchers + Postgres dirty job counters)
    let has_flush_work = request_batcher.is_some()
        || ai_usage_batcher.is_some()
        || job_event_batcher.is_some()
        || state.db_pool.is_some();
    let flush_handle = if has_flush_work {
        let req_batcher = request_batcher.clone();
        let ai_batcher = ai_usage_batcher.clone();
        let job_batcher = job_event_batcher.clone();
        let flush_state = state.clone();
        // With Postgres, the flush task owns the consumer's shutdown join: it
        // must drain before the final flush (R-19).
        let mut consumer_join = if state.db_pool.is_some() {
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
                        // Flush dirty job counters + accounting to Postgres,
                        // then release the acks they cover
                        if let Some(ref pool) = flush_state.db_pool {
                            flush_state.flush_to_db(pool).await;
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
                        // Final Postgres flush — only once the event consumer
                        // has drained and sync-committed (R-19), so the final
                        // snapshot covers every event it applied.
                        if let Some(ref pool) = flush_state.db_pool {
                            if let Some(handle) = consumer_join.take() {
                                if let Err(e) = handle.await {
                                    warn!("Consumer task failed during shutdown: {}", e);
                                }
                            }
                            flush_state.flush_to_db(pool).await;
                        }
                        break;
                    }
                }
            }
        });
        if request_batcher.is_some() || ai_usage_batcher.is_some() {
            info!("ClickHouse event persistence enabled (flush interval: 5s)");
        }
        if state.db_pool.is_some() {
            info!("Postgres job counter flush enabled (flush interval: 5s)");
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

    // Start cron scheduler if database is configured
    let cron_handle = if let Some(ref pool) = state.db_pool {
        let handle =
            configs::spawn_cron_scheduler(state.clone(), pool.clone(), shutdown_rx.clone());
        info!("Cron scheduler started (30s tick interval)");
        Some(handle)
    } else {
        None
    };

    // Email delivery moved to the Rails app (SolidQueue drains the shared
    // scheduled_emails queue); the engine only inserts rows.

    // Build router
    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/health", get(health))
        .route("/health/services", get(health_services))
        .route("/metrics", get(metrics))
        .route("/stats", get(handle_stats))
        .route("/errors", get(handle_errors))
        .route("/domains", get(handle_domains))
        .route("/ws", get(ws_handler))
        .route("/ws/job/{id}", get(ws_job_handler));

    // Product routes — revenue-generating API endpoints
    let product_routes = Router::new()
        .route("/scrape", post(scrape_url))
        .route("/map", post(map_url))
        .route("/search", post(search_url))
        .route("/crawl", post(create_crawl))
        .route("/crawl/sync", post(create_crawl_sync))
        .route("/crawl/bulk", post(create_crawl_bulk));

    // Management routes — job monitoring and configuration
    let management_routes = Router::new()
        .route("/jobs", get(list_jobs))
        .route("/job/{id}/status", get(job_status))
        .route("/job/{id}/events", get(job_events))
        .route("/job/{id}/events/history", get(get_job_events_history))
        .route("/job/{id}", delete(cancel_job))
        .route("/job/{id}/pause", post(pause_job))
        .route("/job/{id}/resume", post(resume_job));

    // Protected routes (API key auth required when enabled).
    // The SaaS surface (auth, account/team, configs/engines CRUD, billing,
    // Stripe, analytics pipes, OAuth provider, /mcp) is served by the Rails
    // app (saas/, SCR-85); the engine keeps only the crawl data plane.
    let protected_routes = product_routes.merge(management_routes);

    // Apply auth middleware if configured (accepts API key, Bearer token, or
    // session cookie). route_layer keeps it off the 404 fallback, so removed
    // SaaS paths return 404 instead of a misleading 401.
    let protected_routes = if let Some(ref auth) = auth_state {
        protected_routes.route_layer(middleware::from_fn_with_state(
            auth.clone(),
            auth::validate_api_key_or_session,
        ))
    } else {
        protected_routes
    };

    // Per-account rate limiting on protected routes was removed — pricing is
    // usage-based (credits), not per-request, so there's nothing to gate on the
    // request rate. Brute-force protection on the auth endpoints lives with
    // the auth endpoints themselves, in the Rails app (Rack::Attack).
    let mut app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    // OAuth token cleanup: both backends validate tokens from the shared
    // Postgres; the engine hosts the hourly expired-code/token sweep.
    if let Some(ref auth) = auth_state {
        auth::oauth::spawn_token_cleanup(auth.pool.clone());
    }

    // OpenAPI spec + Scalar docs UI
    {
        use utoipa::OpenApi;
        let spec = openapi::ScrapixApi::openapi();
        let spec_json = spec.to_json().expect("OpenAPI JSON serialization");
        app = app
            .route(
                "/openapi.json",
                get(|| async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        spec_json,
                    )
                }),
            )
            .route(
                "/docs",
                get(|| async {
                    axum::response::Html(
                        r#"<!doctype html>
<html>
<head><title>Scrapix API Reference</title><meta charset="utf-8"/></head>
<body>
<script id="api-reference" data-url="/openapi.json"></script>
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#,
                    )
                }),
            );
        info!("OpenAPI spec at /openapi.json, docs UI at /docs");
    }

    // The analytics pipes API (/analytics/v0/pipes) is served by the Rails
    // app; the engine only writes events to ClickHouse (batchers above) and
    // reads page-event history for /job/{id}/events/history.

    // Request body size limit (2 MB default, prevents DoS via large payloads)
    app = app.layer(tower_http::limit::RequestBodyLimitLayer::new(
        2 * 1024 * 1024,
    ));

    // POST /parse (document upload) is merged after the 2 MB layer — layers
    // only wrap routes that already exist — with its own cap
    // (DOCUMENT_MAX_SIZE_MB + multipart overhead) and the same auth.
    {
        let upload_limit = documents::max_document_bytes() as usize + 1024 * 1024;
        let parse_routes = Router::new()
            .route("/parse", post(documents::parse_upload))
            .layer(axum::extract::DefaultBodyLimit::max(upload_limit))
            .layer(tower_http::limit::RequestBodyLimitLayer::new(upload_limit));
        let parse_routes = if let Some(ref auth) = auth_state {
            parse_routes.route_layer(middleware::from_fn_with_state(
                auth.clone(),
                auth::validate_api_key_or_session,
            ))
        } else {
            parse_routes
        };
        app = app.merge(
            parse_routes
                .layer(TraceLayer::new_for_http())
                .with_state(state.clone()),
        );
    }

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
    if let Some(handle) = cron_handle {
        if let Err(e) = handle.await {
            warn!("Cron task failed during shutdown: {}", e);
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
}

/// Job lifecycle (R5/R9) tests on a DB-less `AppState` over the in-process bus.
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use scrapix_queue::{ChannelBus, MessageConsumer};
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    fn test_state(bus: &ChannelBus) -> AppState {
        let robots = Arc::new(RobotsCache::new(RobotsConfig::default()).unwrap());
        let fetcher = Arc::new(HttpFetcherBuilder::new().build(robots).unwrap());
        AppState::new(
            AnyProducer::channel(bus.producer()),
            AppConfig {
                max_jobs: 100,
                job_stall_timeout: Duration::from_secs(1800),
                completion_grace: Duration::from_secs(3),
                resume_heal_after: Duration::from_secs(60),
                max_pending_acks: 50_000,
            },
            None,
            None,
            None,
            None,
            fetcher,
            None,
            None,
            None,
            None,
            None,
            webhooks::WebhookDispatcher::new(
                scrapix_crawler::safe_client_builder(None, true)
                    .build()
                    .unwrap(),
                webhooks::DEFAULT_MAX_CONCURRENT_DELIVERIES,
            ),
        )
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

    #[tokio::test]
    async fn balanced_job_completes_once_after_grace_with_one_email() {
        let bus = ChannelBus::new();
        let control = bus.consumer();
        control.subscribe(&[topic_names::JOB_STATUS]).unwrap();
        let state = test_state(&bus);
        running_job(&state, "j1", 1);

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
        // as with a Postgres pool
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
        assert!(is_schema_missing_sqlstate(Some("42703")));
        assert!(is_schema_missing_sqlstate(Some("42P01")));
        assert!(!is_schema_missing_sqlstate(Some("08006")));
        assert!(!is_schema_missing_sqlstate(None));
        assert_eq!(
            classify_flush_error(&sqlx::Error::PoolTimedOut),
            AccountingFlush::Retry
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
        let state = test_state(&bus);
        running_job(&state, "j1", 2);
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
    /// AI-enriched pages bills exactly the credits for what was delivered —
    /// not the whole job at the browser rate, and not AI credits for pages
    /// that were never AI-enriched. Uses the DB-less `credits_billed`
    /// diagnostic hook (no Postgres needed, matching the other billing
    /// tests in this module).
    #[tokio::test]
    async fn completed_job_with_mixed_delivery_bills_expected_credits() {
        let bus = ChannelBus::new();
        let state = test_state(&bus);
        running_job(&state, "mix", 3);

        // AI features enabled on the job (surcharge = 5 + 5 = 10/page), no
        // other features, crawler_type is irrelevant to billing now.
        let features = FeaturesConfig::from_cli_args(
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
        // 1 http (1 credit) + 2 browser (2 credits each = 4) + 1 AI-enriched
        // page (10 credits surcharge) = 15. Not 3 * 12 = 36, which is what
        // the old per-job browser+AI rate would have charged.
        assert_eq!(d.credits_billed.load(Ordering::Relaxed), 15);
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
        let state = test_state(&bus);
        running_job(&state, "j1", 5);
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
            if i % 2 == 0 {
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
