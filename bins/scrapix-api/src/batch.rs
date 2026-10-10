//! `POST /batch/scrape` (SCR-74): scrape many URLs as one job.
//!
//! The API runs the `/scrape` pipeline (`perform_scrape`) for each URL with
//! bounded concurrency. A URL that fails becomes a result item with
//! `success: false` and an `error` instead of failing the batch. The
//! balance and the plan's limits are pre-checked for the batch, and usage
//! is reported per URL by `perform_scrape`, as for `/scrape`. Results are
//! served by `GET /job/{id}/results` (append-only, in the order URLs
//! finish).
//!
//! The body is `{ urls, concurrency?, webhooks?, ...options }` where every
//! other field is a `POST /scrape` option applied to every URL: each URL's
//! `ScrapeRequest` is deserialized from those options plus `url`, so any
//! option `/scrape` accepts works here unchanged.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Extension, State},
    Json,
};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Map, Value};
use tracing::{info, warn};

use scrapix_core::browser::{Action, RequestCookie};
use scrapix_core::config::WebhookConfig;

use crate::auth::AuthenticatedAccount;
use crate::engine_jobs::{self, Gate};
use crate::job_kind::JobKind;
use crate::results::{JobResultError, JobResultItem};
use crate::{
    check_write_permission, extract_account_context, perform_scrape, AccountContext, AiOptions,
    ApiError, AppState, ScrapeFormat, ScrapeRequest, ScrapeSelector, ScreenshotRequestOptions,
};

/// Maximum number of URLs in one batch.
pub(crate) const MAX_BATCH_URLS: usize = 1000;
/// Default number of URLs scraped at once.
pub(crate) const DEFAULT_BATCH_CONCURRENCY: usize = 10;
/// Maximum `concurrency`.
pub(crate) const MAX_BATCH_CONCURRENCY: usize = 25;
/// Extra time a single URL may take on top of its `timeout_ms` (browser
/// rendering, AI enrichment) before it is recorded as timed out.
const PER_URL_GRACE: Duration = Duration::from_secs(120);

/// Request body for `POST /batch/scrape`. Every field besides `urls`,
/// `concurrency` and `webhooks` is a `POST /scrape` option applied to each
/// URL (the most common ones are listed; any `/scrape` option is accepted).
#[derive(Debug, utoipa::ToSchema)]
#[allow(dead_code)] // documentation schema: the handler reads the raw JSON body
pub(crate) struct BatchScrapeRequest {
    /// URLs to scrape (1 to 1000)
    urls: Vec<String>,
    /// URLs scraped at once (default 10, max 25)
    #[schema(nullable = false)]
    concurrency: Option<usize>,
    /// Webhook subscriptions (same as a crawl's `webhooks`): `crawl_started`,
    /// `page_crawled`, `page_error`, `crawl_completed`, `crawl_failed`
    webhooks: Option<Vec<WebhookConfig>>,
    /// Formats to return for each URL (default: markdown, content, metadata)
    formats: Option<Vec<ScrapeFormat>>,
    /// Only the main content (default true)
    only_main_content: Option<bool>,
    /// Include links found on each page
    include_links: Option<bool>,
    /// Render JavaScript (requires Chrome/Chromium on the server)
    render_js: Option<bool>,
    /// Per-URL timeout in milliseconds (default 30000)
    timeout_ms: Option<u64>,
    /// Custom request headers
    headers: Option<HashMap<String, String>>,
    /// CSS selectors to remove before extraction
    exclude_selectors: Option<Vec<String>>,
    /// CSS selectors to keep (only extract from these)
    include_selectors: Option<Vec<String>>,
    /// Custom CSS selector extraction: field name -> a CSS selector, a list
    /// of selectors or a selector definition (as on `/scrape`)
    extract: Option<HashMap<String, ScrapeSelector>>,
    /// AI enrichment, per URL
    ai: Option<AiOptions>,
    /// Screenshot options, used when `formats` includes `"screenshot"`
    screenshot: Option<ScreenshotRequestOptions>,
    /// Browser actions run on each page before capture (forces browser rendering)
    actions: Option<Vec<Action>>,
    /// Emulate a phone (forces browser rendering)
    mobile: Option<bool>,
    /// Cookies sent with each request. A cookie without `domain` goes to each
    /// URL's own host; one with `domain` must be that URL's host or a parent
    /// of it, otherwise that URL fails with a per-item error.
    cookies: Option<Vec<RequestCookie>>,
}

