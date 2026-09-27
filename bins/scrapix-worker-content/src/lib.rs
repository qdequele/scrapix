//! Scrapix Content Worker
//!
//! Processes raw HTML pages into structured documents for indexing.
//!
//! ## Responsibilities
//!
//! 1. Consume raw pages from the `scrapix.pages.raw` topic
//! 2. Parse HTML to extract content, title, metadata
//! 3. Convert content to Markdown
//! 4. Detect language
//! 5. Optionally split into blocks by headings
//! 6. Publish processed documents to `scrapix.documents` topic
//! 7. Index documents directly to Meilisearch
//!
//! ## Delivery (R2)
//!
//! Every `RawPageMessage` ends in exactly one outcome event carrying its
//! `url_message_id`: `DocumentIndexed`, `DocumentSkipped` or
//! `DocumentFailed`. Skipped/failed pages are acked right after their event
//! is published. Indexed pages hand their `Ack` to the Meilisearch buffer;
//! once Meilisearch accepted the batch containing all of the page's
//! documents, `DocumentIndexed` is published and only then is the message
//! acked. A failed publish or a rejected Meilisearch write leaves the
//! message un-acked (redelivered).

mod features;

pub use features::feature_applies;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use scrapix_lifecycle::{
    idle_minutes_from_env, install_signal_handlers, spawn_idle_watchdog, spawn_wake_listener,
    wake_port_from_env,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use scrapix_ai::{AiClient, AiService, AiUsageContext, AiUsageEvent, AI_USAGE_CONTEXT};
use scrapix_core::{Ack, Document, FeaturesConfig, RawPage};
use scrapix_extractor::{
    BlockConfig, BlockSplitter, ContentBlock, SchemaExtractor, SelectorExtractor,
};
use scrapix_frontier::{NearDuplicateConfig, NearDuplicateDetector};
use scrapix_parser::{HtmlParser, HtmlParserConfig};
use scrapix_queue::{
    control_group_id, topic_names, AnyConsumer, AnyProducer, CancelledJobs, ConsumerBuilder,
    CrawlEvent, CrawlHistoryMessage, DocumentMessage, ProducerBuilder, RawPageMessage,
};
use scrapix_storage::{DocAck, MeilisearchStorage, MeilisearchStorageBuilder};

/// `JobWarning` when a job asks for AI enrichment on a worker without an
/// AI provider.
pub const WARN_AI_NO_PROVIDER: &str =
    "AI enrichment requested but no AI provider configured on content workers";
/// `JobWarning` when a job asks for AI enrichment together with block
/// splitting (AI only runs on whole pages).
pub const WARN_AI_BLOCK_SPLIT: &str =
    "AI enrichment is skipped for block-split pages (features.block_split is enabled)";

/// Content processing worker for parsing HTML and creating documents
#[derive(Parser, Debug)]
#[command(name = "scrapix-worker-content")]
#[command(version, about = "Content processing worker")]
pub struct Args {
    /// Kafka/Redpanda broker addresses
    #[arg(short, long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
    pub brokers: String,

    /// Consumer group ID
    #[arg(short, long, env = "KAFKA_GROUP_ID", default_value = "scrapix-content")]
    pub group_id: String,

    /// Number of raw pages processed at once (parse, enrich, buffer).
    /// A page's handler returns once its documents are buffered; the page
    /// stays un-acked (offset uncommitted) until Meilisearch accepts their
    /// batch. Un-acked work is therefore bounded by the Meilisearch buffers
    /// (at most 4 x batch size per index), not by this value.
    #[arg(short, long, env = "CONCURRENCY", default_value = "8")]
    pub concurrency: usize,

    /// Meilisearch URL
    #[arg(long, env = "MEILISEARCH_URL", default_value = "http://localhost:7700")]
    pub meilisearch_url: String,

    /// Meilisearch API key
    #[arg(long, env = "MEILISEARCH_API_KEY")]
    pub meilisearch_key: Option<String>,

    /// Default index UID (can be overridden by message)
    #[arg(long, env = "MEILISEARCH_INDEX", default_value = "documents")]
    pub default_index: String,

    /// Enable content extraction (readability algorithm)
    #[arg(long, env = "EXTRACT_CONTENT", default_value = "true")]
    pub extract_content: bool,

    /// Enable Markdown conversion
    #[arg(long, env = "CONVERT_MARKDOWN", default_value = "true")]
    pub convert_markdown: bool,

    /// Enable language detection
    #[arg(long, env = "DETECT_LANGUAGE", default_value = "true")]
    pub detect_language: bool,

    /// Enable schema.org extraction
    #[arg(long, env = "EXTRACT_SCHEMA", default_value = "true")]
    pub extract_schema: bool,

    /// Minimum content length to consider valid (characters)
    #[arg(long, env = "MIN_CONTENT_LENGTH", default_value = "100")]
    pub min_content_length: usize,

    /// Publish documents to Kafka topic (in addition to Meilisearch)
    #[arg(long, env = "PUBLISH_TO_KAFKA")]
    pub publish_to_kafka: bool,

    /// Publish crawl history to frontier service for recrawl scheduling
    #[arg(long, env = "PUBLISH_HISTORY", default_value = "false")]
    pub publish_history: bool,

    /// Skip Meilisearch indexing (only publish to Kafka)
    #[arg(long, env = "SKIP_MEILISEARCH")]
    pub skip_meilisearch: bool,

    /// Batch size for Meilisearch indexing
    /// Larger batches reduce Meilisearch API call overhead
    #[arg(long, env = "BATCH_SIZE", default_value = "2000")]
    pub batch_size: usize,

    /// Worker ID (for logging/metrics)
    #[arg(long, env = "WORKER_ID")]
    pub worker_id: Option<String>,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,

    // === AI ENRICHMENT OPTIONS ===
    /// Enable AI summarization (generates ai_summary field)
    #[arg(long, env = "ENABLE_SUMMARY")]
    pub enable_summary: bool,

    /// Summary model to use
    #[arg(long, env = "SUMMARY_MODEL", default_value = "gpt-5-nano")]
    pub summary_model: String,

    /// Enable AI extraction with custom prompt (generates ai_extraction field)
    #[arg(long, env = "ENABLE_EXTRACTION")]
    pub enable_extraction: bool,

    /// Custom extraction prompt (required if enable_extraction is true)
    /// Use {content} as placeholder for the page content
    #[arg(long, env = "EXTRACTION_PROMPT")]
    pub extraction_prompt: Option<String>,

    /// Extraction model to use
    #[arg(long, env = "EXTRACTION_MODEL", default_value = "gpt-5-nano")]
    pub extraction_model: String,

    /// Maximum tokens for AI responses
    #[arg(long, env = "AI_MAX_TOKENS", default_value = "1000")]
    pub ai_max_tokens: u32,

    /// Maximum concurrent AI requests
    #[arg(long, env = "AI_CONCURRENCY", default_value = "5")]
    pub ai_concurrency: usize,

    // === BLOCK SPLITTING OPTIONS ===
    /// Enable block splitting (creates multiple documents per page, split by headings)
    #[arg(long, env = "ENABLE_BLOCK_SPLIT", default_value = "false")]
    pub enable_block_split: bool,

    /// Minimum heading level to split on (1-6, default: 2 = H2)
    #[arg(long, env = "BLOCK_SPLIT_MIN_LEVEL", default_value = "2")]
    pub block_split_min_level: u8,

    /// Maximum heading level to split on (1-6, default: 4 = H4)
    #[arg(long, env = "BLOCK_SPLIT_MAX_LEVEL", default_value = "4")]
    pub block_split_max_level: u8,

    /// Minimum content length for a block (characters)
    #[arg(long, env = "BLOCK_SPLIT_MIN_LENGTH", default_value = "50")]
    pub block_split_min_length: usize,

    // === NEAR-DUPLICATE DETECTION OPTIONS ===
    /// Enable near-duplicate detection to skip similar content
    #[arg(long, env = "ENABLE_DEDUP", default_value = "false")]
    pub enable_dedup: bool,

    /// Use SimHash (faster) or MinHash (more accurate) for deduplication
    #[arg(long, env = "DEDUP_USE_SIMHASH", default_value = "true")]
    pub dedup_use_simhash: bool,

    /// SimHash Hamming distance threshold (0-64, lower = stricter, default 3)
    #[arg(long, env = "DEDUP_SIMHASH_THRESHOLD", default_value = "3")]
    pub dedup_simhash_threshold: u32,

    /// MinHash Jaccard similarity threshold (0.0-1.0, higher = stricter, default 0.85)
    #[arg(long, env = "DEDUP_MINHASH_THRESHOLD", default_value = "0.85")]
    pub dedup_minhash_threshold: f64,

    /// Maximum fingerprints to store (memory limit)
    #[arg(long, env = "DEDUP_MAX_FINGERPRINTS", default_value = "10000000")]
    pub dedup_max_fingerprints: usize,
}

/// Worker metrics for monitoring
#[derive(Debug, Default)]
struct WorkerMetrics {
    pages_processed: AtomicU64,
    pages_succeeded: AtomicU64,
    pages_failed: AtomicU64,
    pages_skipped: AtomicU64,
    pages_duplicate: AtomicU64,
    documents_created: AtomicU64,
    documents_indexed: AtomicU64,
    bytes_processed: AtomicU64,
    active_processors: AtomicU64,
}

impl WorkerMetrics {
    fn record_success(&self, bytes: u64, doc_count: u64) {
        self.pages_processed.fetch_add(1, Ordering::Relaxed);
        self.pages_succeeded.fetch_add(1, Ordering::Relaxed);
        self.bytes_processed.fetch_add(bytes, Ordering::Relaxed);
        self.documents_created
            .fetch_add(doc_count, Ordering::Relaxed);
        scrapix_core::metrics::content_documents_total()
            .with_label_values(&["success"])
            .inc();
    }

    fn record_failure(&self) {
        self.pages_processed.fetch_add(1, Ordering::Relaxed);
        self.pages_failed.fetch_add(1, Ordering::Relaxed);
        scrapix_core::metrics::content_documents_total()
            .with_label_values(&["failure"])
            .inc();
    }

    fn record_skipped(&self) {
        self.pages_processed.fetch_add(1, Ordering::Relaxed);
        self.pages_skipped.fetch_add(1, Ordering::Relaxed);
        scrapix_core::metrics::content_documents_total()
            .with_label_values(&["skipped"])
            .inc();
    }

    fn record_duplicate(&self) {
        self.pages_processed.fetch_add(1, Ordering::Relaxed);
        self.pages_duplicate.fetch_add(1, Ordering::Relaxed);
        scrapix_core::metrics::content_documents_total()
            .with_label_values(&["duplicate"])
            .inc();
    }

    fn record_indexed(&self, count: u64) {
        self.documents_indexed.fetch_add(count, Ordering::Relaxed);
    }

    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            pages_processed: self.pages_processed.load(Ordering::Relaxed),
            pages_succeeded: self.pages_succeeded.load(Ordering::Relaxed),
            pages_failed: self.pages_failed.load(Ordering::Relaxed),
            pages_skipped: self.pages_skipped.load(Ordering::Relaxed),
            pages_duplicate: self.pages_duplicate.load(Ordering::Relaxed),
            documents_created: self.documents_created.load(Ordering::Relaxed),
            documents_indexed: self.documents_indexed.load(Ordering::Relaxed),
            bytes_processed: self.bytes_processed.load(Ordering::Relaxed),
            active_processors: self.active_processors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
struct MetricsSnapshot {
    pages_processed: u64,
    pages_succeeded: u64,
    pages_failed: u64,
    pages_skipped: u64,
    pages_duplicate: u64,
    documents_created: u64,
    documents_indexed: u64,
    bytes_processed: u64,
    active_processors: u64,
}

/// AI configuration for the worker
#[derive(Clone)]
#[allow(dead_code)]
struct AiConfig {
    enable_summary: bool,
    enable_extraction: bool,
    extraction_prompt: Option<String>,
    summary_model: String,
    extraction_model: String,
    max_tokens: u32,
}

/// What processing a page produced.
enum PageOutcome {
    /// Index these documents (one per page, or one per block).
    Index {
        docs: Vec<Document>,
        /// `document_id` reported in `DocumentIndexed`
        document_id: String,
        ai_enriched: bool,
    },
    /// Nothing to index (reason goes into `DocumentSkipped`).
    Skipped(String),
    /// The page could not be turned into a document.
    Failed(String),
}

/// A page whose documents all reached a final Meilisearch outcome: publish
/// its event (`DocumentIndexed`, or `DocumentFailed` if any document was
/// refused), then ack its message.
struct IndexedNotice {
    job_id: String,
    event: CrawlEvent,
    /// Documents indexed (0 when the page failed)
    docs: u64,
    ack: Ack,
}

/// Tracks one page's buffered documents until each is accepted or refused.
struct PageTracker {
    remaining: std::sync::atomic::AtomicUsize,
    error: parking_lot::Mutex<Option<String>>,
    notice: parking_lot::Mutex<Option<IndexedNotice>>,
    tx: mpsc::UnboundedSender<IndexedNotice>,
    pending: Arc<AtomicU64>,
}

impl PageTracker {
    /// One document finished (`Some(reason)` = refused). The last one sends
    /// the page's notice.
    fn done(&self, error: Option<String>) {
        if let Some(e) = error {
            self.error.lock().get_or_insert(e);
        }
        if self.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let Some(mut notice) = self.notice.lock().take() else {
            return;
        };
        if let Some(error) = self.error.lock().take() {
            if let CrawlEvent::DocumentIndexed {
                job_id,
                url,
                url_message_id,
                ..
            } = &notice.event
            {
                notice.event = CrawlEvent::DocumentFailed {
                    job_id: job_id.clone(),
                    url: url.clone(),
                    url_message_id: url_message_id.clone(),
                    error,
                    timestamp: now_ms(),
                };
            }
            notice.docs = 0;
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(notice).is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Per-job Meilisearch target: `(url, api key, index uid, primary key,
/// batch size)`. One buffered storage per key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StorageKey {
    url: String,
    api_key: String,
    index_uid: String,
    primary_key: String,
    batch_size: usize,
}

/// Schema extractor cache key: `(sorted only_types, convert_dates)`.
type SchemaKey = (Vec<String>, bool);

/// Bound on remembered `(job_id, message)` warnings before the set resets.
const MAX_REMEMBERED_WARNINGS: usize = 10_000;

/// The main content worker
struct ContentWorker {
    consumer: Arc<AnyConsumer>,
    producer: Arc<AnyProducer>,
    parser: HtmlParser,
    /// `false` only with `SKIP_MEILISEARCH`. A Meilisearch that is
    /// unreachable at startup does not disable indexing: per-job storages
    /// connect lazily and retry, and nothing is acked until accepted.
    indexing_enabled: bool,
    /// Default Meilisearch URL (from env/args)
    default_meilisearch_url: String,
    /// Default Meilisearch API key (from env/args)
    default_meilisearch_key: String,
    /// Per-job buffered storages. A storage's index is configured (created,
    /// settings applied unless `keep_settings`) when it is first created.
    storage_cache: tokio::sync::Mutex<HashMap<StorageKey, Arc<MeilisearchStorage>>>,
    ai_service: Option<Arc<AiService>>,
    ai_config: AiConfig,
    ai_usage_rx: parking_lot::Mutex<Option<scrapix_ai::AiUsageReceiver>>,
    dedup_detector: Option<Arc<NearDuplicateDetector>>,
    block_splitter: Option<BlockSplitter>,
    /// Schema extractors for jobs with `schema.only_types` / `convert_dates`,
    /// keyed by those options.
    schema_extractors: parking_lot::Mutex<HashMap<SchemaKey, Arc<SchemaExtractor>>>,
    /// `(job_id, message)` pairs already reported as `JobWarning`.
    warned: parking_lot::Mutex<HashSet<(String, String)>>,
    indexed_tx: mpsc::UnboundedSender<IndexedNotice>,
    indexed_rx: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<IndexedNotice>>>,
    /// Indexed notices sent but not yet published/acked.
    pending_notices: Arc<AtomicU64>,
    metrics: Arc<WorkerMetrics>,
    shutdown: Arc<AtomicBool>,
    worker_id: String,
    concurrency: usize,
    publish_to_kafka: bool,
    publish_history: bool,
    /// Default batch size for per-job storages without `meilisearch.batch_size`
    default_batch_size: usize,
    /// Default feature config built from CLI args, used when message has no features
    default_features: FeaturesConfig,
    /// Jobs cancelled or finished (from `JOB_STATUS`): their pages are
    /// acked without indexing (spec R5).
    cancelled: Arc<CancelledJobs>,
    /// `JOB_STATUS` consumer feeding `cancelled` (per-worker group).
    control_consumer: Option<Arc<AnyConsumer>>,
}

impl ContentWorker {
    /// Create a new content worker (Kafka bus)
    async fn new(args: &Args) -> anyhow::Result<Self> {
        let worker_id = worker_id(args);
        info!(worker_id = %worker_id, "Initializing content worker");
        if args.worker_id.is_none() {
            warn!(
                worker_id = %worker_id,
                "WORKER_ID is not set: using a random id, so the job-control consumer group \
                 changes on every restart (controls published while down are missed and old \
                 groups leak). Set a stable WORKER_ID per worker instance."
            );
        }

        let kafka_consumer = ConsumerBuilder::new(&args.brokers, &args.group_id)
            .client_id(format!("scrapix-content-{}", worker_id))
            .auto_offset_reset("earliest")
            .build()?;
        kafka_consumer.subscribe(&[topic_names::PAGES_RAW])?;
        info!(
            topic = topic_names::PAGES_RAW,
            "Subscribed to raw pages topic"
        );

        let kafka_producer = ProducerBuilder::new(&args.brokers)
            .client_id(format!("scrapix-content-{}-producer", worker_id))
            .compression("lz4")
            .build()?;

        // Job control: a per-worker group, so every worker sees every
        // cancel; `latest` so a new group does not replay the history.
        let control_group = control_group_id(&args.group_id, &worker_id);
        let control = ConsumerBuilder::new(&args.brokers, &control_group)
            .client_id(format!("scrapix-content-{}-control", worker_id))
            .auto_offset_reset("latest")
            .build()?;
        control.subscribe(&[topic_names::JOB_STATUS])?;
        info!(topic = topic_names::JOB_STATUS, group = %control_group, "Subscribed to job control topic");

        startup_check(args).await;
        let mut worker = Self::build(
            args,
            worker_id,
            Arc::new(kafka_consumer.into()),
            Arc::new(kafka_producer.into()),
            !args.skip_meilisearch,
        );
        worker.control_consumer = Some(Arc::new(control.into()));
        Ok(worker)
    }

    /// Create a content worker with pre-built message bus objects.
    ///
    /// Used by `scrapix all` to inject in-process channel producer/consumer instead of Kafka.
    /// The caller is responsible for subscribing the consumer to the appropriate topic before
    /// calling this function (and `control`, when given, to `JOB_STATUS`).
    async fn with_bus(
        args: &Args,
        consumer: Arc<AnyConsumer>,
        producer: Arc<AnyProducer>,
        control: Option<Arc<AnyConsumer>>,
    ) -> anyhow::Result<Self> {
        let worker_id = worker_id(args);
        info!(worker_id = %worker_id, "Initializing content worker (pre-built bus)");
        startup_check(args).await;
        let mut worker = Self::build(args, worker_id, consumer, producer, !args.skip_meilisearch);
        worker.control_consumer = control;
        Ok(worker)
    }

    /// Shared construction once the bus exists.
    fn build(
        args: &Args,
        worker_id: String,
        consumer: Arc<AnyConsumer>,
        producer: Arc<AnyProducer>,
        indexing_enabled: bool,
    ) -> Self {
        // Always extract everything; per-job features are applied as a
        // post-parse filter so the parser can be reused.
        let parser = HtmlParser::new(HtmlParserConfig {
            extract_content: true,
            convert_to_markdown: true,
            detect_language: true,
            extract_schema: true,
            extract_og_tags: true,
            min_content_length: args.min_content_length,
        });

        let default_features = FeaturesConfig::from_cli_args(
            args.extract_content,
            args.convert_markdown,
            args.extract_schema,
            args.enable_block_split,
            args.enable_summary,
            args.enable_extraction,
            args.extraction_prompt.clone(),
        );

        let ai_config = AiConfig {
            enable_summary: args.enable_summary,
            enable_extraction: args.enable_extraction,
            extraction_prompt: args.extraction_prompt.clone(),
            summary_model: args.summary_model.clone(),
            extraction_model: args.extraction_model.clone(),
            max_tokens: args.ai_max_tokens,
        };

        // The AI service is built whenever an AI provider is configured
        // (AI_PROVIDER + its API key), so per-job `ai_summary` /
        // `ai_extraction` work without worker-level ENABLE_* flags (those
        // only set the default features for messages without features).
        let (ai_service, ai_usage_rx) = match AiClient::from_env_with_tracking() {
            Ok((client, rx)) => {
                let service = AiService::minimal(Arc::new(client))
                    .with_summarization()
                    .with_extraction();
                info!("AI provider configured; AI enrichment available to jobs");
                (Some(Arc::new(service)), Some(rx))
            }
            Err(e) => {
                if args.enable_summary || args.enable_extraction {
                    warn!(error = %e, "Failed to create AI client, AI features disabled");
                } else {
                    info!(reason = %e, "No AI provider configured; AI enrichment unavailable");
                }
                (None, None)
            }
        };

        let dedup_detector = if args.enable_dedup {
            let config = NearDuplicateConfig {
                use_simhash: args.dedup_use_simhash,
                simhash_threshold: args.dedup_simhash_threshold,
                minhash_threshold: args.dedup_minhash_threshold,
                max_fingerprints: args.dedup_max_fingerprints,
                ..Default::default()
            };
            info!(
                use_simhash = args.dedup_use_simhash,
                simhash_threshold = args.dedup_simhash_threshold,
                minhash_threshold = args.dedup_minhash_threshold,
                max_fingerprints = args.dedup_max_fingerprints,
                "Near-duplicate detection enabled (scoped per index)"
            );
            Some(Arc::new(NearDuplicateDetector::new(config)))
        } else {
            None
        };

        let block_splitter = if args.enable_block_split {
            info!(
                min_level = args.block_split_min_level,
                max_level = args.block_split_max_level,
                min_content_length = args.block_split_min_length,
                "Block splitting enabled - will create multiple documents per page"
            );
            Some(BlockSplitter::new(BlockConfig {
                min_level: args.block_split_min_level,
                max_level: args.block_split_max_level,
                min_content_length: args.block_split_min_length,
                include_hierarchy: true,
                extract_anchors: true,
                ..Default::default()
            }))
        } else {
            None
        };

        let (indexed_tx, indexed_rx) = mpsc::unbounded_channel();

        Self {
            consumer,
            producer,
            parser,
            indexing_enabled,
            default_meilisearch_url: args.meilisearch_url.clone(),
            default_meilisearch_key: args.meilisearch_key.clone().unwrap_or_default(),
            storage_cache: tokio::sync::Mutex::new(HashMap::new()),
            ai_service,
            ai_config,
            ai_usage_rx: parking_lot::Mutex::new(ai_usage_rx),
            dedup_detector,
            block_splitter,
            schema_extractors: parking_lot::Mutex::new(HashMap::new()),
            warned: parking_lot::Mutex::new(HashSet::new()),
            indexed_tx,
            indexed_rx: parking_lot::Mutex::new(Some(indexed_rx)),
            pending_notices: Arc::new(AtomicU64::new(0)),
            metrics: Arc::new(WorkerMetrics::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            worker_id,
            concurrency: args.concurrency.max(1),
            publish_to_kafka: args.publish_to_kafka,
            publish_history: args.publish_history,
            default_batch_size: args.batch_size.max(1),
            default_features,
            cancelled: Arc::new(CancelledJobs::default()),
            control_consumer: None,
        }
    }

    /// The Meilisearch target for a message: `(url, key)` from the message
    /// (falling back to the worker defaults), the message's index, and the
    /// job's `primary_key` / `batch_size`.
    fn storage_key(&self, msg: &RawPageMessage) -> StorageKey {
        let msg_url = msg.meilisearch_url.as_deref().unwrap_or("");
        let (url, api_key) = if msg_url.is_empty() {
            (
                self.default_meilisearch_url.clone(),
                self.default_meilisearch_key.clone(),
            )
        } else {
            (
                msg_url.to_string(),
                msg.meilisearch_api_key.clone().unwrap_or_default(),
            )
        };
        let job = msg.job.as_ref();
        StorageKey {
            url,
            api_key,
            index_uid: msg.index_uid.clone(),
            primary_key: job
                .and_then(|j| j.primary_key.clone())
                .filter(|pk| !pk.is_empty())
                .unwrap_or_else(|| "uid".to_string()),
            batch_size: job
                .and_then(|j| j.batch_size)
                .map(|b| b as usize)
                .filter(|b| *b > 0)
                .unwrap_or(self.default_batch_size),
        }
    }

    /// Get (or create) the buffered storage for a message and make sure
    /// the job's index configuration is applied.
    ///
    /// `Ok(None)` = indexing disabled on this worker. `Err` = the job's
    /// Meilisearch target is unusable (never falls back to another
    /// Meilisearch, which could leak a tenant's documents).
    async fn storage_for(
        &self,
        msg: &RawPageMessage,
        features: &FeaturesConfig,
    ) -> Result<Option<Arc<MeilisearchStorage>>, String> {
        if !self.indexing_enabled {
            return Ok(None);
        }
        let key = self.storage_key(msg);
        // The map lock covers only the lookup and the (network-free)
        // client construction, never Meilisearch I/O.
        let storage = {
            let mut cache = self.storage_cache.lock().await;
            match cache.get(&key) {
                Some(storage) => storage.clone(),
                None => {
                    let mut builder = MeilisearchStorageBuilder::new(&key.url, &key.index_uid)
                        .primary_key(&key.primary_key)
                        .batch_size(key.batch_size);
                    if !key.api_key.is_empty() {
                        builder = builder.api_key(&key.api_key);
                    }
                    let storage = Arc::new(
                        builder
                            .connect()
                            .map_err(|e| format!("Invalid Meilisearch target: {e}"))?,
                    );
                    info!(
                        url = %key.url,
                        index = %key.index_uid,
                        primary_key = %key.primary_key,
                        batch_size = key.batch_size,
                        "Created per-job Meilisearch storage"
                    );
                    cache.insert(key.clone(), storage.clone());
                    storage
                }
            }
        };
        // Once per distinct job configuration; concurrent pages of the same
        // job wait here so settings go out before their documents.
        storage
            .ensure_configured(&key.index_uid, features, msg.job.as_ref())
            .await;
        Ok(Some(storage))
    }

    /// Flush all buffered storages (acks follow Meilisearch acceptance).
    async fn flush_all_storages(&self) {
        let storages: Vec<(StorageKey, Arc<MeilisearchStorage>)> = self
            .storage_cache
            .lock()
            .await
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        for (key, storage) in storages {
            let flush_started = Instant::now();
            let result = storage.flush().await;
            scrapix_core::metrics::content_flush_duration_seconds()
                .observe(flush_started.elapsed().as_secs_f64());
            match result {
                Ok(count) if count > 0 => {
                    debug!(count, index = %key.index_uid, "Flushed Meilisearch storage");
                }
                Err(e) => warn!(
                    error = %e,
                    index = %key.index_uid,
                    pending = storage.pending_count(),
                    "Failed to flush Meilisearch storage; documents kept for retry"
                ),
                _ => {}
            }
        }
    }

    /// Spawn the task that publishes `DocumentIndexed` for accepted pages
    /// and then acks their messages. Call once.
    fn spawn_indexed_publisher(&self) -> Option<tokio::task::JoinHandle<()>> {
        let mut rx = self.indexed_rx.lock().take()?;
        let producer = self.producer.clone();
        let metrics = self.metrics.clone();
        let pending = self.pending_notices.clone();
        Some(tokio::spawn(async move {
            while let Some(notice) = rx.recv().await {
                if publish_with_retry(&producer, &notice.job_id, &notice.event).await {
                    notice.ack.ack();
                    if notice.docs > 0 {
                        metrics.record_indexed(notice.docs);
                    } else {
                        metrics.record_failure();
                    }
                }
                // Not published: the ack is dropped, the message stays
                // un-acked and is redelivered.
                pending.fetch_sub(1, Ordering::AcqRel);
            }
        }))
    }

    /// Spawn the task forwarding AI usage events as `AiUsage` crawl events
    /// (reaching ClickHouse through the events topic, R9). Call once.
    fn spawn_ai_usage_forwarder(&self) -> Option<tokio::task::JoinHandle<()>> {
        let mut rx = self.ai_usage_rx.lock().take()?;
        let producer = self.producer.clone();
        Some(tokio::spawn(async move {
            while let Some(usage) = rx.recv().await {
                let Some(event) = ai_usage_event(usage) else {
                    continue;
                };
                let job_id = match &event {
                    CrawlEvent::AiUsage { job_id, .. } => job_id.clone(),
                    _ => String::new(),
                };
                if !publish_with_retry(&producer, &job_id, &event).await {
                    error!(job_id = %job_id, "Dropped AiUsage event after retries (billing data)");
                }
            }
        }))
    }

    /// Wait (bounded) until `cond` holds.
    async fn wait_until(timeout: Duration, cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while !cond() {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    /// Flush every storage and wait (bounded) for the resulting
    /// `DocumentIndexed` publishes and acks to go through.
    async fn drain(&self) {
        let _ = Self::wait_until(Duration::from_secs(30), || {
            self.metrics.active_processors.load(Ordering::Acquire) == 0
        })
        .await;
        self.flush_all_storages().await;
        if !Self::wait_until(Duration::from_secs(10), || {
            self.pending_notices.load(Ordering::Acquire) == 0
        })
        .await
        {
            warn!("Timed out publishing DocumentIndexed events during drain");
        }
    }

    /// Run the content worker
    async fn run(self: &Arc<Self>) -> anyhow::Result<()> {
        info!(worker_id = %self.worker_id, concurrency = self.concurrency, "Starting content worker main loop");

        let metrics = self.metrics.clone();
        let shutdown = self.shutdown.clone();
        let metrics_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            while !shutdown.load(Ordering::Relaxed) {
                interval.tick().await;
                let snapshot = metrics.snapshot();
                info!(
                    processed = snapshot.pages_processed,
                    succeeded = snapshot.pages_succeeded,
                    failed = snapshot.pages_failed,
                    skipped = snapshot.pages_skipped,
                    duplicates = snapshot.pages_duplicate,
                    docs_created = snapshot.documents_created,
                    docs_indexed = snapshot.documents_indexed,
                    bytes_mb = snapshot.bytes_processed / (1024 * 1024),
                    active = snapshot.active_processors,
                    "Worker metrics"
                );
            }
        });

        let publisher_handle = self.spawn_indexed_publisher();
        let ai_usage_handle = self.spawn_ai_usage_forwarder();
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

        // The consumer is stopped only after the final flush, so acks from
        // documents buffered at shutdown still reach its last commit.
        let consumer_stop = Arc::new(AtomicBool::new(false));
        let consumer_done = Arc::new(AtomicBool::new(false));

        // Periodic flush so documents are indexed (and acked) even when a
        // batch never fills up.
        let flush_handle = if self.indexing_enabled {
            let worker = self.clone();
            let done = consumer_stop.clone();
            Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(5));
                while !done.load(Ordering::Relaxed) {
                    interval.tick().await;
                    worker.flush_all_storages().await;
                }
            }))
        } else {
            None
        };

        let coordinator = {
            let worker = self.clone();
            let (stop, done) = (consumer_stop.clone(), consumer_done.clone());
            async move {
                while !worker.shutdown.load(Ordering::Relaxed) && !done.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if !done.load(Ordering::Relaxed) {
                    info!("Shutdown requested: flushing buffered documents before stopping the consumer");
                    worker.drain().await;
                }
                stop.store(true, Ordering::Relaxed);
            }
        };

        let processing = {
            let worker = self.clone();
            let (stop, done) = (consumer_stop.clone(), consumer_done.clone());
            async move {
                let result = worker.process_messages(stop).await;
                done.store(true, Ordering::Relaxed);
                result
            }
        };

        let (result, ()) = tokio::join!(processing, coordinator);

        // Cleanup
        self.shutdown.store(true, Ordering::Relaxed);
        consumer_stop.store(true, Ordering::Relaxed);
        metrics_handle.abort();
        if let Some(handle) = flush_handle {
            // Wait for any in-flight flush to complete instead of aborting
            let _ = handle.await;
        }
        // Final flush (the in-process bus has no commits; with Kafka any
        // ack that lands after the consumer's last commit is redelivered,
        // which is correct for at-least-once).
        self.drain().await;
        if let Some(handle) = publisher_handle {
            handle.abort();
        }
        if let Some(handle) = ai_usage_handle {
            handle.abort();
        }
        if let Some(handle) = control_handle {
            handle.abort();
        }

        result
    }

    /// Process messages from the raw pages queue with `CONCURRENCY`
    /// in-flight pages, each finished by an explicit ack.
    async fn process_messages(self: Arc<Self>, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        let worker = self.clone();
        self.consumer
            .process_with_ack::<RawPageMessage, _, _>(
                move |msg, metadata, ack| {
                    let worker = worker.clone();
                    async move {
                        debug!(
                            url = %msg.url,
                            job_id = %msg.job_id,
                            partition = metadata.partition,
                            offset = metadata.offset,
                            "Received raw page for processing"
                        );
                        worker.handle_message(msg, ack).await;
                    }
                },
                self.concurrency,
                stop,
            )
            .await?;
        Ok(())
    }

    /// Process one raw page and finish it with exactly one outcome event.
    async fn handle_message(&self, msg: RawPageMessage, ack: Ack) {
        if self.shutdown.load(Ordering::Relaxed) {
            // Not processed: left un-acked for redelivery.
            return;
        }
        if self.cancelled.contains(&msg.job_id) {
            // R5: a cancelled/finished job is not indexed. (Documents
            // already buffered for it still flush with their batch.)
            debug!(url = %msg.url, job_id = %msg.job_id, "Skipping page of a cancelled job");
            ack.ack();
            return;
        }
        self.metrics
            .active_processors
            .fetch_add(1, Ordering::AcqRel);
        let outcome = self.process_page(&msg).await;
        match outcome {
            PageOutcome::Skipped(reason) => {
                debug!(url = %msg.url, reason = %reason, "Page skipped");
                let event = CrawlEvent::DocumentSkipped {
                    job_id: msg.job_id.clone(),
                    url: msg.url.clone(),
                    url_message_id: msg.url_message_id.clone(),
                    reason,
                    timestamp: now_ms(),
                };
                self.finish(&msg, event, ack).await;
            }
            PageOutcome::Failed(error) => {
                warn!(url = %msg.url, job_id = %msg.job_id, error = %error, "Failed to process page");
                self.metrics.record_failure();
                let event = CrawlEvent::DocumentFailed {
                    job_id: msg.job_id.clone(),
                    url: msg.url.clone(),
                    url_message_id: msg.url_message_id.clone(),
                    error,
                    timestamp: now_ms(),
                };
                self.finish(&msg, event, ack).await;
            }
            PageOutcome::Index {
                docs,
                document_id,
                ai_enriched,
            } => {
                self.index_page(&msg, docs, document_id, ai_enriched, ack)
                    .await;
            }
        }
        self.metrics
            .active_processors
            .fetch_sub(1, Ordering::AcqRel);
    }

    /// Publish a terminal event, then ack (a failed publish leaves the
    /// message un-acked for redelivery).
    async fn finish(&self, msg: &RawPageMessage, event: CrawlEvent, ack: Ack) {
        if publish_with_retry(&self.producer, &msg.job_id, &event).await {
            ack.ack();
        }
    }

    /// Hand a page's documents to Meilisearch; `DocumentIndexed` + ack
    /// happen once Meilisearch accepted all of them (`DocumentFailed` + ack
    /// if any was refused permanently or timed out on backpressure).
    async fn index_page(
        &self,
        msg: &RawPageMessage,
        docs: Vec<Document>,
        document_id: String,
        ai_enriched: bool,
        ack: Ack,
    ) {
        // Resolve the target first: a page that cannot be indexed must not
        // be published to the documents topic or crawl history.
        let features = self.resolve_features(msg);
        let storage = match self.storage_for(msg, &features).await {
            Ok(storage) => storage,
            Err(error) => {
                warn!(url = %msg.url, error = %error, "Cannot index page");
                self.metrics.record_failure();
                let failed = CrawlEvent::DocumentFailed {
                    job_id: msg.job_id.clone(),
                    url: msg.url.clone(),
                    url_message_id: msg.url_message_id.clone(),
                    error,
                    timestamp: now_ms(),
                };
                self.finish(msg, failed, ack).await;
                return;
            }
        };

        // Publish documents to Kafka if enabled (a failure leaves the page
        // un-acked; redelivery re-publishes).
        if self.publish_to_kafka {
            for doc in &docs {
                let doc_msg = DocumentMessage::new(doc.clone(), &msg.job_id, &msg.index_uid);
                if let Err(e) = self
                    .producer
                    .send(topic_names::DOCUMENTS, Some(&msg.job_id), &doc_msg)
                    .await
                {
                    warn!(url = %msg.url, error = %e, "Failed to publish document to Kafka");
                    return;
                }
            }
        }

        self.publish_history(msg).await;

        let Some(storage) = storage else {
            // Indexing explicitly disabled (SKIP_MEILISEARCH): nothing was
            // indexed, so the page is reported as skipped.
            let skipped = CrawlEvent::DocumentSkipped {
                job_id: msg.job_id.clone(),
                url: msg.url.clone(),
                url_message_id: msg.url_message_id.clone(),
                reason: "indexing disabled".to_string(),
                timestamp: now_ms(),
            };
            self.finish(msg, skipped, ack).await;
            return;
        };

        let tracker = Arc::new(PageTracker {
            remaining: std::sync::atomic::AtomicUsize::new(docs.len()),
            error: parking_lot::Mutex::new(None),
            notice: parking_lot::Mutex::new(Some(IndexedNotice {
                job_id: msg.job_id.clone(),
                event: CrawlEvent::DocumentIndexed {
                    job_id: msg.job_id.clone(),
                    account_id: msg.account_id.clone(),
                    url: msg.url.clone(),
                    document_id,
                    timestamp: now_ms(),
                    url_message_id: msg.url_message_id.clone(),
                    ai_enriched,
                },
                docs: docs.len() as u64,
                ack,
            })),
            tx: self.indexed_tx.clone(),
            pending: self.pending_notices.clone(),
        });

        for doc in docs {
            let (on_accept, on_reject) = (tracker.clone(), tracker.clone());
            let doc_ack = DocAck::new(Ack::from_fn(move || on_accept.done(None)))
                .on_reject(move |reason| on_reject.done(Some(reason)));
            if let Err(e) = storage
                .add_document_to_index(doc, &msg.index_uid, doc_ack)
                .await
            {
                warn!(url = %msg.url, error = %e, "Failed to buffer document for Meilisearch");
            }
        }
    }

    /// Publish crawl history for recrawl scheduling if enabled
    async fn publish_history(&self, msg: &RawPageMessage) {
        if !self.publish_history {
            return;
        }
        use sha2::{Digest, Sha256};

        let content_hash = {
            let mut hasher = Sha256::new();
            hasher.update(msg.html.as_bytes());
            hex::encode(hasher.finalize())
        };

        let mut history_msg = CrawlHistoryMessage::new(&msg.url, msg.status, &msg.job_id)
            .with_content_hash(&content_hash)
            .with_content_length(msg.html.len() as u64)
            .with_content_changed(true); // Assume changed since we processed it
        if let Some(ref etag) = msg.etag {
            history_msg = history_msg.with_etag(etag);
        }
        if let Some(ref last_modified) = msg.last_modified {
            history_msg = history_msg.with_last_modified(last_modified);
        }

        if let Err(e) = self
            .producer
            .send(topic_names::CRAWL_HISTORY, Some(&msg.job_id), &history_msg)
            .await
        {
            debug!(error = %e, "Failed to publish crawl history");
        }
    }

    /// Publish `JobWarning{message}` at most once per job per worker.
    async fn warn_job_once(&self, job_id: &str, message: &str) {
        let key = (job_id.to_string(), message.to_string());
        {
            let mut warned = self.warned.lock();
            if warned.len() >= MAX_REMEMBERED_WARNINGS {
                warned.clear();
            }
            if !warned.insert(key.clone()) {
                return;
            }
        }
        warn!(job_id = %job_id, warning = %message, "Job warning");
        let event = CrawlEvent::JobWarning {
            job_id: job_id.to_string(),
            message: message.to_string(),
            timestamp: now_ms(),
        };
        if let Err(e) = self
            .producer
            .send(topic_names::EVENTS, Some(job_id), &event)
            .await
        {
            // Best effort; forget it so a later page retries the warning.
            debug!(error = %e, "Failed to publish JobWarning");
            self.warned.lock().remove(&key);
        }
    }

    /// Process a single raw page into an outcome.
    async fn process_page(&self, msg: &RawPageMessage) -> PageOutcome {
        // Defense in depth (R1): current crawlers only forward 2xx pages.
        if !(200..300).contains(&msg.status) {
            self.metrics.record_skipped();
            return PageOutcome::Skipped("non-2xx status".to_string());
        }

        let index_only = msg
            .job
            .as_ref()
            .map(|j| j.index_only.as_slice())
            .unwrap_or_default();
        if !features::index_only_allows(index_only, &msg.url) {
            self.metrics.record_skipped();
            return PageOutcome::Skipped("index_only".to_string());
        }

        // Route by content type: PDFs and markdown each get their own path,
        // other non-HTML is skipped.
        if let Some(ref content_type) = msg.content_type {
            if content_type.contains("application/pdf") {
                return self.process_pdf_page(msg).await;
            }
            if content_type.contains("text/markdown") {
                return self.process_markdown_page(msg).await;
            }
            if !content_type.contains("text/html") && !content_type.contains("application/xhtml") {
                self.metrics.record_skipped();
                return PageOutcome::Skipped(format!("unsupported content type {content_type}"));
            }
        }

        let start = Instant::now();
        let page_size = msg.html.len() as u64;

        let raw_page = RawPage {
            url: msg.url.clone(),
            final_url: msg.final_url.clone(),
            status: msg.status,
            headers: HashMap::new(),
            html: msg.html.clone(),
            content_type: msg.content_type.clone(),
            js_rendered: msg.js_rendered,
            fetched_at: chrono::DateTime::from_timestamp_millis(msg.fetched_at)
                .unwrap_or_else(chrono::Utc::now),
            fetch_duration_ms: msg.fetch_duration_ms,
        };

        let mut document = match self.parser.parse(&raw_page) {
            Ok(doc) => doc,
            Err(e) => return PageOutcome::Failed(format!("Parse error: {}", e)),
        };

        let features = features::page_features(&self.resolve_features(msg), &msg.url);
        self.stamp(&mut document, msg);
        Self::filter_document(&mut document, &features);
        if features.schema_enabled() {
            if let Some(extractor) = self.schema_extractor_for(&features) {
                document.schema = features::extract_schema(&extractor, &msg.html);
            }
        }

        if features.custom_selectors_enabled() {
            document.custom = Self::extract_custom_selectors(&msg.html, &features);
        }

        let content_len = document.content.as_ref().map(|c| c.len()).unwrap_or(0);
        if content_len == 0 {
            self.metrics.record_skipped();
            return PageOutcome::Skipped("no content".to_string());
        }

        if let Some(reason) = self.near_duplicate(&document, msg) {
            return PageOutcome::Skipped(reason);
        }

        info!(
            url = %msg.url,
            title = ?document.title,
            content_len = content_len,
            language = ?document.language,
            duration_ms = start.elapsed().as_millis(),
            "Page parsed successfully"
        );

        if features.block_split_enabled() {
            if ai_requested(&features) {
                self.warn_job_once(&msg.job_id, WARN_AI_BLOCK_SPLIT).await;
            }
            let default_splitter;
            let splitter = match self.block_splitter.as_ref() {
                Some(s) => s,
                None => {
                    default_splitter = BlockSplitter::new(BlockConfig {
                        include_hierarchy: true,
                        extract_anchors: true,
                        ..Default::default()
                    });
                    &default_splitter
                }
            };
            match splitter.split(&msg.html) {
                Ok(extracted) if extracted.count > 0 => {
                    info!(url = %msg.url, block_count = extracted.count, "Split page into content blocks");
                    self.metrics
                        .record_success(page_size, extracted.count as u64);
                    let docs = extracted
                        .blocks
                        .iter()
                        .map(|block| Self::create_block_document(&document, block))
                        .collect();
                    return PageOutcome::Index {
                        docs,
                        document_id: format!("{}-blocks", document.uid),
                        ai_enriched: false,
                    };
                }
                Ok(_) => {
                    debug!(url = %msg.url, "No blocks extracted, indexing full document");
                }
                Err(e) => {
                    warn!(url = %msg.url, error = %e, "Block splitting failed, indexing full document");
                }
            }
            self.metrics.record_success(page_size, 1);
            return single(document, false);
        }

        let (document, ai_enriched) = self.enrich_with_ai(document, &features, msg).await;
        self.metrics.record_success(page_size, 1);
        single(document, ai_enriched)
    }

    /// Process a page that was returned as server-provided markdown (e.g. Cloudflare "Markdown for Agents")
    async fn process_markdown_page(&self, msg: &RawPageMessage) -> PageOutcome {
        let page_size = msg.html.len() as u64;
        let mut document = match scrapix_parser::parse_markdown_page(&msg.final_url, &msg.html) {
            Ok(doc) => doc,
            Err(e) => return PageOutcome::Failed(format!("Markdown parse error: {}", e)),
        };

        let content_len = document.content.as_ref().map(|c| c.len()).unwrap_or(0);
        if content_len == 0 {
            self.metrics.record_skipped();
            return PageOutcome::Skipped("no content".to_string());
        }
        if let Some(reason) = self.near_duplicate(&document, msg) {
            return PageOutcome::Skipped(reason);
        }

        let features = features::page_features(&self.resolve_features(msg), &msg.url);
        self.stamp(&mut document, msg);
        Self::filter_document(&mut document, &features);

        info!(
            url = %msg.url,
            title = ?document.title,
            content_len = content_len,
            "Markdown page parsed successfully (server-provided)"
        );

        // No block splitting for server-provided markdown
        let (document, ai_enriched) = self.enrich_with_ai(document, &features, msg).await;
        self.metrics.record_success(page_size, 1);
        single(document, ai_enriched)
    }

    /// Process a page whose Content-Type is `application/pdf`.
    ///
    /// The upstream fetcher base64-encodes PDF bytes into `msg.html` so they
    /// survive the JSON Kafka payload. Empty PDFs (scanned images without
    /// OCR) are indexed with empty content so operators see the URL.
    async fn process_pdf_page(&self, msg: &RawPageMessage) -> PageOutcome {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

        let bytes = match BASE64.decode(msg.html.as_bytes()) {
            Ok(b) => b,
            Err(e) => {
                return PageOutcome::Failed(format!("Failed to base64-decode PDF payload: {}", e))
            }
        };
        let pdf_bytes_len = bytes.len();

        let parsed = match scrapix_parser::pdf::parse_pdf_bytes(&bytes, &msg.url) {
            Ok(p) => p,
            Err(e) => return PageOutcome::Failed(format!("PDF parse error: {}", e)),
        };

        let fallback_title = scrapix_parser::pdf::title_from_url(&msg.final_url);
        let mut document = match scrapix_parser::pdf::build_pdf_document(
            &msg.final_url,
            pdf_bytes_len,
            parsed,
            fallback_title,
        ) {
            Ok(doc) => doc,
            Err(e) => return PageOutcome::Failed(format!("PDF document build error: {}", e)),
        };

        let features = features::page_features(&self.resolve_features(msg), &msg.url);
        self.stamp(&mut document, msg);
        Self::filter_document(&mut document, &features);

        info!(
            url = %msg.url,
            title = ?document.title,
            pdf_bytes = pdf_bytes_len,
            "PDF page parsed successfully"
        );

        // PDFs have no heading tree: no block splitting.
        let (document, ai_enriched) = self.enrich_with_ai(document, &features, msg).await;
        self.metrics.record_success(pdf_bytes_len as u64, 1);
        single(document, ai_enriched)
    }

    /// Stamp job id (Replace strategy stale cleanup) and source
    /// (multi-tenant indexing; falls back to the domain).
    fn stamp(&self, document: &mut Document, msg: &RawPageMessage) {
        document._crawl_job_id = Some(msg.job_id.clone());
        document.source = Some(
            msg.source
                .clone()
                .unwrap_or_else(|| document.domain.clone()),
        );
    }

    /// Near-duplicate check, scoped to the page's Meilisearch index (R9).
    /// Returns the skip reason for a duplicate.
    fn near_duplicate(&self, document: &Document, msg: &RawPageMessage) -> Option<String> {
        let detector = self.dedup_detector.as_ref()?;
        let content = document
            .content
            .as_ref()
            .or(document.markdown.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("");
        let key = self.storage_key(msg);
        let namespace = format!("{}|{}", key.url, key.index_uid);
        let duplicate_of = detector.check_and_insert(&namespace, &msg.url, content)?;
        self.metrics.record_duplicate();
        Some(format!("Near-duplicate of {}", duplicate_of))
    }

    /// The job's schema extractor, when it sets `only_types` / `convert_dates`.
    fn schema_extractor_for(&self, features: &FeaturesConfig) -> Option<Arc<SchemaExtractor>> {
        let key = features::schema_key(features)?;
        let mut cache = self.schema_extractors.lock();
        Some(
            cache
                .entry(key)
                .or_insert_with_key(|k| Arc::new(features::schema_extractor(k)))
                .clone(),
        )
    }

    /// Create a document from a content block
    fn create_block_document(parent_doc: &Document, block: &ContentBlock) -> Document {
        // Build URL with anchor if available
        let block_url = if let Some(ref anchor) = block.anchor {
            format!("{}#{}", parent_doc.url, anchor)
        } else {
            format!("{}#block-{}", parent_doc.url, block.index)
        };

        // Build title from heading hierarchy
        let block_title = if let Some(ref heading) = block.heading {
            match &parent_doc.title {
                Some(page_title) => Some(format!("{} - {}", page_title, heading)),
                None => Some(heading.clone()),
            }
        } else {
            parent_doc.title.clone()
        };

        // Build URL tags from heading hierarchy
        let urls_tags: Vec<String> = [&block.h1, &block.h2, &block.h3, &block.h4]
            .into_iter()
            .flatten()
            .cloned()
            .collect();

        Document {
            uid: Document::uid_from_url(&block_url),
            url: parent_doc.url.clone(),
            block_url: Some(block_url),
            domain: parent_doc.domain.clone(),
            source: parent_doc.source.clone(),
            title: block_title,
            urls_tags: if urls_tags.is_empty() {
                parent_doc.urls_tags.clone()
            } else {
                Some(urls_tags)
            },
            content: Some(block.content.clone()),
            markdown: block.markdown.clone(),
            metadata: parent_doc.metadata.clone(),
            language: parent_doc.language.clone(),
            crawled_at: parent_doc.crawled_at,
            // Block-specific fields
            parent_document_id: Some(parent_doc.uid.clone()),
            page_block: Some(block.index),
            h1: block.h1.clone(),
            h2: block.h2.clone(),
            h3: block.h3.clone(),
            h4: block.h4.clone(),
            h5: block.h5.clone(),
            h6: block.h6.clone(),
            anchor: block.anchor.clone(),
            // Fields not used for blocks
            schema: None,
            custom: None,
            ai_summary: None,
            ai_extraction: None,
            _crawl_job_id: parent_doc._crawl_job_id.clone(),
        }
    }

    /// Enrich document with AI-generated content (summary, extraction).
    /// Returns whether any enrichment was actually produced.
    async fn enrich_with_ai(
        &self,
        mut document: Document,
        features: &FeaturesConfig,
        msg: &RawPageMessage,
    ) -> (Document, bool) {
        if !ai_requested(features) {
            return (document, false);
        }
        let Some(ai_service) = self.ai_service.as_ref() else {
            self.warn_job_once(&msg.job_id, WARN_AI_NO_PROVIDER).await;
            return (document, false);
        };

        // Get content to process - prefer markdown, fallback to content
        let content = document
            .markdown
            .as_ref()
            .or(document.content.as_ref())
            .cloned()
            .unwrap_or_default();
        if content.is_empty() {
            return (document, false);
        }

        // Truncate content if too long (keep first ~6000 tokens worth),
        // on a valid UTF-8 char boundary.
        let content_for_ai = if content.len() > 24000 {
            let mut end = 24000;
            while end > 0 && !content.is_char_boundary(end) {
                end -= 1;
            }
            content[..end].to_string()
        } else {
            content
        };

        let context = |feature: &str| AiUsageContext {
            job_id: msg.job_id.clone(),
            account_id: msg.account_id.clone(),
            feature: feature.to_string(),
            url: msg.url.clone(),
        };

        // Independent calls run concurrently; each carries its own usage
        // attribution. Individual failures are logged.
        let summary_fut = AI_USAGE_CONTEXT.scope(context("ai_summary"), async {
            if !features.ai_summary_enabled() {
                return None;
            }
            match ai_service.tldr(&content_for_ai).await {
                Ok(summary) => Some(summary),
                Err(e) => {
                    warn!(url = %document.url, error = %e, "Failed to generate AI summary");
                    None
                }
            }
        });

        let extraction_fut = AI_USAGE_CONTEXT.scope(context("ai_extraction"), async {
            if !features.ai_extraction_enabled() {
                return None;
            }
            // Per-job extraction prompt, falling back to the CLI config
            let prompt = features
                .ai_extraction
                .as_ref()
                .map(|c| &c.prompt)
                .filter(|p| !p.is_empty())
                .or(self.ai_config.extraction_prompt.as_ref());
            let Some(prompt) = prompt else {
                warn!(url = %document.url, "AI extraction enabled but no prompt provided");
                return None;
            };
            match ai_service.extract(&content_for_ai, prompt).await {
                Ok(result) => Some(result.data),
                Err(e) => {
                    warn!(url = %document.url, error = %e, "Failed to run AI extraction");
                    None
                }
            }
        });

        let (summary, extraction) = tokio::join!(summary_fut, extraction_fut);
        let enriched = summary.is_some() || extraction.is_some();
        if summary.is_some() {
            document.ai_summary = summary;
        }
        if extraction.is_some() {
            document.ai_extraction = extraction;
        }
        (document, enriched)
    }

    /// Resolve per-job features: use message features if present, otherwise fall back to CLI defaults
    fn resolve_features(&self, msg: &RawPageMessage) -> FeaturesConfig {
        msg.features
            .clone()
            .unwrap_or_else(|| self.default_features.clone())
    }

    /// Null out document fields that the page's features say should be disabled
    fn filter_document(document: &mut Document, features: &FeaturesConfig) {
        if !features.metadata_enabled() {
            document.metadata = None;
        }
        if !features.markdown_enabled() {
            document.markdown = None;
        }
        if !features.schema_enabled() {
            document.schema = None;
        }
    }

    /// Extract custom CSS selectors from HTML if the per-job config enables them
    fn extract_custom_selectors(
        html: &str,
        features: &FeaturesConfig,
    ) -> Option<HashMap<String, serde_json::Value>> {
        let config = features.custom_selectors.as_ref()?;
        if !config.enabled || config.selectors.is_empty() {
            return None;
        }

        let simple: HashMap<String, String> = config
            .selectors
            .iter()
            .map(|(field, def)| {
                let selector = match def {
                    scrapix_core::SelectorDef::Single(s) => s.clone(),
                    scrapix_core::SelectorDef::Multiple(v) => v.join(", "),
                };
                (field.clone(), selector)
            })
            .collect();

        let extractor = SelectorExtractor::from_simple(simple);
        match extractor.extract(html) {
            Ok(extracted) if !extracted.values.is_empty() => Some(extracted.values),
            _ => None,
        }
    }
}

fn worker_id(args: &Args) -> String {
    args.worker_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string())
}

/// Startup connectivity check: initializes the default index. A failure is
/// only logged; indexing stays enabled (per-job storages connect lazily and
/// retry until Meilisearch accepts their documents).
async fn startup_check(args: &Args) {
    if args.skip_meilisearch {
        info!("Meilisearch indexing disabled (SKIP_MEILISEARCH)");
        return;
    }
    let mut builder = MeilisearchStorageBuilder::new(&args.meilisearch_url, &args.default_index)
        .batch_size(args.batch_size);
    if let Some(ref key) = args.meilisearch_key {
        builder = builder.api_key(key);
    }
    // `build()` waits for the index-creation task; bound it so a
    // blackholed Meilisearch cannot hang startup.
    let timeout = scrapix_storage::MeilisearchConfig::default().timeout;
    match tokio::time::timeout(timeout, builder.build()).await {
        Ok(Ok(_)) => info!(
            url = %args.meilisearch_url,
            index = %args.default_index,
            "Connected to Meilisearch"
        ),
        Ok(Err(e)) => warn!(
            error = %e,
            "Meilisearch not reachable at startup; indexing stays enabled and will retry"
        ),
        Err(_) => warn!(
            timeout_secs = timeout.as_secs(),
            "Meilisearch startup check timed out; indexing stays enabled and will retry"
        ),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn single(document: Document, ai_enriched: bool) -> PageOutcome {
    PageOutcome::Index {
        document_id: document.uid.clone(),
        docs: vec![document],
        ai_enriched,
    }
}

fn ai_requested(features: &FeaturesConfig) -> bool {
    features.ai_summary_enabled() || features.ai_extraction_enabled()
}

/// Publish an event to the events topic, retrying transient failures.
/// Returns whether it was published.
async fn publish_with_retry(producer: &AnyProducer, job_id: &str, event: &CrawlEvent) -> bool {
    let mut delay = Duration::from_millis(100);
    for attempt in 1..=3 {
        match producer
            .send(topic_names::EVENTS, Some(job_id), event)
            .await
        {
            Ok(_) => return true,
            Err(e) => {
                warn!(job_id = %job_id, attempt, error = %e, "Failed to publish event");
                if attempt < 3 {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
    }
    false
}

/// Convert an AI client usage event into an `AiUsage` crawl event. Calls
/// made outside a job context (no attribution) are not reported.
fn ai_usage_event(usage: AiUsageEvent) -> Option<CrawlEvent> {
    let ctx = usage.context?;
    Some(CrawlEvent::AiUsage {
        job_id: ctx.job_id,
        account_id: ctx.account_id,
        provider: usage.provider,
        model: usage.model,
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        duration_ms: usage.duration_ms,
        feature: ctx.feature,
        url: ctx.url,
        timestamp: usage.timestamp.timestamp_millis(),
    })
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    info!(
        concurrency = args.concurrency,
        brokers = %args.brokers,
        group_id = %args.group_id,
        meilisearch = %args.meilisearch_url,
        "Starting Scrapix content worker"
    );

    let worker = Arc::new(ContentWorker::new(&args).await?);

    // Install SIGTERM + Ctrl-C handler.
    let signal_handle = install_signal_handlers(worker.shutdown.clone());

    // Wake listener for Fly.io autostart.
    let wake_handle = spawn_wake_listener(wake_port_from_env(), worker.shutdown.clone());

    // Idle watchdog: exit cleanly after IDLE_EXIT_MINUTES of no processed pages.
    let idle_metrics = worker.metrics.clone();
    let idle_handle = spawn_idle_watchdog(
        move || idle_metrics.pages_processed.load(Ordering::Relaxed),
        idle_minutes_from_env(10.0),
        worker.shutdown.clone(),
    );

    let result = worker.run().await;

    signal_handle.abort();
    wake_handle.abort();
    idle_handle.abort();

    let metrics = worker.metrics.snapshot();
    info!(
        processed = metrics.pages_processed,
        succeeded = metrics.pages_succeeded,
        failed = metrics.pages_failed,
        skipped = metrics.pages_skipped,
        duplicates = metrics.pages_duplicate,
        docs_created = metrics.documents_created,
        docs_indexed = metrics.documents_indexed,
        bytes_mb = metrics.bytes_processed / (1024 * 1024),
        "Final worker metrics"
    );

    result
}

/// Run the content worker using pre-built message bus trait objects.
///
/// Used by `scrapix all` to run the content worker in-process alongside other services.
pub async fn run_with_bus(
    args: Args,
    consumer: Arc<AnyConsumer>,
    producer: Arc<AnyProducer>,
    control: Option<Arc<AnyConsumer>>,
) -> anyhow::Result<()> {
    info!(
        concurrency = args.concurrency,
        "Starting Scrapix content worker (in-process bus)"
    );

    let worker = Arc::new(ContentWorker::with_bus(&args, consumer, producer, control).await?);

    let result = worker.run().await;

    let metrics = worker.metrics.snapshot();
    info!(
        processed = metrics.pages_processed,
        succeeded = metrics.pages_succeeded,
        failed = metrics.pages_failed,
        "Final content worker metrics (in-process)"
    );

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrapix_core::{FeatureToggle, JobSpec};
    use scrapix_queue::{ChannelBus, UrlMessage};
    use serde::de::DeserializeOwned;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PAGE: &str = "<html><head><title>Guide</title></head><body><main><h1>Guide</h1>\
        <p>This page explains in detail how the crawler, the frontier and the content worker \
        cooperate to index documents into Meilisearch with at-least-once delivery.</p>\
        <p>Every raw page ends in exactly one outcome event, and offsets commit only after \
        Meilisearch accepted the batch containing the page's documents.</p></main></body></html>";

    fn task_accepted() -> ResponseTemplate {
        ResponseTemplate::new(202).set_body_json(serde_json::json!({
            "taskUid": 1, "indexUid": "idx", "status": "enqueued",
            "type": "documentAdditionOrUpdate", "enqueuedAt": "2026-01-01T00:00:00Z"}))
    }

    /// Meilisearch mock: index lookups 404 (fresh index), index creation
    /// and settings accepted, document additions answered by `docs`.
    async fn meilisearch(docs: ResponseTemplate) -> MockServer {
        let ms = MockServer::start().await;
        mount_meilisearch(&ms, docs).await;
        ms
    }

    /// Mount the [`meilisearch`] mocks on an existing server (mocks mounted
    /// earlier take precedence).
    async fn mount_meilisearch(ms: &MockServer, docs: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path_regex(r"^/indexes/[^/]+$"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "message": "not found", "code": "index_not_found",
                "type": "invalid_request", "link": "https://docs.meilisearch.com"})))
            .mount(ms)
            .await;
        Mock::given(method("POST"))
            .and(path("/indexes"))
            .respond_with(task_accepted())
            .mount(ms)
            .await;
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/indexes/[^/]+/settings$"))
            .respond_with(task_accepted())
            .mount(ms)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/indexes/[^/]+/documents$"))
            .respond_with(docs)
            .mount(ms)
            .await;
    }

    /// Worker against `ms` (indexing enabled, no startup check), with
    /// extra CLI flags.
    fn build_worker(bus: &ChannelBus, ms: &MockServer, extra: &[&str]) -> ContentWorker {
        let uri = ms.uri();
        let mut argv = vec!["scrapix-worker-content", "--meilisearch-url", uri.as_str()];
        argv.extend_from_slice(extra);
        ContentWorker::build(
            &Args::parse_from(argv),
            "test".into(),
            Arc::new(AnyConsumer::from(bus.consumer())),
            Arc::new(AnyProducer::from(bus.producer())),
            true,
        )
    }

    fn started(w: ContentWorker) -> Arc<ContentWorker> {
        let w = Arc::new(w);
        w.spawn_indexed_publisher();
        w
    }

    fn worker(bus: &ChannelBus, ms: &MockServer) -> Arc<ContentWorker> {
        started(build_worker(bus, ms, &[]))
    }

    fn events_reader(bus: &ChannelBus) -> AnyConsumer {
        let c = AnyConsumer::from(bus.consumer());
        c.subscribe(&[topic_names::EVENTS]).unwrap();
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

    fn page(url: &str, status: u16, job: Option<JobSpec>) -> RawPageMessage {
        let parent = UrlMessage::new(scrapix_core::CrawlUrl::seed(url), "job-1", "idx")
            .account("acct")
            .with_job(job);
        let raw = RawPage {
            url: url.to_string(),
            final_url: url.to_string(),
            status,
            headers: HashMap::new(),
            html: PAGE.to_string(),
            content_type: Some("text/html".to_string()),
            js_rendered: false,
            fetched_at: chrono::Utc::now(),
            fetch_duration_ms: 1,
        };
        RawPageMessage::from_url_message(&parent, raw, None, None)
    }

    async fn wait_for(flag: &AtomicBool) -> bool {
        ContentWorker::wait_until(Duration::from_secs(5), || flag.load(Ordering::SeqCst)).await
    }

    #[tokio::test]
    async fn indexed_event_and_ack_follow_meilisearch_acceptance() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let w = worker(&bus, &ms);
        let msg = page("https://a.test/guide", 200, None);
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;

        // Buffered, not yet sent: no ack, no DocumentIndexed.
        assert!(!acked.load(Ordering::SeqCst));
        assert!(drain::<CrawlEvent>(&events).await.is_empty());

        w.flush_all_storages().await;
        assert!(wait_for(&acked).await, "acked after Meilisearch accepted");
        let events: Vec<CrawlEvent> = drain(&events).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                CrawlEvent::DocumentIndexed { url_message_id, ai_enriched: false, account_id: Some(_), .. }
                    if *url_message_id == msg.url_message_id
            )),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn rejected_batch_is_not_acked_and_not_reported() {
        let ms = meilisearch(ResponseTemplate::new(500)).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let w = worker(&bus, &ms);
        let (ack, acked) = tracked_ack();

        w.handle_message(page("https://a.test/guide", 200, None), ack)
            .await;
        w.flush_all_storages().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(!acked.load(Ordering::SeqCst));
        let events: Vec<CrawlEvent> = drain(&events).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CrawlEvent::DocumentIndexed { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn non_2xx_page_is_skipped_and_acked() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let w = worker(&bus, &ms);
        let msg = page("https://a.test/missing", 404, None);
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;

        assert!(acked.load(Ordering::SeqCst));
        let events: Vec<CrawlEvent> = drain(&events).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                CrawlEvent::DocumentSkipped { reason, url_message_id, .. }
                    if reason == "non-2xx status" && *url_message_id == msg.url_message_id
            )),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn index_only_mismatch_is_skipped() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let w = worker(&bus, &ms);
        let job = JobSpec {
            index_only: vec!["https://a.test/docs/*".into()],
            ..Default::default()
        };
        let (ack, acked) = tracked_ack();

        w.handle_message(page("https://a.test/blog/1", 200, Some(job)), ack)
            .await;

        assert!(acked.load(Ordering::SeqCst));
        let events: Vec<CrawlEvent> = drain(&events).await;
        assert!(
            events.iter().any(
                |e| matches!(e, CrawlEvent::DocumentSkipped { reason, .. } if reason == "index_only")
            ),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn job_primary_key_and_batch_size_are_honored() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let w = worker(&bus, &ms);
        let job = JobSpec {
            primary_key: Some("id".into()),
            batch_size: Some(1),
            ..Default::default()
        };
        let (ack, acked) = tracked_ack();

        // batch_size 1: the add itself sends the batch, no flush needed.
        w.handle_message(page("https://a.test/guide", 200, Some(job)), ack)
            .await;
        assert!(wait_for(&acked).await);

        let requests = ms.received_requests().await.unwrap();
        let add = requests
            .iter()
            .find(|r| r.url.path() == "/indexes/idx/documents")
            .expect("documents sent");
        assert!(add.url.query().unwrap_or("").contains("primaryKey=id"));
        let create = requests
            .iter()
            .find(|r| r.method.as_str() == "POST" && r.url.path() == "/indexes")
            .expect("index created");
        let body: serde_json::Value = serde_json::from_slice(&create.body).unwrap();
        assert_eq!(body["primaryKey"], "id");
    }

    #[tokio::test]
    async fn ai_without_provider_indexes_and_warns_once_per_job() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let mut w = build_worker(&bus, &ms, &[]);
        // No AI provider, whatever this environment's AI_* variables say.
        w.ai_service = None;
        let w = started(w);
        let features = FeaturesConfig {
            ai_summary: Some(FeatureToggle {
                enabled: true,
                include_pages: vec![],
                exclude_pages: vec![],
            }),
            ..Default::default()
        };
        let mut acks = Vec::new();
        for p in ["https://a.test/1", "https://a.test/2"] {
            let mut msg = page(p, 200, None);
            msg.features = Some(features.clone());
            let (ack, acked) = tracked_ack();
            w.handle_message(msg, ack).await;
            acks.push(acked);
        }
        w.flush_all_storages().await;
        for acked in &acks {
            assert!(wait_for(acked).await);
        }

        let events: Vec<CrawlEvent> = drain(&events).await;
        let warnings = events
            .iter()
            .filter(|e| matches!(e, CrawlEvent::JobWarning { message, .. } if message == WARN_AI_NO_PROVIDER))
            .count();
        assert_eq!(warnings, 1, "{events:?}");
        let indexed = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    CrawlEvent::DocumentIndexed {
                        ai_enriched: false,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(indexed, 2, "{events:?}");
    }

    #[tokio::test]
    async fn near_duplicates_are_scoped_per_index_and_ignore_recrawls() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let w = build_worker(&bus, &ms, &["--enable-dedup"]);

        let a = page("https://a.test/guide", 200, None);
        let recrawl = page("https://a.test/guide", 200, None);
        let mut other_index = page("https://b.test/guide", 200, None);
        other_index.index_uid = "other".into();
        let copy = page("https://a.test/copy", 200, None);

        for m in [&a, &recrawl, &other_index] {
            let (ack, _) = tracked_ack();
            w.handle_message(m.clone(), ack).await;
        }
        let (ack, acked) = tracked_ack();
        w.handle_message(copy.clone(), ack).await;
        assert!(acked.load(Ordering::SeqCst), "duplicate skip is acked");

        let events: Vec<CrawlEvent> = drain(&events).await;
        let skipped: Vec<&String> = events
            .iter()
            .filter_map(|e| match e {
                CrawlEvent::DocumentSkipped { url_message_id, .. } => Some(url_message_id),
                _ => None,
            })
            .collect();
        assert_eq!(skipped, vec![&copy.url_message_id], "{events:?}");
    }

    #[test]
    fn ai_usage_events_carry_job_attribution() {
        let usage = AiUsageEvent {
            provider: "openai".into(),
            model: "m".into(),
            prompt_tokens: 3,
            completion_tokens: 4,
            total_tokens: 7,
            duration_ms: 5,
            timestamp: chrono::Utc::now(),
            context: Some(AiUsageContext {
                job_id: "job".into(),
                account_id: Some("acct".into()),
                feature: "ai_summary".into(),
                url: "https://a.test/".into(),
            }),
        };
        match ai_usage_event(usage.clone()) {
            Some(CrawlEvent::AiUsage {
                job_id,
                account_id,
                prompt_tokens: 3,
                completion_tokens: 4,
                feature,
                ..
            }) => {
                assert_eq!(job_id, "job");
                assert_eq!(account_id.as_deref(), Some("acct"));
                assert_eq!(feature, "ai_summary");
            }
            other => panic!("unexpected {other:?}"),
        }
        let unattributed = AiUsageEvent {
            context: None,
            ..usage
        };
        assert!(ai_usage_event(unattributed).is_none());
    }

    fn failed_for<'a>(events: &'a [CrawlEvent], id: &str) -> Option<&'a String> {
        events.iter().find_map(|e| match e {
            CrawlEvent::DocumentFailed {
                url_message_id,
                error,
                ..
            } if url_message_id == id => Some(error),
            _ => None,
        })
    }

    fn indexed_count(events: &[CrawlEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, CrawlEvent::DocumentIndexed { .. }))
            .count()
    }

    #[tokio::test]
    async fn meilisearch_unreachable_at_startup_keeps_indexing_enabled() {
        let ms = MockServer::start().await;
        // Startup index initialization fails...
        Mock::given(method("POST"))
            .and(path("/indexes"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&ms)
            .await;
        mount_meilisearch(&ms, task_accepted()).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let args = Args::parse_from(["scrapix-worker-content", "--meilisearch-url", &ms.uri()]);
        let w = ContentWorker::with_bus(
            &args,
            Arc::new(AnyConsumer::from(bus.consumer())),
            Arc::new(AnyProducer::from(bus.producer())),
            None,
        )
        .await
        .unwrap();
        assert!(w.indexing_enabled);
        let w = started(w);
        let msg = page("https://a.test/guide", 200, None);
        let (ack, acked) = tracked_ack();

        // ...but pages are still indexed, and acked only once accepted.
        w.handle_message(msg.clone(), ack).await;
        assert!(!acked.load(Ordering::SeqCst));
        w.flush_all_storages().await;
        assert!(wait_for(&acked).await);
        let events: Vec<CrawlEvent> = drain(&events).await;
        assert_eq!(indexed_count(&events), 1, "{events:?}");
    }

    #[tokio::test]
    async fn skip_meilisearch_reports_indexing_disabled() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let args = Args::parse_from([
            "scrapix-worker-content",
            "--meilisearch-url",
            &ms.uri(),
            "--skip-meilisearch",
        ]);
        let w = ContentWorker::with_bus(
            &args,
            Arc::new(AnyConsumer::from(bus.consumer())),
            Arc::new(AnyProducer::from(bus.producer())),
            None,
        )
        .await
        .unwrap();
        let w = started(w);
        let msg = page("https://a.test/guide", 200, None);
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;

        assert!(acked.load(Ordering::SeqCst));
        let events: Vec<CrawlEvent> = drain(&events).await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                CrawlEvent::DocumentSkipped { reason, url_message_id, .. }
                    if reason == "indexing disabled" && *url_message_id == msg.url_message_id
            )),
            "{events:?}"
        );
        assert_eq!(indexed_count(&events), 0);
        assert!(ms.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn jobs_with_different_settings_on_one_index_each_get_them() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let w = worker(&bus, &ms);
        let job = |attr: &str| JobSpec {
            index_settings: Some(scrapix_core::MeilisearchSettings {
                searchable_attributes: Some(vec![attr.to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        for (url, spec) in [
            ("https://a.test/1", job("a")),
            ("https://a.test/2", job("a")),
            ("https://a.test/3", job("b")),
        ] {
            let (ack, _) = tracked_ack();
            w.handle_message(page(url, 200, Some(spec)), ack).await;
        }

        let patches: Vec<serde_json::Value> = ms
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "PATCH")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(patches.len(), 2, "once per distinct job configuration");
        assert_eq!(patches[0]["searchableAttributes"], serde_json::json!(["a"]));
        assert_eq!(patches[1]["searchableAttributes"], serde_json::json!(["b"]));
    }

    #[tokio::test]
    async fn permanently_refused_batch_fails_the_page_and_acks() {
        let ms = meilisearch(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "message": "The provided API key is invalid.", "code": "invalid_api_key",
            "type": "auth", "link": "https://docs.meilisearch.com/errors#invalid_api_key"})))
        .await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let w = worker(&bus, &ms);
        let msg = page("https://a.test/guide", 200, None);
        let (ack, acked) = tracked_ack();

        w.handle_message(msg.clone(), ack).await;
        w.flush_all_storages().await;

        assert!(wait_for(&acked).await, "refused page is acked, not stalled");
        let events: Vec<CrawlEvent> = drain(&events).await;
        let error = failed_for(&events, &msg.url_message_id).expect("DocumentFailed");
        assert!(error.contains("refused"), "{error}");
        assert_eq!(indexed_count(&events), 0, "{events:?}");
    }

    #[tokio::test]
    async fn block_split_page_acks_once_after_all_blocks_are_accepted() {
        let ms = MockServer::start().await;
        let docs = |status: u16| {
            Mock::given(method("POST"))
                .and(path_regex(r"^/indexes/[^/]+/documents$"))
                .respond_with(if status == 202 {
                    task_accepted()
                } else {
                    ResponseTemplate::new(status)
                })
                .up_to_n_times(1)
        };
        // 1st batch accepted, 2nd rejected (retryable), then accepted.
        docs(202).mount(&ms).await;
        docs(500).mount(&ms).await;
        mount_meilisearch(&ms, task_accepted()).await;

        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let w = worker(&bus, &ms);
        let section = |h: &str| {
            format!(
                "<h2 id=\"{h}\">{h}</h2><p>Section {h} has a long enough paragraph to be kept \
                 as its own block by the splitter, well over fifty characters.</p>"
            )
        };
        let html = format!(
            "<html><head><title>T</title></head><body><main>{}{}{}</main></body></html>",
            section("one"),
            section("two"),
            section("three")
        );
        let mut msg = page(
            "https://a.test/blocks",
            200,
            Some(JobSpec {
                batch_size: Some(2),
                ..Default::default()
            }),
        );
        msg.html = html;
        msg.features = Some(FeaturesConfig {
            block_split: Some(FeatureToggle {
                enabled: true,
                include_pages: vec![],
                exclude_pages: vec![],
            }),
            ..Default::default()
        });
        let (ack, acked) = tracked_ack();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = count.clone();
        let ack = Ack::from_fn(move || {
            c.fetch_add(1, Ordering::SeqCst);
            ack.ack();
        });

        // Blocks 1-2 go out as the first (accepted) batch; block 3 stays
        // buffered: the page is not acked yet.
        w.handle_message(msg.clone(), ack).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!acked.load(Ordering::SeqCst));

        // Block 3's first send is rejected: still not acked.
        w.flush_all_storages().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!acked.load(Ordering::SeqCst));

        // Accepted: acked exactly once, one DocumentIndexed for the page.
        w.flush_all_storages().await;
        assert!(wait_for(&acked).await);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let sent: usize = ms
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/documents"))
            .map(|r| {
                serde_json::from_slice::<Vec<serde_json::Value>>(&r.body)
                    .unwrap()
                    .len()
            })
            .sum();
        assert_eq!(sent, 4, "2 + 1 rejected + 1 accepted");
        let events: Vec<CrawlEvent> = drain(&events).await;
        assert_eq!(indexed_count(&events), 1, "{events:?}");
    }

    /// R5: a page of a cancelled (or finished) job is acked and never
    /// indexed, with no outcome event.
    #[tokio::test]
    async fn cancelled_job_page_is_acked_and_not_indexed() {
        let ms = meilisearch(task_accepted()).await;
        let bus = ChannelBus::new();
        let events = events_reader(&bus);
        let w = worker(&bus, &ms);
        w.cancelled.insert("job-1");
        let (ack, acked) = tracked_ack();

        w.handle_message(page("https://a.test/guide", 200, None), ack)
            .await;
        w.flush_all_storages().await;

        assert!(acked.load(Ordering::SeqCst), "acked without work");
        let requests = ms.received_requests().await.unwrap();
        assert!(
            !requests
                .iter()
                .any(|r| r.url.path().ends_with("/documents")),
            "nothing sent to Meilisearch: {:?}",
            requests
                .iter()
                .map(|r| r.url.path().to_string())
                .collect::<Vec<_>>()
        );
        assert!(drain::<CrawlEvent>(&events).await.is_empty());
    }
}
