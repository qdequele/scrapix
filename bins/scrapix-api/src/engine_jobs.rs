//! Lifecycle of the jobs the API runs itself (batch scrape, extract), as
//! opposed to crawl jobs driven by the Kafka pipeline.
//!
//! These jobs reuse the crawl job machinery end to end: a `JobState` in the
//! in-memory map and the `jobs` table (`/job/{id}/status`, `/jobs`), their
//! progress as `CrawlEvent`s applied with `process_event` and broadcast
//! (counters, SSE/WebSocket, ClickHouse page events, webhooks), and
//! `DELETE /job/{id}` / pause / resume through the usual status
//! transitions. What differs:
//!
//! - no pipeline: nothing is published to Kafka, the job has no work
//!   accounting, and the completion loop ignores it (see
//!   [`JobKind::is_pipeline`]) — the runner task finalizes it;
//! - billing is per URL, charged by `perform_scrape` as each page is
//!   scraped (plus AI calls for extract), never by `bill_job`;
//! - results are stored by the engine (`results::store_page`).
//!
//! A runner checks the job's status before each URL: it stops on a
//! terminal status (cancel) and waits while the job is paused. A job that
//! was running when the API stopped cannot be resumed (its scrape options
//! are only persisted redacted), so startup recovery marks it failed; the
//! results it stored stay readable.

use std::sync::Arc;
use std::time::Duration;

use scrapix_core::{config::WebhookConfig, JobState, JobStatus};
use scrapix_queue::CrawlEvent;
use serde_json::Value;
use tracing::{info, warn};

use crate::job_kind::{JobKind, JOB_TYPE_KEY};
use crate::{billing, is_terminal, webhooks, AccountContext, ApiError, AppState};

/// Error message of a job interrupted by an API restart.
pub(crate) const INTERRUPTED_MESSAGE: &str =
    "Interrupted: the API restarted while this job was running (results stored so far are kept)";

/// An owned copy of the caller's account context, for a background runner.
pub(crate) fn clone_account_ctx(ctx: &Option<AccountContext>) -> Option<AccountContext> {
    ctx.as_ref().map(|c| AccountContext {
        account_id: c.account_id.clone(),
        api_key_id: c.api_key_id.clone(),
        tier: c.tier.clone(),
        user_role: c.user_role.clone(),
    })
}

/// Validate and normalize webhook subscriptions (same rules as crawl jobs).
pub(crate) fn validate_webhooks(hooks: &mut [WebhookConfig]) -> Result<(), ApiError> {
    for hook in hooks.iter_mut() {
        hook.timeout_ms = webhooks::clamp_timeout_ms(hook.timeout_ms);
        webhooks::validate_webhook_config(hook)
            .map_err(|msg| ApiError::new(format!("webhooks: {msg}"), "validation_error"))?;
    }
    Ok(())
}

/// Credit pre-check and per-tier concurrent job limit, as for `/crawl`.
pub(crate) async fn preflight(
    state: &AppState,
    account_ctx: &Option<AccountContext>,
    required_credits: i64,
) -> Result<(), ApiError> {
    let (Some(pool), Some(ctx)) = (&state.saas_pool, account_ctx) else {
        return Ok(());
    };
    billing::check_credits(pool, &ctx.account_id, required_credits.max(1)).await?;
    let tier: scrapix_core::BillingTier = ctx.tier.parse().unwrap_or_default();
    let max_concurrent = tier.max_concurrent_jobs() as i64;
    let active_count = state.active_job_count(&ctx.account_id).await;
    if active_count >= max_concurrent {
        return Err(ApiError::new(
            format!(
                "Maximum concurrent jobs reached ({}/{}). Upgrade your plan for more.",
                active_count, max_concurrent
            ),
            "quota_exceeded",
        ));
    }
    Ok(())
}

/// Create, persist and start an engine-run job. `config` is the (already
/// redacted) job description shown as the job's `config`; the job kind is
/// added to it. `webhooks` are the real subscriptions (kept in memory only).
pub(crate) async fn start_job(
    state: &Arc<AppState>,
    account_ctx: &Option<AccountContext>,
    kind: JobKind,
    start_urls: Vec<String>,
    mut config: Value,
    webhooks: Vec<WebhookConfig>,
) -> JobState {
    let job_id = uuid::Uuid::new_v4().to_string();
    if let Some(obj) = config.as_object_mut() {
        obj.insert(JOB_TYPE_KEY.to_string(), Value::from(kind.as_str()));
    }
    let mut job = match account_ctx {
        Some(ctx) => {
            let mut j = JobState::with_account(&job_id, "", &ctx.account_id);
            j.api_key_id = ctx.api_key_id.clone();
            j
        }
        None => JobState::new(&job_id, ""),
    };
    job.max_pages = Some(start_urls.len() as u64);
    job.start_urls = start_urls;
    job.config = Some(config);
    job.webhooks = webhooks;
    job.start();
    state.insert_job(job.clone());

    // Persist before any result is stored (job_results references jobs).
    let durable = match state.job_store {
        Some(ref s) => s.insert_job(&job).await.is_ok(),
        None => false,
    };
    if !durable {
        state.results.use_memory(&job_id);
    }

    let event = CrawlEvent::JobStarted {
        job_id: job_id.clone(),
        index_uid: String::new(),
        account_id: job.account_id.clone(),
        start_urls: job.start_urls.clone(),
        timestamp: chrono::Utc::now().timestamp_millis(),
    };
    state.process_event(&job_id, &event);
    state.broadcast_event(&job_id, event);
    info!(job_id = %job_id, kind = kind.as_str(), urls = job.start_urls.len(), durable, "Engine job started");
    job
}