/// Response of `POST /batch/scrape`
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct BatchScrapeResponse {
    pub job_id: String,
    pub status: String,
    /// Number of URLs accepted
    pub urls_count: usize,
    pub message: String,
}

/// A batch after body validation.
#[derive(Debug)]
pub(crate) struct ParsedBatch {
    pub urls: Vec<String>,
    pub concurrency: usize,
    pub webhooks: Vec<WebhookConfig>,
    /// `/scrape` options shared by every URL (no `url` key)
    pub options: Map<String, Value>,
    /// The options parsed for the first URL (validation, plan check)
    pub sample: ScrapeRequest,
}

/// The `ScrapeRequest` for `url` with the batch's shared options.
pub(crate) fn scrape_request_for(
    options: &Map<String, Value>,
    url: &str,
) -> Result<ScrapeRequest, serde_json::Error> {
    let mut body = options.clone();
    body.insert("url".to_string(), Value::String(url.to_string()));
    serde_json::from_value(Value::Object(body))
}

/// Validate a `POST /batch/scrape` body.
pub(crate) fn parse_batch_body(body: Value) -> Result<ParsedBatch, ApiError> {
    let Value::Object(mut options) = body else {
        return Err(ApiError::new(
            "Request body must be a JSON object",
            "validation_error",
        ));
    };
    let urls: Vec<String> = match options.remove("urls") {
        Some(v) => serde_json::from_value(v)
            .map_err(|_| ApiError::new("`urls` must be an array of strings", "validation_error"))?,
        None => return Err(ApiError::new("`urls` is required", "validation_error")),
    };
    let urls: Vec<String> = urls
        .into_iter()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect();
    if urls.is_empty() {
        return Err(ApiError::new(
            "At least one URL is required",
            "validation_error",
        ));
    }
    if urls.len() > MAX_BATCH_URLS {
        return Err(ApiError::new(
            format!(
                "Too many URLs: {} (max {MAX_BATCH_URLS} per batch)",
                urls.len()
            ),
            "validation_error",
        ));
    }
    // Indexes count the non-blank URLs (the ones scraped).
    for (i, url) in urls.iter().enumerate() {
        crate::check_http_url(url)
            .map_err(|msg| ApiError::new(format!("urls[{i}]: {msg}"), "validation_error"))?;
    }
    let concurrency = match options.remove("concurrency") {
        None | Some(Value::Null) => DEFAULT_BATCH_CONCURRENCY,
        Some(v) => v
            .as_u64()
            .filter(|n| *n >= 1)
            .ok_or_else(|| {
                ApiError::new(
                    "`concurrency` must be a positive integer",
                    "validation_error",
                )
            })?
            .min(MAX_BATCH_CONCURRENCY as u64) as usize,
    };
    let mut webhooks: Vec<WebhookConfig> = match options.remove("webhooks") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => serde_json::from_value(v)
            .map_err(|e| ApiError::new(format!("webhooks: {e}"), "validation_error"))?,
    };
    engine_jobs::validate_webhooks(&mut webhooks)?;
    options.remove("url");

    let sample = scrape_request_for(&options, &urls[0])
        .map_err(|e| ApiError::new(format!("Invalid scrape options: {e}"), "validation_error"))?;
    Ok(ParsedBatch {
        urls,
        concurrency,
        webhooks,
        options,
        sample,
    })
}

