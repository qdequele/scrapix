//! Topic definitions for the message queue

use serde::{Deserialize, Serialize};

use scrapix_core::{CrawlUrl, Document, FeaturesConfig, JobSpec, RawPage, UrlPatterns};

/// Predefined topic names
pub mod names {
    /// URLs to be crawled
    pub const URL_FRONTIER: &str = "scrapix.urls.frontier";
    /// URLs currently being processed
    pub const URL_PROCESSING: &str = "scrapix.urls.processing";
    /// Raw crawled pages awaiting content extraction
    pub const PAGES_RAW: &str = "scrapix.pages.raw";
    /// Processed documents ready for indexing
    pub const DOCUMENTS: &str = "scrapix.documents";
    /// Failed URLs (dead letter queue)
    pub const DLQ_URLS: &str = "scrapix.dlq.urls";
    /// Crawl events for monitoring
    pub const EVENTS: &str = "scrapix.events";
    /// Job status updates
    pub const JOB_STATUS: &str = "scrapix.jobs.status";
    /// Link graph updates (discovered links)
    pub const LINKS: &str = "scrapix.links";
    /// Crawl history updates (for incremental crawling)
    pub const CRAWL_HISTORY: &str = "scrapix.crawl.history";
    /// Crawler → frontier feedback after every fetch attempt (politeness
    /// slot release, robots crawl-delay, Retry-After)
    pub const FETCH_FEEDBACK: &str = "scrapix.fetch.feedback";
}

/// Message types for the URL frontier queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlMessage {
    /// The URL to crawl
    pub url: CrawlUrl,
    /// Job ID this URL belongs to
    pub job_id: String,
    /// Index UID for the destination
    pub index_uid: String,
    /// Source identifier for multi-tenant indexing
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Account ID for billing attribution
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Message ID for tracking
    pub message_id: String,
    /// Timestamp when the message was created
    pub created_at: i64,
    /// URL patterns for filtering discovered URLs (optional, inherited from job config)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_patterns: Option<UrlPatterns>,
    /// Per-job Meilisearch URL (overrides global env var)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meilisearch_url: Option<String>,
    /// Per-job Meilisearch API key (overrides global env var)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meilisearch_api_key: Option<String>,
    /// Per-job feature configuration (overrides worker defaults)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<FeaturesConfig>,
    /// Per-job max crawl depth (None = unlimited)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    /// Per-job max pages to crawl (None = unlimited)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<u64>,
    /// Whether this job uses incremental crawling (conditional HTTP headers + Redis history).
    /// Defaults to true. Set to false for Replace index strategy (full re-crawl).
    #[serde(default = "default_true")]
    pub incremental: bool,
    /// Job-scoped crawl settings (rate limits, proxy, meilisearch settings, etc.)
    /// that workers need but that are not per-URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobSpec>,
}

fn default_true() -> bool {
    true
}

impl UrlMessage {
    pub fn new(url: CrawlUrl, job_id: impl Into<String>, index_uid: impl Into<String>) -> Self {
        Self {
            url,
            job_id: job_id.into(),
            index_uid: index_uid.into(),
            source: None,
            account_id: None,
            message_id: uuid::Uuid::new_v4().to_string(),
            created_at: chrono::Utc::now().timestamp_millis(),
            url_patterns: None,
            meilisearch_url: None,
            meilisearch_api_key: None,
            features: None,
            max_depth: None,
            max_pages: None,
            incremental: true,
            job: None,
        }
    }

