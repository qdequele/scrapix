//! `GET /job/{id}/results` (SCR-71): paginated retrieval of the documents a
//! job produced, each item shaped like a `/scrape` response.
//!
//! ## Sources
//!
//! - **Crawl jobs** index their pages into Meilisearch (content worker);
//!   every document carries `_crawl_job_id`, which is always filterable.
//!   Results are read from the job's index with
//!   `POST /indexes/{uid}/documents/fetch` filtered on `_crawl_job_id`.
//! - Jobs the engine runs itself (batch scrape, extract) store one result
//!   per URL with [`store_page`]: in the `job_results` Postgres table
//!   (`seq` = completion order, append-only), or in memory when the engine
//!   has no database (or could not persist the job row). The cursor is the
//!   `seq` of the last item read, so paging never skips or repeats an item
//!   even while the job runs.
//!
//! ## Paging and ordering
//!
//! `cursor` is opaque. For crawl jobs it encodes an offset into the
//! filtered document set: Meilisearch returns fetched documents in internal
//! document-id order, which is stable for existing documents (an update of
//! an existing URL keeps its internal id), and the index has no attribute
//! that is guaranteed sortable (`crawled_at` is only sortable with the
//! default settings, and `keep_settings` jobs keep the index's own).
//! Consequences, documented in the API reference:
//! - after the job finished, paging is exact;
//! - while it runs, pages indexed later are usually appended, but a new
//!   document may reuse a freed internal id (documents deleted from the
//!   same index) and land on an already-read page. Re-read from the start
//!   after completion for an exact snapshot.
//!
//! `next` is `null` only when the job is terminal and every result was
//! returned. While the job runs it is always set (possibly to the same
//! position), so a client can poll it for new results.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::warn;

use scrapix_core::{JobState, JobStatus};

use crate::auth::{AuthenticatedAccount, AuthenticatedUser};
use crate::job_kind::JobKind;
use crate::{
    check_job_ownership, extract_account_context, is_terminal, jobs_db, AccountContext, ApiError,
    AppState,
};

/// Default page size of `GET /job/{id}/results`.
pub(crate) const DEFAULT_RESULTS_LIMIT: usize = 20;
/// Maximum page size (items can carry full page markdown/HTML).
pub(crate) const MAX_RESULTS_LIMIT: usize = 100;
/// How many crawl jobs' Meilisearch targets are remembered in memory.
const MAX_CRAWL_TARGETS: usize = 10_000;

// ============================================================================
// Types
// ============================================================================

/// Query parameters of `GET /job/{id}/results`
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct JobResultsQuery {
    /// Page size (default 20, max 100)
    pub limit: Option<usize>,
    /// Opaque cursor from a previous response's `next`
    pub cursor: Option<String>,
}

/// One page of a job's results
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct JobResultsResponse {
    pub job_id: String,
    /// `crawl`, `batch_scrape` or `extract`
    pub job_type: JobKind,
    /// Job status at the time of the read
    #[schema(value_type = JobStatus)]
    pub status: String,
    /// Number of results available so far
    pub total: u64,
    /// Cursor of the next page; `null` once the job is terminal and every
    /// result was returned. Always set while the job runs.
    pub next: Option<String>,
    /// Results, each shaped like a `/scrape` response
    #[schema(value_type = Vec<JobResultItem>)]
    pub data: Vec<Value>,
}

/// A single job result, shaped like a `/scrape` response. Fields a job did
/// not produce (formats/features it did not request) are omitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct JobResultItem {
    /// Whether this page was scraped successfully
    pub success: bool,
    /// Page URL (after redirects)
    pub url: String,
    /// URL as submitted (batch scrape / extract)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Position of `source_url` in the submitted `urls` (batch scrape / extract)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// Why this page failed (`success: false`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JobResultError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Page metadata (same shape as `ScrapeResponse.metadata`)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// JSON-LD / structured data
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<Object>>)]
    pub blocks: Option<Value>,
    /// Custom selector extraction results
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub extract: Option<Value>,
    /// AI enrichment results (`summary`, `extract`)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub ai: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrape_duration_ms: Option<u64>,
    /// Meilisearch document id (crawl jobs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// When the page was crawled (crawl jobs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawled_at: Option<String>,
    /// Block index within the page, when the crawl split pages into blocks
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_block: Option<u64>,
    /// URL of the block (with its anchor), when the crawl split pages into blocks
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_url: Option<String>,
}