/// The batch's shared options as shown in the job's `config`: header
/// values are masked (typically auth tokens).
fn redacted_options(options: &Map<String, Value>) -> Value {
    let mut v = Value::Object(options.clone());
    if let Some(headers) = v.get_mut("headers").and_then(|h| h.as_object_mut()) {
        for value in headers.values_mut() {
            *value = Value::String("***".to_string());
        }
    }
    v
}

/// Scrape many URLs as one job
///
/// Starts a job that runs the `/scrape` pipeline for every URL (bounded
/// concurrency) and returns its `job_id` immediately. Track it with
/// `GET /job/{id}/status`, `/job/{id}/events` or webhooks, read the pages
/// with `GET /job/{id}/results`, cancel with `DELETE /job/{id}`. A URL that
/// fails is reported as a result with `success: false` and an `error`; it
/// does not fail the batch. Usage is reported like `/scrape`, per URL
/// scraped. Refused when the account's balance is gone; once a URL finds it
/// gone, the remaining URLs are skipped with `insufficient_credits`.
#[utoipa::path(
    post,
    path = "/batch/scrape",
    tag = "scrape",
    request_body = BatchScrapeRequest,
    responses(
        (status = 200, body = BatchScrapeResponse),
        (status = 400, body = ApiError),
        (status = 402, description = "The account's credit balance is exhausted", body = ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn batch_scrape(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Json(body): Json<Value>,
) -> Result<Json<BatchScrapeResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    check_write_permission(&account_ctx)?;
    let batch = parse_batch_body(body)?;
    start_batch(&state, &account_ctx, batch).await.map(Json)
}

/// Pre-flight checks, job creation and runner spawn.
pub(crate) async fn start_batch(
    state: &Arc<AppState>,
    account_ctx: &Option<AccountContext>,
    batch: ParsedBatch,
) -> Result<BatchScrapeResponse, ApiError> {
    crate::require_ai_provider(state, batch.sample.ai.as_ref())?;
    if batch.sample.render_js && state.browser_renderer.is_none() {
        return Err(ApiError::new(
            "JS rendering is not available (Chrome/Chromium not found on this server)",
            "render_js_unavailable",
        ));
    }
    engine_jobs::preflight(
        state,
        account_ctx.as_ref(),
        engine_jobs::PlanCheck {
            max_depth: None,
            js_rendering: batch.sample.render_js,
        },
    )
    .await?;

    let mut config = serde_json::json!({
        "urls_count": batch.urls.len(),
        "concurrency": batch.concurrency,
        "options": redacted_options(&batch.options),
        "webhooks": batch.webhooks,
    });
    crate::webhooks::redact_webhooks_json(&mut config);
    let job = engine_jobs::start_job(
        state,
        account_ctx,
        JobKind::BatchScrape,
        batch.urls.clone(),
        config,
        batch.webhooks.clone(),
    )
    .await;

    let urls_count = batch.urls.len();
    let runner_state = state.clone();
    let runner_ctx = engine_jobs::clone_account_ctx(account_ctx);
    let job_id = job.job_id.clone();
    tokio::spawn(async move {
        run_batch(
            runner_state,
            runner_ctx,
            job_id,
            batch.urls,
            batch.options,
            batch.concurrency,
        )
        .await;
    });

    Ok(BatchScrapeResponse {
        job_id: job.job_id,
        status: "running".to_string(),
        urls_count,
        message: format!("Batch scrape started with {urls_count} URLs"),
    })
}

/// The outcome of one URL.
pub(crate) struct UrlOutcome {
    pub index: usize,
    pub source_url: String,
    /// The result, typed (for bookkeeping)
    pub item: JobResultItem,
    /// The result as stored and served (every `/scrape` field)
    pub payload: Value,
    pub duration_ms: u64,
    /// Stop scraping the remaining URLs (out of credits).
    pub out_of_credits: bool,
}

fn error_outcome(index: usize, url: &str, code: &str, message: String) -> UrlOutcome {
    let item = JobResultItem {
        success: false,
        url: url.to_string(),
        source_url: Some(url.to_string()),
        index: Some(index),
        error: Some(JobResultError {
            code: code.to_string(),
            message,
        }),
        ..Default::default()
    };
    let payload = serde_json::to_value(&item).unwrap_or(Value::Null);
    UrlOutcome {
        index,
        source_url: url.to_string(),
        item,
        payload,
        duration_ms: 0,
        out_of_credits: code == "insufficient_credits" || code == "spend_limit_exceeded",
    }
}

/// Scrape one URL of a batch (`options` shared, `url` its own).
pub(crate) async fn scrape_one(
    state: &Arc<AppState>,
    account_ctx: &Option<AccountContext>,
    options: &Map<String, Value>,
    index: usize,
    url: &str,
) -> UrlOutcome {
    let request = match scrape_request_for(options, url) {
        Ok(r) => r,
        Err(e) => return error_outcome(index, url, "validation_error", e.to_string()),
    };
    let budget = Duration::from_millis(request.timeout_ms) + PER_URL_GRACE;
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(budget, perform_scrape(state, account_ctx, &request)).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    match result {
        Err(_) => error_outcome(
            index,
            url,
            "timeout",
            format!("Timed out after {} ms", budget.as_millis()),
        ),
        Ok(Err(e)) => {
            let mut out = error_outcome(index, url, &e.code, e.error.clone());
            out.duration_ms = duration_ms;
            out
        }
        Ok(Ok(response)) => {
            let mut payload = serde_json::to_value(&response).unwrap_or(Value::Null);
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("source_url".into(), Value::String(url.to_string()));
                obj.insert("index".into(), Value::from(index));
                if !response.success {
                    let error = JobResultError {
                        code: "http_error".to_string(),
                        message: format!("HTTP {}", response.status_code),
                    };
                    obj.insert(
                        "error".into(),
                        serde_json::to_value(error).unwrap_or(Value::Null),
                    );
                }
            }
            let item: JobResultItem = serde_json::from_value(payload.clone()).unwrap_or_default();
            UrlOutcome {
                index,
                source_url: url.to_string(),
                item,
                payload,
                duration_ms,
                out_of_credits: false,
            }
        }
    }
}