/// What a runner should do before its next URL.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Go,
    Stop,
}

/// Wait while the job is paused; `Stop` once it is terminal (cancelled) or
/// gone.
pub(crate) async fn gate(state: &AppState, job_id: &str) -> Gate {
    loop {
        match state.get_job(job_id).map(|j| j.status) {
            Some(JobStatus::Paused) => tokio::time::sleep(Duration::from_millis(250)).await,
            Some(JobStatus::Running) | Some(JobStatus::Pending) => return Gate::Go,
            _ => return Gate::Stop,
        }
    }
}

/// Whether the job was stopped (cancelled, or terminal for another reason).
pub(crate) fn is_stopped(state: &AppState, job_id: &str) -> bool {
    state.get_job(job_id).is_none_or(|j| is_terminal(&j.status))
}

/// Report one processed page: a `PageCrawled` / `PageFailed` event.
pub(crate) fn page_event(
    state: &AppState,
    job_id: &str,
    account_id: Option<String>,
    url: &str,
    status: Option<u16>,
    duration_ms: u64,
    error: Option<&str>,
) {
    let timestamp = chrono::Utc::now().timestamp_millis();
    let event = match error {
        None => CrawlEvent::PageCrawled {
            job_id: job_id.to_string(),
            account_id,
            url: url.to_string(),
            status: status.unwrap_or(200),
            content_length: 0,
            duration_ms,
            timestamp,
            links_published: 0,
            url_message_id: String::new(),
            js_rendered: false,
            sitemap_pending: false,
        },
        Some(error) => CrawlEvent::PageFailed {
            job_id: job_id.to_string(),
            account_id,
            url: url.to_string(),
            error: error.to_string(),
            retry_count: 0,
            timestamp,
            status,
            url_message_id: String::new(),
        },
    };
    state.process_event(job_id, &event);
    state.broadcast_event(job_id, event);
}

/// Complete the job (unless it was cancelled meanwhile).
pub(crate) fn complete_job(
    state: &AppState,
    job_id: &str,
    succeeded: u64,
    failed: u64,
    started: std::time::Instant,
) {
    let account_id = state.get_job(job_id).and_then(|j| j.account_id);
    let event = CrawlEvent::JobCompleted {
        job_id: job_id.to_string(),
        account_id,
        pages_crawled: succeeded,
        documents_indexed: succeeded,
        errors: failed,
        bytes_downloaded: 0,
        duration_secs: started.elapsed().as_secs(),
        timestamp: chrono::Utc::now().timestamp_millis(),
    };
    if state.process_event(job_id, &event).applied {
        state.broadcast_event(job_id, event);
        info!(job_id = %job_id, succeeded, failed, "Engine job completed");
    }
}

/// Fail the job (unless it was cancelled meanwhile).
pub(crate) fn fail_job(state: &AppState, job_id: &str, error: &str) {
    let account_id = state.get_job(job_id).and_then(|j| j.account_id);
    let event = CrawlEvent::JobFailed {
        job_id: job_id.to_string(),
        account_id,
        error: error.to_string(),
        timestamp: chrono::Utc::now().timestamp_millis(),
    };
    if state.process_event(job_id, &event).applied {
        state.broadcast_event(job_id, event);
        warn!(job_id = %job_id, error = %error, "Engine job failed");
    }
}

/// Startup recovery: engine-run jobs recovered as active can't be resumed
/// (their runner died with the previous process). Mark them failed, in
/// the store too, and return the list with their new state.
pub(crate) async fn fail_interrupted(
    store: &dyn crate::job_store::JobStore,
    jobs: Vec<JobState>,
) -> Vec<JobState> {
    let mut out = Vec::with_capacity(jobs.len());
    for mut job in jobs {
        if !JobKind::of(&job).is_pipeline() && !is_terminal(&job.status) {
            job.fail(INTERRUPTED_MESSAGE);
            if store.update_job_full(&job).await.is_ok() {
                info!(job_id = %job.job_id, "Marked interrupted engine job as failed");
            }
        }
        out.push(job);
    }
    out
}