    /// Create a new URL message with account ID
    pub fn with_account(
        url: CrawlUrl,
        job_id: impl Into<String>,
        index_uid: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Self {
        Self {
            account_id: Some(account_id.into()),
            ..Self::new(url, job_id, index_uid)
        }
    }

    /// Create a new URL message with URL patterns
    pub fn with_patterns(
        url: CrawlUrl,
        job_id: impl Into<String>,
        index_uid: impl Into<String>,
        patterns: UrlPatterns,
    ) -> Self {
        Self {
            url_patterns: Some(patterns),
            ..Self::new(url, job_id, index_uid)
        }
    }

    /// Set account ID (builder pattern)
    pub fn account(mut self, account_id: impl Into<String>) -> Self {
        self.account_id = Some(account_id.into());
        self
    }

    /// Set source identifier for multi-tenant indexing (builder pattern)
    pub fn with_source(mut self, source: Option<String>) -> Self {
        self.source = source;
        self
    }

    /// Set per-job Meilisearch URL and API key (builder pattern)
    pub fn with_meilisearch(mut self, url: Option<String>, api_key: Option<String>) -> Self {
        self.meilisearch_url = url;
        self.meilisearch_api_key = api_key;
        self
    }

    /// Set per-job feature configuration (builder pattern)
    pub fn with_features(mut self, features: Option<FeaturesConfig>) -> Self {
        self.features = features;
        self
    }

    /// Set per-job crawl limits (builder pattern)
    pub fn with_limits(mut self, max_depth: Option<u32>, max_pages: Option<u64>) -> Self {
        self.max_depth = max_depth;
        self.max_pages = max_pages;
        self
    }

    /// Set whether this job uses incremental crawling (builder pattern).
    /// When false, the crawler always does a full fetch (no conditional headers, no Redis history).
    pub fn with_incremental(mut self, incremental: bool) -> Self {
        self.incremental = incremental;
        self
    }

    /// Set the job-scoped crawl settings (builder pattern)
    pub fn with_job(mut self, job: Option<JobSpec>) -> Self {
        self.job = job;
        self
    }

    /// Derive a new `UrlMessage` for a discovered/child URL, copying every
    /// job-scoped field from `self` except `url`, `message_id` (fresh uuid)
    /// and `created_at` (now).
    pub fn child(&self, url: CrawlUrl) -> Self {
        Self {
            url,
            message_id: uuid::Uuid::new_v4().to_string(),
            created_at: chrono::Utc::now().timestamp_millis(),
            job_id: self.job_id.clone(),
            index_uid: self.index_uid.clone(),
            source: self.source.clone(),
            account_id: self.account_id.clone(),
            url_patterns: self.url_patterns.clone(),
            meilisearch_url: self.meilisearch_url.clone(),
            meilisearch_api_key: self.meilisearch_api_key.clone(),
            features: self.features.clone(),
            max_depth: self.max_depth,
            max_pages: self.max_pages,
            incremental: self.incremental,
            job: self.job.clone(),
        }
    }

    /// Get the partition key (domain for locality)
    pub fn partition_key(&self) -> String {
        // Extract domain from URL for partitioning
        url::Url::parse(&self.url.url)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_else(|| self.job_id.clone())
    }
}

/// Message for raw crawled pages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawPageMessage {
    /// Source URL
    pub url: String,
    /// Final URL after redirects
    pub final_url: String,
    /// HTTP status code
    pub status: u16,
    /// Raw HTML content
    pub html: String,
    /// Content type
    pub content_type: Option<String>,
    /// Content length in bytes (for billing)
    #[serde(default)]
    pub content_length: u64,
    /// Whether JS was rendered
    pub js_rendered: bool,
    /// Fetch timestamp (millis)
    pub fetched_at: i64,
    /// Fetch duration (millis)
    pub fetch_duration_ms: u64,
    /// Job ID
    pub job_id: String,
    /// Index UID
    pub index_uid: String,
    /// Source identifier for multi-tenant indexing
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Account ID for billing attribution
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Message ID
    pub message_id: String,
    /// ETag from response (for incremental crawling)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Last-Modified from response (for incremental crawling)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    /// Per-job Meilisearch URL (overrides global env var)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meilisearch_url: Option<String>,
    /// Per-job Meilisearch API key (overrides global env var)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meilisearch_api_key: Option<String>,
    /// Per-job feature configuration (overrides worker defaults)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<FeaturesConfig>,
    /// Job-scoped crawl settings (rate limits, proxy, meilisearch settings, etc.)
    /// that workers need but that are not per-URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobSpec>,
    /// The `message_id` of the `UrlMessage` this page was fetched for.
    /// Used by accounting to tie billing events back to the originating
    /// frontier message.
    #[serde(default)]
    pub url_message_id: String,
}

impl RawPageMessage {
    /// Build a `RawPageMessage` from the `UrlMessage` it was fetched for and
    /// the resulting `RawPage`, copying every job-scoped field from `msg`.
    pub fn from_url_message(
        msg: &UrlMessage,
        page: RawPage,
        etag: Option<String>,
        last_modified: Option<String>,
    ) -> Self {
        let content_length = page.html.len() as u64;
        Self {
            url: page.url,
            final_url: page.final_url,
            status: page.status,
            html: page.html,
            content_type: page.content_type,
            content_length,
            js_rendered: page.js_rendered,
            fetched_at: page.fetched_at.timestamp_millis(),
            fetch_duration_ms: page.fetch_duration_ms,
            job_id: msg.job_id.clone(),
            index_uid: msg.index_uid.clone(),
            source: msg.source.clone(),
            account_id: msg.account_id.clone(),
            message_id: uuid::Uuid::new_v4().to_string(),
            etag,
            last_modified,
            meilisearch_url: msg.meilisearch_url.clone(),
            meilisearch_api_key: msg.meilisearch_api_key.clone(),
            features: msg.features.clone(),
            job: msg.job.clone(),
            url_message_id: msg.message_id.clone(),
        }
    }
}