/// Run a batch to the end (or until cancelled).
async fn run_batch(
    state: Arc<AppState>,
    account_ctx: Option<AccountContext>,
    job_id: String,
    urls: Vec<String>,
    options: Map<String, Value>,
    concurrency: usize,
) {
    let started = std::time::Instant::now();
    let account_ctx = Arc::new(account_ctx);
    let options = Arc::new(options);
    let out_of_credits = Arc::new(AtomicBool::new(false));
    let account_id = account_ctx.as_ref().as_ref().map(|c| c.account_id.clone());

    let mut outcomes = futures::stream::iter(urls.into_iter().enumerate())
        .map(|(index, url)| {
            let state = state.clone();
            let account_ctx = account_ctx.clone();
            let options = options.clone();
            let job_id = job_id.clone();
            let out_of_credits = out_of_credits.clone();
            async move {
                // Checked when the URL's turn comes: cancel stops the rest,
                // pause holds it.
                if engine_jobs::gate(&state, &job_id).await == Gate::Stop {
                    return None;
                }
                if out_of_credits.load(Ordering::Relaxed) {
                    return Some(error_outcome(
                        index,
                        &url,
                        "insufficient_credits",
                        "Skipped: the account ran out of credits during the batch".to_string(),
                    ));
                }
                Some(scrape_one(&state, &account_ctx, &options, index, &url).await)
            }
        })
        .buffer_unordered(concurrency.max(1));

    let (mut seq, mut succeeded, mut failed) = (0u64, 0u64, 0u64);
    while let Some(outcome) = outcomes.next().await {
        let Some(outcome) = outcome else { continue };
        if outcome.out_of_credits {
            out_of_credits.store(true, Ordering::Relaxed);
        }
        seq += 1;
        crate::results::store_page(
            &state,
            &job_id,
            seq,
            &outcome.source_url,
            outcome.item.success,
            outcome.payload,
        )
        .await;
        let error = outcome
            .item
            .error
            .as_ref()
            .map(|e| format!("{}: {}", e.code, e.message));
        if outcome.item.success {
            succeeded += 1;
        } else {
            failed += 1;
        }
        let url = if outcome.item.url.is_empty() {
            outcome.source_url.clone()
        } else {
            outcome.item.url.clone()
        };
        engine_jobs::page_event(
            &state,
            &job_id,
            account_id.clone(),
            &url,
            outcome.item.status_code,
            outcome.duration_ms,
            error.as_deref(),
        );
        if outcome.index % 100 == 0 {
            info!(job_id = %job_id, done = seq, "Batch scrape progress");
        }
    }

    if engine_jobs::is_stopped(&state, &job_id) {
        info!(job_id = %job_id, done = seq, "Batch scrape stopped");
        return;
    }
    if seq == 0 {
        warn!(job_id = %job_id, "Batch scrape processed no URL");
    }
    engine_jobs::complete_job(&state, &job_id, succeeded, failed, started);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{results_page, test_support::test_state};
    use scrapix_core::JobStatus;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn html(i: usize) -> String {
        format!(
            "<html><head><title>Page {i}</title></head><body><main><h1>Page {i}</h1>\
             <p>Body of page {i}.</p></main></body></html>"
        )
    }

    async fn serve(delay: Duration) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("^/ok/[0-9]+$"))
            .respond_with(move |req: &wiremock::Request| {
                let i: usize = req
                    .url
                    .path()
                    .rsplit('/')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                ResponseTemplate::new(200)
                    .set_body_raw(html(i), "text/html")
                    .set_delay(delay)
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("^/missing/.*"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;
        server
    }

    /// `http://localhost:<port>`: `/scrape` refuses raw IPs.
    fn base(server: &MockServer) -> String {
        server.uri().replace("127.0.0.1", "localhost")
    }

    async fn wait_terminal(state: &AppState, job_id: &str) -> scrapix_core::JobState {
        for _ in 0..1200 {
            let job = state.get_job(job_id).unwrap();
            if crate::is_terminal(&job.status) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job {job_id} did not finish");
    }

    async fn all_results(state: &AppState, job_id: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let job = state.get_job(job_id).unwrap();
            let page = results_page(state, &job, Some(7), cursor.as_deref())
                .await
                .unwrap();
            assert_eq!(page.job_type, JobKind::BatchScrape);
            let empty = page.data.is_empty();
            out.extend(page.data);
            match page.next {
                // A running job always hands back a cursor: stop polling
                // once a page comes back empty.
                Some(next) if !(empty && !crate::is_terminal(&job.status)) => cursor = Some(next),
                _ => return out,
            }
        }
    }

    #[test]
    fn body_validation() {
        assert!(parse_batch_body(serde_json::json!([])).is_err());
        assert!(parse_batch_body(serde_json::json!({})).is_err());
        assert!(parse_batch_body(serde_json::json!({ "urls": [] })).is_err());
        assert!(parse_batch_body(serde_json::json!({ "urls": "https://a.test" })).is_err());
        let too_many: Vec<String> = (0..=MAX_BATCH_URLS)
            .map(|i| format!("https://a.test/{i}"))
            .collect();
        assert!(parse_batch_body(serde_json::json!({ "urls": too_many })).is_err());
        assert!(parse_batch_body(
            serde_json::json!({ "urls": ["https://a.test"], "formats": ["nope"] })
        )
        .is_err());
        assert!(parse_batch_body(
            serde_json::json!({ "urls": ["https://a.test"], "concurrency": 0 })
        )
        .is_err());

        for bad in ["nope", "ftp://a.test/x", "https://"] {
            let err = parse_batch_body(serde_json::json!({ "urls": ["https://a.test", bad] }))
                .err()
                .unwrap_or_else(|| panic!("{bad} must be refused"));
            assert_eq!(err.code, "validation_error");
            assert!(err.error.starts_with("urls[1]: "), "{}", err.error);
        }

        let parsed = parse_batch_body(serde_json::json!({
            "urls": [" https://a.test/1 ", "", "https://a.test/2"],
            "concurrency": 500,
            "formats": ["markdown", "links"],
            "headers": { "Authorization": "secret" },
            "url": "ignored"
        }))
        .unwrap();
        assert_eq!(parsed.urls, vec!["https://a.test/1", "https://a.test/2"]);
        assert_eq!(parsed.concurrency, MAX_BATCH_CONCURRENCY);
        assert!(!parsed.options.contains_key("url"));
        assert_eq!(parsed.sample.url, "https://a.test/1");
        let redacted = redacted_options(&parsed.options);
        assert_eq!(redacted["headers"]["Authorization"], "***");
    }

    #[tokio::test]
    async fn ai_without_a_provider_refuses_the_batch() {
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state(&bus);
        let batch = parse_batch_body(serde_json::json!({
            "urls": ["https://a.test/1"],
            "ai": { "summary": true }
        }))
        .unwrap();
        let err = start_batch(&state, &None, batch).await.err().unwrap();
        assert_eq!(err.code, "service_unavailable");
        assert!(state.crawl.jobs.read().is_empty(), "no job created");
    }

    /// SCR-74 acceptance: a 100-URL batch against a local server, with
    /// failing URLs (404s, an unreachable host, an invalid URL), captured as
    /// items without failing the batch; results page consistently.
    #[tokio::test]
    async fn hundred_url_batch_with_failures() {
        let server = serve(Duration::from_millis(5)).await;
        let base = base(&server);
        let mut urls: Vec<String> = (0..90).map(|i| format!("{base}/ok/{i}")).collect();
        urls.extend((0..8).map(|i| format!("{base}/missing/{i}")));
        urls.push("http://unreachable.invalid/".to_string());
        // A well-formed URL `/scrape` refuses (raw IP): a per-item error.
        urls.push("https://10.0.0.1/".to_string());
        assert_eq!(urls.len(), 100);

        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state(&bus);
        let mut events = state.crawl.event_tx.subscribe();
        let batch = parse_batch_body(serde_json::json!({
            "urls": urls,
            "formats": ["markdown", "metadata"],
            "concurrency": 8
        }))
        .unwrap();
        let response = start_batch(&state, &None, batch).await.unwrap();
        assert_eq!(response.urls_count, 100);

        let job = wait_terminal(&state, &response.job_id).await;
        assert_eq!(job.status, JobStatus::Completed);
        assert_eq!(JobKind::of(&job), JobKind::BatchScrape);
        assert_eq!(job.pages_crawled, 90);
        assert_eq!(job.errors, 10);
        assert_eq!(job.max_pages, Some(100));
        // Engine-run jobs are never billed per crawl page by bill_job.
        assert_eq!(
            state
                .diagnostics
                .job_bills_requested
                .load(Ordering::Relaxed),
            0
        );

        let results = all_results(&state, &response.job_id).await;
        assert_eq!(results.len(), 100, "every URL has exactly one result");
        let mut indexes: Vec<u64> = results
            .iter()
            .map(|r| r["index"].as_u64().unwrap())
            .collect();
        indexes.sort_unstable();
        assert_eq!(indexes, (0..100).collect::<Vec<u64>>());

        let ok: Vec<&Value> = results.iter().filter(|r| r["success"] == true).collect();
        assert_eq!(ok.len(), 90);
        for r in &ok {
            let i = r["index"].as_u64().unwrap();
            assert!(r["markdown"]
                .as_str()
                .unwrap()
                .contains(&format!("Page {i}")));
            assert_eq!(r["metadata"]["title"], format!("Page {i}"));
            assert!(r.get("error").is_none());
        }
        let codes: Vec<&str> = results
            .iter()
            .filter(|r| r["success"] == false)
            .map(|r| r["error"]["code"].as_str().unwrap())
            .collect();
        assert_eq!(codes.iter().filter(|c| **c == "http_error").count(), 8);
        assert!(codes.contains(&"fetch_error"), "{codes:?}");
        assert!(codes.contains(&"validation_error"), "{codes:?}");
        let missing = results.iter().find(|r| r["status_code"] == 404).unwrap();
        assert_eq!(missing["error"]["message"], "HTTP 404");

        // Lifecycle events were broadcast (SSE / WebSocket).
        let (mut started, mut crawled, mut failed, mut completed) = (0, 0, 0, 0);
        while let Ok((id, event)) = events.try_recv() {
            assert_eq!(id, response.job_id);
            match event {
                scrapix_queue::CrawlEvent::JobStarted { .. } => started += 1,
                scrapix_queue::CrawlEvent::PageCrawled { .. } => crawled += 1,
                scrapix_queue::CrawlEvent::PageFailed { .. } => failed += 1,
                scrapix_queue::CrawlEvent::JobCompleted { .. } => completed += 1,
                _ => {}
            }
        }
        assert_eq!((started, crawled, failed, completed), (1, 90, 10, 1));
    }

    #[tokio::test]
    async fn cancel_stops_remaining_urls_and_keeps_results() {
        let server = serve(Duration::from_millis(100)).await;
        let base = base(&server);
        let urls: Vec<String> = (0..60).map(|i| format!("{base}/ok/{i}")).collect();
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state(&bus);
        let batch = parse_batch_body(serde_json::json!({
            "urls": urls, "formats": ["markdown"], "concurrency": 2
        }))
        .unwrap();
        let response = start_batch(&state, &None, batch).await.unwrap();

        // Let a few URLs finish, then cancel.
        for _ in 0..200 {
            if state.get_job(&response.job_id).unwrap().pages_crawled >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let cancelled = state.cancel(&response.job_id).unwrap();
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        // A cancelled engine job is not billed through bill_job.
        assert_eq!(
            state
                .diagnostics
                .job_bills_requested
                .load(Ordering::Relaxed),
            0
        );

        // In-flight URLs finish; nothing new starts.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let stored = all_results(&state, &response.job_id).await;
        assert!(stored.len() >= 3, "results before the cancel are kept");
        assert!(stored.len() < 60, "remaining URLs were skipped");
        let job = state.get_job(&response.job_id).unwrap();
        assert_eq!(job.status, JobStatus::Cancelled);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            all_results(&state, &response.job_id).await.len(),
            stored.len()
        );
    }

    #[tokio::test]
    async fn pause_holds_and_resume_continues() {
        let server = serve(Duration::from_millis(50)).await;
        let base = base(&server);
        let urls: Vec<String> = (0..20).map(|i| format!("{base}/ok/{i}")).collect();
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state(&bus);
        let batch = parse_batch_body(serde_json::json!({
            "urls": urls, "formats": ["markdown"], "concurrency": 1
        }))
        .unwrap();
        let response = start_batch(&state, &None, batch).await.unwrap();
        state.pause(&response.job_id).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let held = state.get_job(&response.job_id).unwrap().pages_crawled;
        assert!(held <= 2, "at most the in-flight URL finished while paused");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(state.get_job(&response.job_id).unwrap().pages_crawled, held);

        state.resume(&response.job_id).unwrap();
        let job = wait_terminal(&state, &response.job_id).await;
        assert_eq!(job.status, JobStatus::Completed);
        assert_eq!(job.pages_crawled, 20);
    }

    #[tokio::test]
    async fn completion_loop_ignores_engine_jobs() {
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state(&bus);
        let job = engine_jobs::start_job(
            &state,
            &None,
            JobKind::BatchScrape,
            vec!["https://a.test/".into()],
            serde_json::json!({}),
            Vec::new(),
        )
        .await;
        let later = std::time::Instant::now() + Duration::from_secs(3600);
        assert!(state.completion_decisions(later).is_empty());
        assert_eq!(
            state.get_job(&job.job_id).unwrap().status,
            JobStatus::Running
        );
    }
}