/// Error of a failed result item
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct JobResultError {
    /// Machine-readable code (`fetch_error`, `http_error`, `validation_error`, ...)
    pub code: String,
    pub message: String,
}

/// Where a crawl job's documents live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MeiliTarget {
    pub url: String,
    pub api_key: Option<String>,
}

/// Results-layer state kept in `AppState`.
pub(crate) struct ResultsState {
    /// Client for reading crawl jobs' Meilisearch indexes.
    http: reqwest::Client,
    /// Meilisearch connection of recent crawl jobs (the persisted job config
    /// has its API key redacted). Insertion-ordered, bounded.
    crawl_targets: RwLock<(HashMap<String, MeiliTarget>, VecDeque<String>)>,
    /// Results of engine-run jobs that are not persisted in Postgres.
    memory: RwLock<MemoryResults>,
}

impl Default for ResultsState {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            crawl_targets: RwLock::new((HashMap::new(), VecDeque::new())),
            memory: RwLock::new(MemoryResults::default()),
        }
    }
}

impl ResultsState {
    /// Remember where a crawl job indexes its documents (called when the
    /// job is created, while the unredacted config is at hand).
    pub(crate) fn remember_crawl_target(&self, job_id: &str, url: &str, api_key: &str) {
        if url.is_empty() {
            return;
        }
        let target = MeiliTarget {
            url: url.to_string(),
            api_key: (!api_key.is_empty()).then(|| api_key.to_string()),
        };
        let mut guard = self.crawl_targets.write();
        let (map, order) = &mut *guard;
        if map.insert(job_id.to_string(), target).is_none() {
            order.push_back(job_id.to_string());
        }
        while order.len() > MAX_CRAWL_TARGETS {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
    }

    fn crawl_target(&self, job_id: &str) -> Option<MeiliTarget> {
        self.crawl_targets.read().0.get(job_id).cloned()
    }
}

// ============================================================================
// Cursor
// ============================================================================

/// Encode a result position as an opaque cursor.
pub(crate) fn encode_cursor(position: u64) -> String {
    hex::encode(format!("v1:{position}"))
}

/// Decode a cursor produced by [`encode_cursor`].
pub(crate) fn decode_cursor(cursor: &str) -> Result<u64, ApiError> {
    let invalid = || ApiError::new("Invalid cursor", "validation_error");
    let bytes = hex::decode(cursor).map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    text.strip_prefix("v1:")
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(invalid)
}

/// Clamp a requested page size.
pub(crate) fn clamp_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_RESULTS_LIMIT)
        .clamp(1, MAX_RESULTS_LIMIT)
}

/// `next` for a page ending at position `end`: `null` only when the job
/// is terminal and nothing is left.
pub(crate) fn next_cursor(end: u64, has_more: bool, terminal: bool) -> Option<String> {
    if terminal && !has_more {
        None
    } else {
        Some(encode_cursor(end))
    }
}

/// A page read from a result source.
#[derive(Debug, Default)]
pub(crate) struct ResultsSlice {
    pub data: Vec<Value>,
    /// Number of results available so far
    pub total: u64,
    /// Position after the last returned item (the next cursor)
    pub end: u64,
    /// Whether results exist after `end`
    pub has_more: bool,
}

pub(crate) fn status_str(status: &JobStatus) -> String {
    format!("{:?}", status).to_lowercase()
}

// ============================================================================
// Handler
// ============================================================================

/// The job `job_id` (in memory, else Postgres), if the caller owns it.
/// Same lookup and ownership rules as `GET /job/{id}/status`.
pub(crate) async fn find_owned_job(
    state: &AppState,
    account_ctx: &Option<AccountContext>,
    job_id: &str,
) -> Result<JobState, ApiError> {
    let job = if let Some(job) = state.get_job(job_id) {
        job
    } else if let Some(ref pool) = state.db_pool {
        if let Some(ctx) = account_ctx {
            jobs_db::get_job_for_account(pool, job_id, &ctx.account_id).await
        } else {
            jobs_db::get_job_from_db(pool, job_id).await
        }
        .ok_or_else(|| ApiError::new("Job not found", "not_found"))?
    } else {
        return Err(ApiError::new("Job not found", "not_found"));
    };
    check_job_ownership(&job, account_ctx)?;
    Ok(job)
}