/// Message for processed documents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentMessage {
    /// The processed document
    pub document: Document,
    /// Job ID
    pub job_id: String,
    /// Index UID
    pub index_uid: String,
    /// Message ID
    pub message_id: String,
}

impl DocumentMessage {
    pub fn new(
        document: Document,
        job_id: impl Into<String>,
        index_uid: impl Into<String>,
    ) -> Self {
        Self {
            document,
            job_id: job_id.into(),
            index_uid: index_uid.into(),
            message_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// Crawl event types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CrawlEvent {
    /// Job started
    JobStarted {
        job_id: String,
        index_uid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        start_urls: Vec<String>,
        timestamp: i64,
    },
    /// Job completed
    JobCompleted {
        job_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        pages_crawled: u64,
        documents_indexed: u64,
        errors: u64,
        /// Total bytes downloaded during the job
        #[serde(default)]
        bytes_downloaded: u64,
        duration_secs: u64,
        timestamp: i64,
    },
    /// Job failed
    JobFailed {
        job_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        error: String,
        timestamp: i64,
    },
    /// Page crawled successfully
    PageCrawled {
        job_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        url: String,
        status: u16,
        /// Content length in bytes (for billing)
        #[serde(default)]
        content_length: u64,
        duration_ms: u64,
        timestamp: i64,
        /// Number of discovered links published back to the frontier
        #[serde(default)]
        links_published: u64,
        /// `message_id` of the `UrlMessage` this page was fetched for
        #[serde(default)]
        url_message_id: String,
        /// Whether the page was rendered by a browser (browser billing)
        #[serde(default)]
        js_rendered: bool,
        /// Whether this message is the one that spawned a first-time
        /// sitemap discovery for (job, domain) (R-18). When true, a
        /// `SitemapPublished` with this same `url_message_id` is
        /// guaranteed to follow (with `count: 0` on the disabled/empty/
        /// error paths), so job-completion accounting knows to wait for
        /// it before balancing.
        #[serde(default)]
        sitemap_pending: bool,
    },
    /// Page crawl failed (terminal for this URL)
    PageFailed {
        job_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        url: String,
        error: String,
        retry_count: u32,
        timestamp: i64,
        /// Final HTTP status, when the failure came from a response
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        /// `message_id` of the `UrlMessage` that failed
        #[serde(default)]
        url_message_id: String,
    },
    /// A URL failed transiently and was re-queued with `retry_count + 1`
    PageRetried {
        job_id: String,
        url: String,
        /// `message_id` of the `UrlMessage` that was retried (the re-queued
        /// message gets a fresh id)
        #[serde(default)]
        url_message_id: String,
        /// Retry count of the re-queued message
        retry_count: u32,
        error: String,
        timestamp: i64,
    },
    /// Document indexed
    DocumentIndexed {
        job_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        url: String,
        document_id: String,
        timestamp: i64,
        /// `message_id` of the `UrlMessage` this page was fetched for
        #[serde(default)]
        url_message_id: String,
        /// Whether AI enrichment actually ran on this page (AI billing)
        #[serde(default)]
        ai_enriched: bool,
    },
    /// The content worker processed a page but indexed nothing for it
    /// (non-2xx page from an old crawler, `index_only` mismatch, no content,
    /// near-duplicate, non-HTML content type, ...). Terminal for the page.
    DocumentSkipped {
        #[serde(default)]
        job_id: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        url_message_id: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        timestamp: i64,
    },
    /// The content worker could not turn a page into a document (parse
    /// error, ...). Terminal for the page.
    DocumentFailed {
        #[serde(default)]
        job_id: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        url_message_id: String,
        #[serde(default)]
        error: String,
        #[serde(default)]
        timestamp: i64,
    },
    /// One LLM call made by a content worker while enriching a page.
    AiUsage {
        #[serde(default)]
        job_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        model: String,
        #[serde(default)]
        prompt_tokens: u32,
        #[serde(default)]
        completion_tokens: u32,
        #[serde(default)]
        duration_ms: u64,
        /// AI feature that made the call (`ai_summary`, `ai_extraction`)
        #[serde(default)]
        feature: String,
        /// Page the call was made for
        #[serde(default)]
        url: String,
        #[serde(default)]
        timestamp: i64,
    },
    /// Job-level warning raised by a worker (e.g. a requested feature that
    /// this worker cannot honor). At most once per (job, message) per worker.
    JobWarning {
        #[serde(default)]
        job_id: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        timestamp: i64,
    },
    /// Periodic per-job frontier snapshot (cumulative store counters plus
    /// the current queue depth), published by the frontier instance that
    /// holds the job's dispatch lease whenever the counters change.
    FrontierProgress {
        #[serde(default)]
        job_id: String,
        #[serde(default)]
        instance_id: String,
        #[serde(default)]
        received: u64,
        #[serde(default)]
        admitted: u64,
        #[serde(default)]
        dispatched: u64,
        #[serde(default)]
        rejected: u64,
        #[serde(default)]
        dropped: u64,
        #[serde(default)]
        queued: u64,
        #[serde(default)]
        timestamp: i64,
    },
    /// URLs discovered
    UrlsDiscovered {
        job_id: String,
        source_url: String,
        count: usize,
        timestamp: i64,
    },
    /// Rate limited
    RateLimited {
        job_id: String,
        domain: String,
        wait_ms: u64,
        timestamp: i64,
    },
    /// Page skipped (duplicate, filtered, etc.)
    PageSkipped {
        job_id: String,
        url: String,
        reason: String,
        timestamp: i64,
        /// `message_id` of the `UrlMessage` that was skipped (empty when the
        /// skip is not tied to a frontier message)
        #[serde(default)]
        url_message_id: String,
    },
    /// Sitemap URLs published to the frontier for a job's domain.
    ///
    /// Distinct from `UrlsDiscovered` (which is also published alongside
    /// this event) so job completion accounting can attribute
    /// sitemap-seeded URLs to the `UrlMessage` that triggered discovery.
    /// All fields default so old and new workers interoperate during a
    /// rolling deploy.
    SitemapPublished {
        #[serde(default)]
        job_id: String,
        #[serde(default)]
        count: usize,
        /// `message_id` of the `UrlMessage` whose successful fetch
        /// triggered this sitemap discovery.
        #[serde(default)]
        url_message_id: String,
        #[serde(default)]
        timestamp: i64,
    },
}

impl CrawlEvent {
    pub fn job_started(
        job_id: impl Into<String>,
        index_uid: impl Into<String>,
        start_urls: Vec<String>,
    ) -> Self {
        Self::JobStarted {
            job_id: job_id.into(),
            index_uid: index_uid.into(),
            account_id: None,
            start_urls,
            timestamp: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Create job started event with account ID
    pub fn job_started_with_account(
        job_id: impl Into<String>,
        index_uid: impl Into<String>,
        account_id: impl Into<String>,
        start_urls: Vec<String>,
    ) -> Self {
        Self::JobStarted {
            job_id: job_id.into(),
            index_uid: index_uid.into(),
            account_id: Some(account_id.into()),
            start_urls,
            timestamp: chrono::Utc::now().timestamp_millis(),
        }
    }

    pub fn page_crawled(
        job_id: impl Into<String>,
        url: impl Into<String>,
        status: u16,
        duration_ms: u64,
    ) -> Self {
        Self::PageCrawled {
            job_id: job_id.into(),
            account_id: None,
            url: url.into(),
            status,
            content_length: 0,
            duration_ms,
            timestamp: chrono::Utc::now().timestamp_millis(),
            links_published: 0,
            url_message_id: String::new(),
            js_rendered: false,
            sitemap_pending: false,
        }
    }

    /// Create page crawled event with content length for billing
    pub fn page_crawled_with_billing(
        job_id: impl Into<String>,
        account_id: Option<String>,
        url: impl Into<String>,
        status: u16,
        content_length: u64,
        duration_ms: u64,
    ) -> Self {
        Self::PageCrawled {
            job_id: job_id.into(),
            account_id,
            url: url.into(),
            status,
            content_length,
            duration_ms,
            timestamp: chrono::Utc::now().timestamp_millis(),
            links_published: 0,
            url_message_id: String::new(),
            js_rendered: false,
            sitemap_pending: false,
        }
    }

    pub fn page_failed(
        job_id: impl Into<String>,
        url: impl Into<String>,
        error: impl Into<String>,
        retry_count: u32,
    ) -> Self {
        Self::PageFailed {
            job_id: job_id.into(),
            account_id: None,
            url: url.into(),
            error: error.into(),
            retry_count,
            timestamp: chrono::Utc::now().timestamp_millis(),
            status: None,
            url_message_id: String::new(),
        }
    }
}

/// Dead letter queue message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlqMessage {
    /// Original message (JSON)
    pub original_message: String,
    /// Original topic
    pub original_topic: String,
    /// Error that caused the failure
    pub error: String,
    /// Number of retry attempts
    pub retry_count: u32,
    /// Timestamp of last failure
    pub failed_at: i64,
    /// Job ID if available
    pub job_id: Option<String>,
}

impl DlqMessage {
    pub fn new(
        original_message: impl Into<String>,
        original_topic: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            original_message: original_message.into(),
            original_topic: original_topic.into(),
            error: error.into(),
            retry_count: 1,
            failed_at: chrono::Utc::now().timestamp_millis(),
            job_id: None,
        }
    }

    pub fn with_job_id(mut self, job_id: impl Into<String>) -> Self {
        self.job_id = Some(job_id.into());
        self
    }

    pub fn increment_retry(mut self) -> Self {
        self.retry_count += 1;
        self.failed_at = chrono::Utc::now().timestamp_millis();
        self
    }
}

/// Message for link graph updates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinksMessage {
    /// Source URL where links were found
    pub source_url: String,
    /// Target URLs (outbound links)
    pub target_urls: Vec<String>,
    /// Job ID
    pub job_id: String,
    /// Timestamp
    pub timestamp: i64,
}

impl LinksMessage {
    pub fn new(
        source_url: impl Into<String>,
        target_urls: Vec<String>,
        job_id: impl Into<String>,
    ) -> Self {
        Self {
            source_url: source_url.into(),
            target_urls,
            job_id: job_id.into(),
            timestamp: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// Message for crawl history updates (for recrawl scheduling)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlHistoryMessage {
    /// URL that was crawled
    pub url: String,
    /// ETag from response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Last-Modified from response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    /// SHA-256 hash of content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// HTTP status code
    pub status: u16,
    /// Content length
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
    /// Whether content changed since last crawl
    pub content_changed: bool,
    /// Job ID
    pub job_id: String,
    /// Timestamp
    pub timestamp: i64,
}

impl CrawlHistoryMessage {
    pub fn new(url: impl Into<String>, status: u16, job_id: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            etag: None,
            last_modified: None,
            content_hash: None,
            status,
            content_length: None,
            content_changed: true, // Assume changed by default
            job_id: job_id.into(),
            timestamp: chrono::Utc::now().timestamp_millis(),
        }
    }

    pub fn with_etag(mut self, etag: impl Into<String>) -> Self {
        self.etag = Some(etag.into());
        self
    }

    pub fn with_last_modified(mut self, last_modified: impl Into<String>) -> Self {
        self.last_modified = Some(last_modified.into());
        self
    }

    pub fn with_content_hash(mut self, hash: impl Into<String>) -> Self {
        self.content_hash = Some(hash.into());
        self
    }

    pub fn with_content_length(mut self, length: u64) -> Self {
        self.content_length = Some(length);
        self
    }

    pub fn with_content_changed(mut self, changed: bool) -> Self {
        self.content_changed = changed;
        self
    }
}

/// Crawler → frontier: one per dispatched `UrlMessage` the crawler handled,
/// published to [`names::FETCH_FEEDBACK`] keyed by `domain`. It releases the
/// politeness slot the frontier took at dispatch and carries what the fetch
/// learned about the domain.
///
/// Every field is `#[serde(default)]` so old and new workers interoperate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FetchFeedback {
    /// Politeness key of the URL (its host, as `extract_domain` returns it)
    #[serde(default)]
    pub domain: String,
    /// Job the URL belongs to (per-job in-flight cap)
    #[serde(default)]
    pub job_id: String,
    /// `message_id` of the dispatched `UrlMessage` (identifies the slot)
    #[serde(default)]
    pub message_id: String,
    /// The URL fetched
    #[serde(default)]
    pub url: String,
    /// HTTP status of the response (`304` for not-modified); `None` when no
    /// response was received (transport error, or no request was made)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// A transport-level failure (timeout, connection, network) reached the
    /// domain without a response
    #[serde(default)]
    pub transport_error: bool,
    /// Server `Retry-After` (429/503), in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// robots.txt `Crawl-delay` of the URL's origin, in milliseconds (only
    /// for jobs that respect robots.txt, and only when already cached)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crawl_delay_ms: Option<u64>,
    /// The URL's robots.txt was fetched (the crawler's robots cache holds
    /// its origin) and the job respects robots.txt — so `crawl_delay_ms:
    /// None` means "robots.txt sets no Crawl-delay", not "unknown"
    #[serde(default)]
    pub robots_checked: bool,
    /// When the feedback was produced (ms since epoch)
    #[serde(default)]
    pub timestamp: i64,
}

impl FetchFeedback {
    /// Feedback for `url` of message `message_id`; fill in the rest with
    /// struct update syntax.
    pub fn new(
        domain: impl Into<String>,
        job_id: impl Into<String>,
        message_id: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            domain: domain.into(),
            job_id: job_id.into(),
            message_id: message_id.into(),
            url: url.into(),
            timestamp: chrono::Utc::now().timestamp_millis(),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_feedback_tolerates_missing_fields_and_skips_nones() {
        let fb: FetchFeedback = serde_json::from_str(r#"{"domain":"a.test"}"#).unwrap();
        assert_eq!(fb.domain, "a.test");
        assert_eq!(fb.status, None);
        assert!(!fb.transport_error);
        let json = serde_json::to_string(&FetchFeedback::new("a.test", "j", "m", "u")).unwrap();
        assert!(!json.contains("status"), "{json}");
        assert!(!json.contains("retry_after_ms"), "{json}");
        let back: FetchFeedback = serde_json::from_str(&json).unwrap();
        assert_eq!(back.message_id, "m");
    }

    #[test]
    fn test_links_message_creation() {
        let msg = LinksMessage::new(
            "https://example.com/page",
            vec![
                "https://example.com/link1".to_string(),
                "https://example.com/link2".to_string(),
            ],
            "job-123",
        );

        assert_eq!(msg.source_url, "https://example.com/page");
        assert_eq!(msg.target_urls.len(), 2);
        assert_eq!(msg.job_id, "job-123");
        assert!(msg.timestamp > 0);
    }

    #[test]
    fn test_links_message_serialization() {
        let msg = LinksMessage::new(
            "https://example.com/source",
            vec!["https://example.com/target".to_string()],
            "job-456",
        );

        let json = serde_json::to_string(&msg).expect("Failed to serialize");
        let deserialized: LinksMessage =
            serde_json::from_str(&json).expect("Failed to deserialize");

        assert_eq!(deserialized.source_url, msg.source_url);
        assert_eq!(deserialized.target_urls, msg.target_urls);
        assert_eq!(deserialized.job_id, msg.job_id);
    }

    #[test]
    fn test_crawl_history_message_creation() {
        let msg = CrawlHistoryMessage::new("https://example.com/page", 200, "job-789");

        assert_eq!(msg.url, "https://example.com/page");
        assert_eq!(msg.status, 200);
        assert_eq!(msg.job_id, "job-789");
        assert!(msg.content_changed); // Default is true
        assert!(msg.etag.is_none());
        assert!(msg.last_modified.is_none());
        assert!(msg.content_hash.is_none());
    }

    #[test]
    fn test_crawl_history_message_builder() {
        let msg = CrawlHistoryMessage::new("https://example.com/page", 200, "job-123")
            .with_etag("\"abc123\"")
            .with_last_modified("Wed, 21 Oct 2023 07:28:00 GMT")
            .with_content_hash("sha256:deadbeef")
            .with_content_length(12345)
            .with_content_changed(false);

        assert_eq!(msg.etag, Some("\"abc123\"".to_string()));
        assert_eq!(
            msg.last_modified,
            Some("Wed, 21 Oct 2023 07:28:00 GMT".to_string())
        );
        assert_eq!(msg.content_hash, Some("sha256:deadbeef".to_string()));
        assert_eq!(msg.content_length, Some(12345));
        assert!(!msg.content_changed);
    }

    #[test]
    fn test_crawl_history_message_serialization() {
        let msg = CrawlHistoryMessage::new("https://example.com/page", 200, "job-123")
            .with_etag("\"etag\"")
            .with_content_hash("hash123");

        let json = serde_json::to_string(&msg).expect("Failed to serialize");
        let deserialized: CrawlHistoryMessage =
            serde_json::from_str(&json).expect("Failed to deserialize");

        assert_eq!(deserialized.url, msg.url);
        assert_eq!(deserialized.status, msg.status);
        assert_eq!(deserialized.etag, msg.etag);
        assert_eq!(deserialized.content_hash, msg.content_hash);
    }

    #[test]
    fn test_url_message_partition_key() {
        let url = CrawlUrl::seed("https://example.com/path/to/page");
        let msg = UrlMessage::new(url, "job-1", "index-1");

        let key = msg.partition_key();
        assert_eq!(key, "example.com");
    }

    #[test]
    fn test_document_message_creation() {
        let doc = Document::new("https://example.com/doc", "example.com");
        let msg = DocumentMessage::new(doc.clone(), "job-1", "index-1");

        assert_eq!(msg.document.url, "https://example.com/doc");
        assert_eq!(msg.job_id, "job-1");
        assert_eq!(msg.index_uid, "index-1");
        assert!(!msg.message_id.is_empty());
    }

    #[test]
    fn test_dlq_message_creation() {
        let msg = DlqMessage::new(
            r#"{"url": "https://failed.com"}"#,
            "scrapix.urls.frontier",
            "Connection timeout",
        )
        .with_job_id("job-failed");

        assert!(msg.original_message.contains("failed.com"));
        assert_eq!(msg.original_topic, "scrapix.urls.frontier");
        assert_eq!(msg.error, "Connection timeout");
        assert_eq!(msg.job_id, Some("job-failed".to_string()));
        assert_eq!(msg.retry_count, 1);

        // Test increment_retry
        let msg = msg.increment_retry();
        assert_eq!(msg.retry_count, 2);
    }

    #[test]
    fn test_crawl_event_constructors() {
        let started =
            CrawlEvent::job_started("job-1", "index-1", vec!["https://example.com".to_string()]);
        match started {
            CrawlEvent::JobStarted {
                job_id,
                index_uid,
                start_urls,
                ..
            } => {
                assert_eq!(job_id, "job-1");
                assert_eq!(index_uid, "index-1");
                assert_eq!(start_urls.len(), 1);
            }
            _ => panic!("Expected JobStarted"),
        }

        let crawled = CrawlEvent::page_crawled("job-1", "https://example.com", 200, 150);
        match crawled {
            CrawlEvent::PageCrawled {
                job_id,
                url,
                status,
                duration_ms,
                ..
            } => {
                assert_eq!(job_id, "job-1");
                assert_eq!(url, "https://example.com");
                assert_eq!(status, 200);
                assert_eq!(duration_ms, 150);
            }
            _ => panic!("Expected PageCrawled"),
        }

        let failed = CrawlEvent::page_failed("job-1", "https://failed.com", "Timeout", 3);
        match failed {
            CrawlEvent::PageFailed {
                job_id,
                url,
                error,
                retry_count,
                ..
            } => {
                assert_eq!(job_id, "job-1");
                assert_eq!(url, "https://failed.com");
                assert_eq!(error, "Timeout");
                assert_eq!(retry_count, 3);
            }
            _ => panic!("Expected PageFailed"),
        }
    }

    #[test]
    fn page_events_without_new_fields_still_deserialize() {
        // Events written by a pre-upgrade worker (rolling deploy).
        let failed: CrawlEvent = serde_json::from_str(
            r#"{"type":"page_failed","job_id":"j","url":"u","error":"e","retry_count":0,"timestamp":1}"#,
        )
        .unwrap();
        match failed {
            CrawlEvent::PageFailed {
                status,
                url_message_id,
                ..
            } => {
                assert_eq!(status, None);
                assert!(url_message_id.is_empty());
            }
            other => panic!("{other:?}"),
        }
        let crawled: CrawlEvent = serde_json::from_str(
            r#"{"type":"page_crawled","job_id":"j","url":"u","status":200,"duration_ms":1,"timestamp":1}"#,
        )
        .unwrap();
        assert!(matches!(
            crawled,
            CrawlEvent::PageCrawled {
                links_published: 0,
                js_rendered: false,
                ..
            }
        ));
        let skipped: CrawlEvent = serde_json::from_str(
            r#"{"type":"page_skipped","job_id":"j","url":"u","reason":"r","timestamp":1}"#,
        )
        .unwrap();
        assert!(matches!(skipped, CrawlEvent::PageSkipped { .. }));
    }

    #[test]
    fn page_retried_round_trips() {
        let ev = CrawlEvent::PageRetried {
            job_id: "j".into(),
            url: "https://a.test/".into(),
            url_message_id: "m1".into(),
            retry_count: 2,
            error: "HTTP 503".into(),
            timestamp: 5,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"page_retried""#));
        match serde_json::from_str::<CrawlEvent>(&json).unwrap() {
            CrawlEvent::PageRetried {
                retry_count,
                url_message_id,
                ..
            } => {
                assert_eq!(retry_count, 2);
                assert_eq!(url_message_id, "m1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn child_copies_every_job_scoped_field() {
        let parent = UrlMessage::new(CrawlUrl::seed("https://a.test/"), "job", "idx")
            .with_source(Some("src".into()))
            .account("acct")
            .with_meilisearch(Some("http://ms".into()), Some("key".into()))
            .with_features(Some(FeaturesConfig::default()))
            .with_limits(Some(3), Some(50))
            .with_incremental(false)
            .with_job(Some(scrapix_core::JobSpec {
                user_agents: vec!["UA".into()],
                ..Default::default()
            }));
        let child = parent.child(CrawlUrl::new("https://a.test/x", 1));

        let strip = |m: &UrlMessage| {
            let mut v = serde_json::to_value(m).unwrap();
            let o = v.as_object_mut().unwrap();
            o.remove("url");
            o.remove("message_id");
            o.remove("created_at");
            v
        };
        assert_eq!(strip(&parent), strip(&child));
        assert_ne!(parent.message_id, child.message_id);
        assert_eq!(child.url.url, "https://a.test/x");
    }

    #[test]
    fn old_url_message_without_job_still_deserializes() {
        let json = r#"{"url":{"url":"https://a.test/","depth":0,"priority":0,"discovered_at":"2024-01-01T00:00:00Z","retry_count":0,"requires_js":false},
                       "job_id":"j","index_uid":"i","message_id":"m","created_at":0}"#;
        let m: UrlMessage = serde_json::from_str(json).unwrap();
        assert!(m.job.is_none());
        assert!(m.incremental);
    }

    #[test]
    fn raw_page_message_from_url_message_copies_job_scoped_fields() {
        let parent = UrlMessage::new(CrawlUrl::seed("https://a.test/"), "job", "idx")
            .with_source(Some("src".into()))
            .account("acct")
            .with_meilisearch(Some("http://ms".into()), Some("key".into()))
            .with_features(Some(FeaturesConfig::default()))
            .with_job(Some(scrapix_core::JobSpec {
                user_agents: vec!["UA".into()],
                ..Default::default()
            }));

        let page = scrapix_core::RawPage {
            url: "https://a.test/".to_string(),
            final_url: "https://a.test/".to_string(),
            status: 200,
            headers: std::collections::HashMap::new(),
            html: "<html></html>".to_string(),
            content_type: Some("text/html".to_string()),
            js_rendered: false,
            fetched_at: chrono::Utc::now(),
            fetch_duration_ms: 10,
        };

        let raw = RawPageMessage::from_url_message(
            &parent,
            page.clone(),
            Some("etag-1".to_string()),
            Some("Wed, 21 Oct 2023 07:28:00 GMT".to_string()),
        );

        assert_eq!(raw.job_id, parent.job_id);
        assert_eq!(raw.index_uid, parent.index_uid);
        assert_eq!(raw.source, parent.source);
        assert_eq!(raw.account_id, parent.account_id);
        assert_eq!(raw.meilisearch_url, parent.meilisearch_url);
        assert_eq!(raw.meilisearch_api_key, parent.meilisearch_api_key);
        assert_eq!(
            serde_json::to_value(&raw.features).unwrap(),
            serde_json::to_value(&parent.features).unwrap()
        );
        assert_eq!(raw.job, parent.job);
        assert_eq!(raw.url_message_id, parent.message_id);
        assert_eq!(raw.content_length, page.html.len() as u64);
        assert_eq!(raw.etag, Some("etag-1".to_string()));
        assert_ne!(raw.message_id, parent.message_id);
    }

    #[test]
    fn old_document_indexed_deserializes_with_defaults() {
        let json =
            r#"{"type":"document_indexed","job_id":"j","url":"u","document_id":"d","timestamp":1}"#;
        match serde_json::from_str::<CrawlEvent>(json).unwrap() {
            CrawlEvent::DocumentIndexed {
                url_message_id,
                ai_enriched,
                ..
            } => {
                assert!(url_message_id.is_empty());
                assert!(!ai_enriched);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn new_content_outcome_events_round_trip() {
        for json in [
            r#"{"type":"document_skipped","job_id":"j","url":"u","url_message_id":"m","reason":"index_only","timestamp":1}"#,
            r#"{"type":"document_failed","job_id":"j","url":"u","url_message_id":"m","error":"e","timestamp":1}"#,
            r#"{"type":"ai_usage","job_id":"j","model":"m","prompt_tokens":1,"completion_tokens":2,"feature":"ai_summary","timestamp":1}"#,
            r#"{"type":"job_warning","job_id":"j","message":"w","timestamp":1}"#,
        ] {
            let event: CrawlEvent = serde_json::from_str(json).unwrap();
            let back = serde_json::to_value(&event).unwrap();
            let orig: serde_json::Value = serde_json::from_str(json).unwrap();
            for (k, v) in orig.as_object().unwrap() {
                assert_eq!(&back[k], v, "{json}: field {k}");
            }
        }
    }
}