/// Get the results of a job
///
/// Paginated documents produced by a crawl, batch scrape or extract job,
/// each shaped like a `/scrape` response. Works while the job is running
/// (partial results) and after it finished. Follow `next` until it is
/// `null`; while the job runs, `next` stays set so it can be polled.
#[utoipa::path(
    get,
    path = "/job/{id}/results",
    tag = "jobs",
    params(("id" = String, Path, description = "Job ID"), JobResultsQuery),
    responses(
        (status = 200, body = JobResultsResponse),
        (status = 400, description = "Invalid cursor", body = ApiError),
        (status = 404, body = ApiError),
        (status = 503, description = "The job's result store is unreachable", body = ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn job_results(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    user_ext: Option<Extension<AuthenticatedUser>>,
    Path(job_id): Path<String>,
    Query(query): Query<JobResultsQuery>,
) -> Result<Json<JobResultsResponse>, ApiError> {
    let account_ctx =
        extract_account_context(state.db_pool.as_ref(), &account_ext, &user_ext).await;
    let job = find_owned_job(&state, &account_ctx, &job_id).await?;
    results_page(&state, &job, query.limit, query.cursor.as_deref())
        .await
        .map(Json)
}

/// One page of `job`'s results, whatever produced them.
pub(crate) async fn results_page(
    state: &AppState,
    job: &JobState,
    limit: Option<usize>,
    cursor: Option<&str>,
) -> Result<JobResultsResponse, ApiError> {
    let limit = clamp_limit(limit);
    let start = cursor.map(decode_cursor).transpose()?.unwrap_or(0);
    let kind = JobKind::of(job);
    // `job` is a snapshot taken before the read: if it says terminal, every
    // result was written before the read started, so `next: null` is safe.
    let slice = match kind {
        JobKind::Crawl => crawl_results(state, job, start, limit).await?,
        JobKind::BatchScrape | JobKind::Extract => {
            stored_results(state, &job.job_id, start, limit).await?
        }
    };
    Ok(JobResultsResponse {
        job_id: job.job_id.clone(),
        job_type: kind,
        status: status_str(&job.status),
        total: slice.total,
        next: next_cursor(slice.end, slice.has_more, is_terminal(&job.status)),
        data: slice.data,
    })
}

// ============================================================================
// POST /crawl/sync?include_results=true
// ============================================================================

/// Query parameters of `POST /crawl/sync`
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct CrawlSyncQuery {
    /// Also return the first page of the job's results (default false)
    #[serde(default)]
    pub include_results: bool,
    /// Page size of the included results (default 20, max 100)
    pub results_limit: Option<usize>,
}

/// `POST /crawl/sync` response: the final job status, plus the first page
/// of results when `include_results=true`
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct CrawlSyncResponse {
    #[serde(flatten)]
    pub status: crate::JobStatusResponse,
    /// First page of results (`include_results=true`). Follow `results.next`
    /// with `GET /job/{id}/results`. Meilisearch indexes asynchronously, so
    /// the very last pages of a just-finished crawl can take a moment to
    /// appear.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<JobResultsResponse>,
    /// Why the results could not be read (`include_results=true`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results_error: Option<String>,
}

pub(crate) async fn crawl_sync_response(
    state: &AppState,
    job: JobState,
    query: &CrawlSyncQuery,
) -> CrawlSyncResponse {
    let (results, results_error) = if query.include_results {
        match results_page(state, &job, query.results_limit, None).await {
            Ok(page) => (Some(page), None),
            Err(e) => (None, Some(e.error.clone())),
        }
    } else {
        (None, None)
    };
    CrawlSyncResponse {
        status: job.into(),
        results,
        results_error,
    }
}

// ============================================================================
// Crawl jobs: Meilisearch
// ============================================================================

async fn crawl_results(
    state: &AppState,
    job: &JobState,
    offset: u64,
    limit: usize,
) -> Result<ResultsSlice, ApiError> {
    let target = resolve_crawl_target(state, job).await?;
    let (docs, total) = fetch_job_documents(
        &state.results.http,
        &target,
        &job.index_uid,
        &job.job_id,
        offset,
        limit,
    )
    .await?;
    let data: Vec<Value> = docs
        .iter()
        .map(|d| serde_json::to_value(document_to_item(d)).unwrap_or(Value::Null))
        .collect();
    let end = offset + data.len() as u64;
    Ok(ResultsSlice {
        data,
        total,
        end,
        has_more: end < total,
    })
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Where `job`'s documents live: the connection remembered at creation,
/// else the Replace-strategy connection persisted with the job, else the
/// account's engine matching the job's (redacted) config URL, else the
/// server's own `MEILISEARCH_URL`/`MEILISEARCH_API_KEY`.
async fn resolve_crawl_target(state: &AppState, job: &JobState) -> Result<MeiliTarget, ApiError> {
    if let Some(target) = state.results.crawl_target(&job.job_id) {
        return Ok(target);
    }
    if let Some(ref url) = job.swap_meilisearch_url {
        return Ok(MeiliTarget {
            url: url.clone(),
            api_key: job.swap_meilisearch_api_key.clone(),
        });
    }
    let config_url = job
        .config
        .as_ref()
        .and_then(|c| c.pointer("/meilisearch/url"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let (Some(pool), Some(account)) = (&state.db_pool, &job.account_id) {
        if let Ok(account_uuid) = account.parse::<uuid::Uuid>() {
            use sqlx::Row as _;
            let row = match config_url {
                Some(ref url) => {
                    sqlx::query(
                        "SELECT url, api_key FROM meilisearch_engines \
                     WHERE account_id = $1 AND rtrim(url, '/') = rtrim($2, '/') \
                     ORDER BY is_default DESC LIMIT 1",
                    )
                    .bind(account_uuid)
                    .bind(url)
                    .fetch_optional(pool)
                    .await
                }
                None => {
                    sqlx::query(
                        "SELECT url, api_key FROM meilisearch_engines \
                     WHERE account_id = $1 AND is_default = true LIMIT 1",
                    )
                    .bind(account_uuid)
                    .fetch_optional(pool)
                    .await
                }
            };
            match row {
                Ok(Some(row)) => {
                    let api_key: Option<String> = row.try_get("api_key").ok();
                    return Ok(MeiliTarget {
                        url: row.get("url"),
                        api_key: api_key.filter(|k| !k.is_empty()),
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(job_id = %job.job_id, error = %e, "Engine lookup for job results failed")
                }
            }
        }
    }

    let env_url = std::env::var("MEILISEARCH_URL")
        .ok()
        .filter(|s| !s.is_empty());
    let env_key = std::env::var("MEILISEARCH_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    match (config_url, env_url) {
        (Some(url), Some(env)) if same_url(&url, &env) => Ok(MeiliTarget {
            url,
            api_key: env_key,
        }),
        // An unknown key: try without one (an unsecured instance).
        (Some(url), _) => Ok(MeiliTarget { url, api_key: None }),
        (None, Some(url)) => Ok(MeiliTarget {
            url,
            api_key: env_key,
        }),
        (None, None) => Err(ApiError::new(
            "The Meilisearch instance of this job is unknown",
            "not_found",
        )),
    }
}

/// Escape a value for a double-quoted Meilisearch filter string.
fn filter_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[derive(Deserialize)]
struct FetchDocumentsResponse {
    #[serde(default)]
    results: Vec<Value>,
    #[serde(default)]
    total: u64,
}

#[derive(Deserialize, Default)]
struct MeiliErrorBody {
    #[serde(default)]
    code: String,
}

/// Read a page of the documents `job_id` indexed into `index_uid`.
/// A missing index (nothing indexed yet) is an empty result.
pub(crate) async fn fetch_job_documents(
    http: &reqwest::Client,
    target: &MeiliTarget,
    index_uid: &str,
    job_id: &str,
    offset: u64,
    limit: usize,
) -> Result<(Vec<Value>, u64), ApiError> {
    if index_uid.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let url = format!(
        "{}/indexes/{}/documents/fetch",
        target.url.trim_end_matches('/'),
        index_uid
    );
    let body = serde_json::json!({
        "filter": format!("_crawl_job_id = \"{}\"", filter_escape(job_id)),
        "offset": offset,
        "limit": limit,
    });
    let mut request = http.post(&url).json(&body);
    if let Some(ref key) = target.api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|e| {
        warn!(job_id = %job_id, error = %e, "Could not reach Meilisearch for job results");
        ApiError::new(
            "Could not reach the Meilisearch instance of this job",
            "service_unavailable",
        )
    })?;
    let status = response.status();
    if status.is_success() {
        let parsed: FetchDocumentsResponse = response.json().await.map_err(|_| {
            ApiError::new(
                "Unexpected response from the Meilisearch instance of this job",
                "service_unavailable",
            )
        })?;
        return Ok((parsed.results, parsed.total));
    }
    // Never echo the upstream body: only its error code.
    let err: MeiliErrorBody = response.json().await.unwrap_or_default();
    match err.code.as_str() {
        "index_not_found" => Ok((Vec::new(), 0)),
        "invalid_document_filter" => Err(ApiError::new(
            "The job's index cannot be filtered by _crawl_job_id yet; retry shortly",
            "conflict",
        )),
        "invalid_api_key" | "missing_authorization_header" => Err(ApiError::new(
            "The engine has no valid API key for this job's Meilisearch instance",
            "service_unavailable",
        )),
        code => Err(ApiError::new(
            format!(
                "Meilisearch returned HTTP {} ({})",
                status.as_u16(),
                if code.is_empty() {
                    "no error code"
                } else {
                    code
                }
            ),
            "service_unavailable",
        )),
    }
}

fn str_field(doc: &Value, key: &str) -> Option<String> {
    doc.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn some_value(doc: &Value, key: &str) -> Option<Value> {
    doc.get(key).filter(|v| !v.is_null()).cloned()
}

/// `ScrapeResponse.metadata`-shaped object from a document's title and raw
/// meta tags (as normalized by the parser: `og:title` → `title`, ...).
fn document_metadata(doc: &Value) -> Option<Value> {
    let meta = doc.get("metadata").and_then(|m| m.as_object());
    let title = str_field(doc, "title");
    if meta.is_none() && title.is_none() {
        return None;
    }
    let empty = Map::new();
    let meta = meta.unwrap_or(&empty);
    let get = |k: &str| {
        meta.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let keywords: Vec<String> = get("keywords")
        .map(|k| {
            k.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mut open_graph = Map::new();
    let mut twitter = Map::new();
    for (k, v) in meta {
        let Some(v) = v.as_str() else { continue };
        if let Some(rest) = k.strip_prefix("og:") {
            open_graph.insert(rest.to_string(), Value::String(v.to_string()));
        } else if matches!(k.as_str(), "image" | "url" | "type" | "site_name") {
            open_graph.insert(k.clone(), Value::String(v.to_string()));
        } else if let Some(rest) = k
            .strip_prefix("twitter:")
            .or_else(|| k.strip_prefix("twitter_"))
        {
            twitter.insert(rest.to_string(), Value::String(v.to_string()));
        }
    }
    let mut out = Map::new();
    out.insert("title".into(), title.or_else(|| get("title")).into());
    out.insert("description".into(), get("description").into());
    out.insert("author".into(), get("author").into());
    out.insert("keywords".into(), keywords.into());
    if let Some(published) = get("article:published_time") {
        out.insert("published_date".into(), published.into());
    }
    if !open_graph.is_empty() {
        out.insert("open_graph".into(), Value::Object(open_graph));
    }
    if !twitter.is_empty() {
        out.insert("twitter".into(), Value::Object(twitter));
    }
    Some(Value::Object(out))
}

/// Map an indexed Meilisearch document to a `/scrape`-shaped result item.
pub(crate) fn document_to_item(doc: &Value) -> JobResultItem {
    let ai = {
        let summary = some_value(doc, "ai_summary");
        let extract = some_value(doc, "ai_extraction");
        if summary.is_none() && extract.is_none() {
            None
        } else {
            let mut ai = Map::new();
            if let Some(s) = summary {
                ai.insert("summary".into(), s);
            }
            if let Some(e) = extract {
                ai.insert("extract".into(), e);
            }
            Some(Value::Object(ai))
        }
    };
    JobResultItem {
        success: true,
        url: str_field(doc, "url").unwrap_or_default(),
        status_code: Some(200),
        markdown: str_field(doc, "markdown"),
        content: str_field(doc, "content"),
        metadata: document_metadata(doc),
        language: str_field(doc, "language"),
        schema: some_value(doc, "schema"),
        extract: some_value(doc, "custom"),
        ai,
        document_id: str_field(doc, "uid"),
        crawled_at: str_field(doc, "crawled_at"),
        page_block: doc.get("page_block").and_then(|v| v.as_u64()),
        block_url: str_field(doc, "block_url"),
        ..Default::default()
    }
}

// ============================================================================
// Engine-run jobs (batch scrape, extract): stored results
// ============================================================================

/// How many jobs' results the in-memory fallback keeps.
const MAX_MEMORY_RESULT_JOBS: usize = 50;

/// In-memory results of jobs that are not persisted (no Postgres, or the
/// job row could not be written).
#[derive(Default)]
pub(crate) struct MemoryResults {
    pages: HashMap<String, Vec<Value>>,
    summaries: HashMap<String, Value>,
    order: VecDeque<String>,
}

impl ResultsState {
    /// Keep `job_id`'s results in memory instead of Postgres.
    pub(crate) fn use_memory(&self, job_id: &str) {
        let mut m = self.memory.write();
        if m.pages.contains_key(job_id) {
            return;
        }
        m.pages.insert(job_id.to_string(), Vec::new());
        m.order.push_back(job_id.to_string());
        while m.order.len() > MAX_MEMORY_RESULT_JOBS {
            if let Some(old) = m.order.pop_front() {
                m.pages.remove(&old);
                m.summaries.remove(&old);
            }
        }
    }

    fn in_memory(&self, job_id: &str) -> bool {
        self.memory.read().pages.contains_key(job_id)
    }
}

/// Store result `seq` (1-based, in completion order) of an engine-run job.
/// A Postgres write is retried; if it keeps failing the item is lost from
/// the results (logged, and counted as a job warning).
pub(crate) async fn store_page(
    state: &AppState,
    job_id: &str,
    seq: u64,
    url: &str,
    success: bool,
    payload: Value,
) {
    if state.results.in_memory(job_id) || state.db_pool.is_none() {
        let mut m = state.results.memory.write();
        if let Some(pages) = m.pages.get_mut(job_id) {
            pages.push(payload);
        }
        return;
    }
    let Some(ref pool) = state.db_pool else {
        return;
    };
    let mut last_error = None;
    for attempt in 0..3u64 {
        let res = sqlx::query(
            "INSERT INTO job_results (job_id, seq, kind, url, success, payload) \
             VALUES ($1, $2, 'page', $3, $4, $5) ON CONFLICT (job_id, seq) DO NOTHING",
        )
        .bind(job_id)
        .bind(seq as i32)
        .bind(url)
        .bind(success)
        .bind(&payload)
        .execute(pool)
        .await;
        match res {
            Ok(_) => return,
            Err(e) => {
                last_error = Some(e);
                tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
            }
        }
    }
    let error = last_error.map(|e| e.to_string()).unwrap_or_default();
    tracing::error!(job_id = %job_id, seq, error = %error, "Failed to store a job result");
    state.update_job(job_id, |j| {
        let msg = "Some results could not be stored (database error)".to_string();
        if !j.warnings.contains(&msg) {
            j.warnings.push(msg);
        }
    });
}

/// Store (replace) the summary of an extract job.
pub(crate) async fn store_summary(state: &AppState, job_id: &str, payload: Value) {
    if state.results.in_memory(job_id) || state.db_pool.is_none() {
        let mut m = state.results.memory.write();
        if m.pages.contains_key(job_id) {
            m.summaries.insert(job_id.to_string(), payload);
        }
        return;
    }
    let Some(ref pool) = state.db_pool else {
        return;
    };
    if let Err(e) = sqlx::query(
        "INSERT INTO job_results (job_id, seq, kind, url, success, payload) \
         VALUES ($1, 0, 'extract', NULL, true, $2) \
         ON CONFLICT (job_id, seq) DO UPDATE SET payload = EXCLUDED.payload",
    )
    .bind(job_id)
    .bind(&payload)
    .execute(pool)
    .await
    {
        tracing::error!(job_id = %job_id, error = %e, "Failed to store an extract result");
    }
}

/// The summary of an extract job, if stored.
pub(crate) async fn load_summary(state: &AppState, job_id: &str) -> Option<Value> {
    if state.results.in_memory(job_id) {
        return state.results.memory.read().summaries.get(job_id).cloned();
    }
    let pool = state.db_pool.as_ref()?;
    sqlx::query_scalar::<_, Value>(
        "SELECT payload FROM job_results WHERE job_id = $1 AND kind = 'extract' LIMIT 1",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| warn!(job_id = %job_id, error = %e, "Failed to load an extract result"))
    .ok()
    .flatten()
}

/// Stored results of an engine-run job after position `after` (the seq of
/// the last item already read).
async fn stored_results(
    state: &AppState,
    job_id: &str,
    after: u64,
    limit: usize,
) -> Result<ResultsSlice, ApiError> {
    if state.results.in_memory(job_id) || state.db_pool.is_none() {
        let m = state.results.memory.read();
        let pages = m.pages.get(job_id).map(Vec::as_slice).unwrap_or_default();
        let start = (after as usize).min(pages.len());
        let end = (start + limit).min(pages.len());
        return Ok(ResultsSlice {
            data: pages[start..end].to_vec(),
            total: pages.len() as u64,
            end: end as u64,
            has_more: end < pages.len(),
        });
    }
    let Some(ref pool) = state.db_pool else {
        unreachable!("checked above");
    };
    use sqlx::Row as _;
    let db_error = |e: sqlx::Error| {
        warn!(job_id = %job_id, error = %e, "Failed to read job results");
        ApiError::new("Could not read the job's results", "service_unavailable")
    };
    let rows = sqlx::query(
        "SELECT seq, payload FROM job_results \
         WHERE job_id = $1 AND kind = 'page' AND seq > $2 ORDER BY seq LIMIT $3",
    )
    .bind(job_id)
    .bind(after.min(i32::MAX as u64) as i32)
    .bind(limit as i64 + 1)
    .fetch_all(pool)
    .await
    .map_err(db_error)?;
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM job_results WHERE job_id = $1 AND kind = 'page'")
            .bind(job_id)
            .fetch_one(pool)
            .await
            .map_err(db_error)?;
    let has_more = rows.len() > limit;
    let mut end = after;
    let mut data = Vec::with_capacity(rows.len().min(limit));
    for row in rows.into_iter().take(limit) {
        end = row.get::<i32, _>("seq") as u64;
        data.push(row.get::<Value, _>("payload"));
    }
    Ok(ResultsSlice {
        data,
        total: total as u64,
        end,
        has_more,
    })
}

/// Test helpers shared by the results, batch and extract tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;
    use std::time::Duration;

    use scrapix_crawler::{HttpFetcherBuilder, RobotsCache, RobotsConfig};
    use scrapix_queue::{AnyProducer, ChannelBus};

    use crate::{webhooks, AppConfig, AppState};

    /// An `AppState` without Postgres/ClickHouse/AI whose fetcher may reach
    /// local test servers.
    pub(crate) fn test_state(bus: &ChannelBus) -> Arc<AppState> {
        test_state_with_ai(bus, None)
    }

    /// Same, with an AI service (e.g. an OpenAI-compatible mock).
    pub(crate) fn test_state_with_ai(
        bus: &ChannelBus,
        ai_service: Option<Arc<scrapix_ai::AiService>>,
    ) -> Arc<AppState> {
        let robots = Arc::new(
            RobotsCache::new(RobotsConfig {
                respect_robots: false,
                allow_private_ips: true,
                ..Default::default()
            })
            .unwrap(),
        );
        let fetcher = Arc::new(
            HttpFetcherBuilder::new()
                .allow_private_ips(true)
                .build(robots)
                .unwrap(),
        );
        Arc::new(AppState::new(
            AnyProducer::channel(bus.producer()),
            AppConfig {
                max_jobs: 1000,
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
            ai_service,
            None,
            None,
            None,
            webhooks::WebhookDispatcher::new(
                scrapix_crawler::safe_client_builder(None, true)
                    .build()
                    .unwrap(),
                webhooks::DEFAULT_MAX_CONCURRENT_DELIVERIES,
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        for n in [0u64, 1, 20, 123_456_789] {
            assert_eq!(decode_cursor(&encode_cursor(n)).unwrap(), n);
        }
        assert!(decode_cursor("zz").is_err());
        assert!(decode_cursor(&hex::encode("v2:10")).is_err());
        assert!(decode_cursor(&hex::encode("v1:-3")).is_err());
    }

    #[test]
    fn limit_is_clamped() {
        assert_eq!(clamp_limit(None), DEFAULT_RESULTS_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10_000)), MAX_RESULTS_LIMIT);
        assert_eq!(clamp_limit(Some(7)), 7);
    }

    #[test]
    fn next_is_null_only_when_terminal_and_exhausted() {
        assert_eq!(next_cursor(10, false, true), None);
        assert_eq!(next_cursor(5, true, true), Some(encode_cursor(5)));
        // Running: always a cursor to poll, even with nothing new.
        assert_eq!(next_cursor(10, false, false), Some(encode_cursor(10)));
    }

    #[test]
    fn document_maps_to_scrape_shape() {
        let doc = serde_json::json!({
            "uid": "abc",
            "url": "https://a.test/page",
            "domain": "a.test",
            "title": "Page",
            "markdown": "# Page",
            "content": "Page",
            "metadata": {
                "description": "desc",
                "keywords": "a, b ,,c",
                "image": "https://a.test/i.png",
                "twitter_title": "T",
                "viewport": "width=device-width"
            },
            "language": "en",
            "ai_summary": "sum",
            "custom": { "price": "1" },
            "crawled_at": "2026-09-27T00:00:00Z",
            "_crawl_job_id": "job-1"
        });
        let item = serde_json::to_value(document_to_item(&doc)).unwrap();
        assert_eq!(item["success"], true);
        assert_eq!(item["url"], "https://a.test/page");
        assert_eq!(item["markdown"], "# Page");
        assert_eq!(item["metadata"]["title"], "Page");
        assert_eq!(item["metadata"]["description"], "desc");
        assert_eq!(
            item["metadata"]["keywords"],
            serde_json::json!(["a", "b", "c"])
        );
        assert_eq!(
            item["metadata"]["open_graph"]["image"],
            "https://a.test/i.png"
        );
        assert_eq!(item["metadata"]["twitter"]["title"], "T");
        assert_eq!(item["ai"]["summary"], "sum");
        assert!(item["ai"].get("extract").is_none());
        assert_eq!(item["extract"]["price"], "1");
        assert_eq!(item["document_id"], "abc");
        // Internal fields never leak.
        assert!(item.get("_crawl_job_id").is_none());
        assert!(item.get("html").is_none());
    }

    #[test]
    fn filter_values_are_escaped() {
        assert_eq!(filter_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[tokio::test]
    async fn fetches_filtered_documents_from_meilisearch() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/indexes/docs/documents/fetch"))
            .and(header("authorization", "Bearer key"))
            .and(body_partial_json(serde_json::json!({
                "filter": "_crawl_job_id = \"job-1\"",
                "offset": 2,
                "limit": 2
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{ "uid": "1", "url": "https://a.test/1" }, { "uid": "2", "url": "https://a.test/2" }],
                "offset": 2, "limit": 2, "total": 5
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/indexes/missing/documents/fetch"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "message": "Index `missing` not found.", "code": "index_not_found",
                "type": "invalid_request", "link": ""
            })))
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let target = MeiliTarget {
            url: server.uri(),
            api_key: Some("key".into()),
        };
        let (docs, total) = fetch_job_documents(&http, &target, "docs", "job-1", 2, 2)
            .await
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(docs.len(), 2);

        let (docs, total) = fetch_job_documents(&http, &target, "missing", "job-1", 0, 2)
            .await
            .unwrap();
        assert!(docs.is_empty());
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn crawl_job_results_page_through_meilisearch() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let docs: Vec<Value> = (0..3)
            .map(|i| serde_json::json!({ "uid": format!("d{i}"), "url": format!("https://a.test/{i}"), "markdown": "x" }))
            .collect();
        for (offset, page) in [(0u64, &docs[0..2]), (2, &docs[2..3])] {
            Mock::given(method("POST"))
                .and(path("/indexes/idx/documents/fetch"))
                .and(body_partial_json(
                    serde_json::json!({ "offset": offset, "limit": 2 }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": page, "offset": offset, "limit": 2, "total": 3
                })))
                .mount(&server)
                .await;
        }

        let bus = scrapix_queue::ChannelBus::new();
        let state = test_support::test_state(&bus);
        let mut job = JobState::new("job-1", "idx");
        job.start();
        state.insert_job(job);
        state
            .results
            .remember_crawl_target("job-1", &server.uri(), "");

        let job = state.get_job("job-1").unwrap();
        let first = results_page(&state, &job, Some(2), None).await.unwrap();
        assert_eq!(first.job_type, JobKind::Crawl);
        assert_eq!(first.total, 3);
        assert_eq!(first.data.len(), 2);
        assert_eq!(first.data[0]["url"], "https://a.test/0");
        let next = first.next.clone().expect("more results");

        // Still running: the last page keeps a cursor to poll.
        let second = results_page(&state, &job, Some(2), Some(&next))
            .await
            .unwrap();
        assert_eq!(second.data.len(), 1);
        assert!(second.next.is_some());

        // Once terminal, the last page ends the listing.
        state.update_job("job-1", |j| j.complete());
        let job = state.get_job("job-1").unwrap();
        let last = results_page(&state, &job, Some(2), Some(&next))
            .await
            .unwrap();
        assert_eq!(last.status, "completed");
        assert_eq!(last.next, None);

        assert!(results_page(&state, &job, None, Some("garbage"))
            .await
            .is_err());
    }
}
