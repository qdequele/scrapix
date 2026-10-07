//! The 14 pipes: each runs its `scrapix_storage::clickhouse` query and maps
//! the rows to the Rails field names and formats
//! (meilisearch/lab `saas/app/controllers/analytics_controller.rb`).

use scrapix_storage::clickhouse::{ClickHouseError, DomainStats};
use serde_json::{json, Value};
use tracing::error;

use super::{iso_date, iso_time, metas, rate, Pipe};
use crate::ApiError;

pub(super) const DOMAIN_META: &[(&str, &str)] = &[
    ("domain", "String"),
    ("total_requests", "UInt64"),
    ("successful_requests", "UInt64"),
    ("failed_requests", "UInt64"),
    ("success_rate", "Float64"),
    ("avg_duration_ms", "Float64"),
    ("total_bytes", "UInt64"),
];

const USAGE_META_TAIL: &[(&str, &str)] = &[
    ("total_bytes", "UInt64"),
    ("avg_duration_ms", "Float64"),
    ("unique_domains", "UInt64"),
    ("js_renders", "UInt64"),
    ("ai_prompt_tokens", "UInt64"),
    ("ai_completion_tokens", "UInt64"),
];

const DAILY_USAGE_META: &[(&str, &str)] = &[
    ("requests", "UInt64"),
    ("bytes", "UInt64"),
    ("js_renders", "UInt64"),
    ("ai_prompt_tokens", "UInt64"),
    ("ai_completion_tokens", "UInt64"),
];

pub(super) fn domain_row(s: &DomainStats) -> Value {
    json!({
        "domain": s.domain,
        "total_requests": s.total_requests,
        "successful_requests": s.successful_requests,
        "failed_requests": s.failed_requests,
        "success_rate": rate(s.successful_requests, s.total_requests),
        "avg_duration_ms": s.avg_duration_ms,
        "total_bytes": s.total_bytes,
    })
}

/// A failed ClickHouse query: `500 {"error": <message>, "code": "QUERY_ERROR"}`.
fn query_error(pipe: &'static str) -> impl FnOnce(ClickHouseError) -> ApiError {
    move |e| {
        error!(pipe, error = %e, "analytics query failed");
        ApiError::new(e.to_string(), "QUERY_ERROR")
    }
}

/// Top domains by request count.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/top_domains.json",
    tag = "analytics",
    params(
        ("hours" = Option<u32>, Query, description = "Look-back window in hours (default 24)"),
        ("limit" = Option<u32>, Query, description = "Maximum rows (default 20)"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn top_domains(p: Pipe<'_>) -> Result<Value, ApiError> {
    let rows =
        p.ch.get_top_domains(p.hours(), p.int("limit", 20), p.account)
            .await
            .map_err(query_error("top_domains"))?;
    Ok(p.respond(metas(DOMAIN_META), rows.iter().map(domain_row).collect()))
}

/// Statistics for one domain (an all-zeros row when it has no data).
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/domain_stats.json",
    tag = "analytics",
    params(
        ("domain" = String, Query, description = "Domain"),
        ("hours" = Option<u32>, Query, description = "Look-back window in hours (default 24)"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 400, description = "`domain` is missing", body = ApiError),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn domain_stats(p: Pipe<'_>) -> Result<Value, ApiError> {
    let domain = p.required("domain")?;
    let row =
        p.ch.get_domain_stats(domain, p.hours(), p.account)
            .await
            .map_err(query_error("domain_stats"))?;
    Ok(p.respond(metas(DOMAIN_META), vec![domain_row(&row)]))
}

/// Hourly crawl statistics.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/hourly_stats.json",
    tag = "analytics",
    params(
        ("hours" = Option<u32>, Query, description = "Look-back window in hours (default 24)"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn hourly_stats(p: Pipe<'_>) -> Result<Value, ApiError> {
    let rows =
        p.ch.get_hourly_stats(p.hours(), p.account)
            .await
            .map_err(query_error("hourly_stats"))?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "hour": iso_time(r.hour),
                "requests": r.requests,
                "successes": r.successes,
                "failures": r.failures,
                "success_rate": rate(r.successes, r.requests),
                "avg_duration_ms": r.avg_duration_ms,
                "total_bytes": r.total_bytes,
            })
        })
        .collect();
    let meta = metas(&[
        ("hour", "DateTime"),
        ("requests", "UInt64"),
        ("successes", "UInt64"),
        ("failures", "UInt64"),
        ("success_rate", "Float64"),
        ("avg_duration_ms", "Float64"),
        ("total_bytes", "UInt64"),
    ]);
    Ok(p.respond(meta, data))
}

/// Daily crawl statistics.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/daily_stats.json",
    tag = "analytics",
    params(
        ("days" = Option<u32>, Query, description = "Look-back window in days (default 30)"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn daily_stats(p: Pipe<'_>) -> Result<Value, ApiError> {
    let rows =
        p.ch.get_daily_stats(p.days(), p.account)
            .await
            .map_err(query_error("daily_stats"))?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "date": iso_date(r.date),
                "requests": r.requests,
                "successes": r.successes,
                "failures": r.failures,
                "success_rate": rate(r.successes, r.requests),
                "avg_duration_ms": r.avg_duration_ms,
                "total_bytes": r.total_bytes,
            })
        })
        .collect();
    let meta = metas(&[
        ("date", "Date"),
        ("requests", "UInt64"),
        ("successes", "UInt64"),
        ("failures", "UInt64"),
        ("success_rate", "Float64"),
        ("avg_duration_ms", "Float64"),
        ("total_bytes", "UInt64"),
    ]);
    Ok(p.respond(meta, data))
}

/// Error breakdown by status code.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/error_distribution.json",
    tag = "analytics",
    params(
        ("hours" = Option<u32>, Query, description = "Look-back window in hours (default 24)"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn error_distribution(p: Pipe<'_>) -> Result<Value, ApiError> {
    let rows =
        p.ch.get_error_distribution(p.hours(), p.account)
            .await
            .map_err(query_error("error_distribution"))?;
    let total: u64 = rows.iter().map(|(_, count)| count).sum();
    let data = rows
        .iter()
        .map(|(status_code, count)| {
            json!({
                "status_code": status_code,
                "count": count,
                "percentage": rate(*count, total),
            })
        })
        .collect();
    let meta = metas(&[
        ("status_code", "UInt16"),
        ("count", "UInt64"),
        ("percentage", "Float64"),
    ]);
    Ok(p.respond(meta, data))
}

/// Statistics for one job (zero rows when it has no data).
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/job_stats.json",
    tag = "analytics",
    params(
        ("job_id" = String, Query, description = "Job ID"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 400, description = "`job_id` is missing", body = ApiError),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn job_stats(p: Pipe<'_>) -> Result<Value, ApiError> {
    let job_id = p.required("job_id")?;
    let row =
        p.ch.get_job_stats(job_id, p.account)
            .await
            .map_err(query_error("job_stats"))?;
    let data = row
        .iter()
        .map(|r| {
            json!({
                "job_id": r.job_id,
                "total_requests": r.total_requests,
                "successful_requests": r.successful_requests,
                "failed_requests": r.failed_requests,
                "success_rate": rate(r.successful_requests, r.total_requests),
                "total_bytes": r.total_bytes,
                "avg_duration_ms": r.avg_duration_ms,
                "unique_domains": r.unique_domains,
                "started_at": iso_time(r.started_at),
                "last_activity_at": iso_time(r.last_activity_at),
                "duration_seconds": (r.last_activity_at - r.started_at).whole_seconds(),
            })
        })
        .collect();
    let meta = metas(&[
        ("job_id", "String"),
        ("total_requests", "UInt64"),
        ("successful_requests", "UInt64"),
        ("failed_requests", "UInt64"),
        ("success_rate", "Float64"),
        ("total_bytes", "UInt64"),
        ("avg_duration_ms", "Float64"),
        ("unique_domains", "UInt64"),
        ("started_at", "DateTime"),
        ("last_activity_at", "DateTime"),
        ("duration_seconds", "Int64"),
    ]);
    Ok(p.respond(meta, data))
}

/// Key performance indicators, aggregated from `top_domains` (limit 10 000).
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/kpis.json",
    tag = "analytics",
    params(
        ("hours" = Option<u32>, Query, description = "Look-back window in hours (default 24)"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn kpis(p: Pipe<'_>) -> Result<Value, ApiError> {
    let domains =
        p.ch.get_top_domains(p.hours(), 10_000, p.account)
            .await
            .map_err(query_error("kpis"))?;
    let total_crawls: u64 = domains.iter().map(|d| d.total_requests).sum();
    let total_successes: u64 = domains.iter().map(|d| d.successful_requests).sum();
    let total_bytes: u64 = domains.iter().map(|d| d.total_bytes).sum();
    let errors_count: u64 = domains.iter().map(|d| d.failed_requests).sum();
    let total_response_time: f64 = domains
        .iter()
        .map(|d| d.avg_duration_ms * d.total_requests as f64)
        .sum();
    let avg_duration_ms = if total_crawls > 0 {
        total_response_time / total_crawls as f64
    } else {
        0.0
    };
    let data = vec![json!({
        "total_crawls": total_crawls,
        "total_bytes": total_bytes,
        "unique_domains": domains.len(),
        "success_rate": rate(total_successes, total_crawls),
        "avg_duration_ms": avg_duration_ms,
        "errors_count": errors_count,
    })];
    let meta = metas(&[
        ("total_crawls", "UInt64"),
        ("total_bytes", "UInt64"),
        ("unique_domains", "UInt64"),
        ("success_rate", "Float64"),
        ("avg_duration_ms", "Float64"),
        ("errors_count", "UInt64"),
    ]);
    Ok(p.respond(meta, data))
}

/// AI/LLM token usage per model.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/ai_usage.json",
    tag = "analytics",
    params(
        ("hours" = Option<u32>, Query, description = "Look-back window in hours (default 24)"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn ai_usage(p: Pipe<'_>) -> Result<Value, ApiError> {
    let rows =
        p.ch.get_ai_usage_stats(p.hours(), p.account)
            .await
            .map_err(query_error("ai_usage"))?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "model": r.model,
                "total_calls": r.total_calls,
                "total_prompt_tokens": r.total_prompt_tokens,
                "total_completion_tokens": r.total_completion_tokens,
                "total_tokens": r.total_tokens,
                "avg_duration_ms": r.avg_duration_ms,
            })
        })
        .collect();
    let meta = metas(&[
        ("model", "String"),
        ("total_calls", "UInt64"),
        ("total_prompt_tokens", "UInt64"),
        ("total_completion_tokens", "UInt64"),
        ("total_tokens", "UInt64"),
        ("avg_duration_ms", "Float64"),
    ]);
    Ok(p.respond(meta, data))
}

/// Job lifecycle events, newest first. `meta` declares 3 of the 12 fields
/// (preserved for compatibility).
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/job_timeline.json",
    tag = "analytics",
    params(
        ("job_id" = String, Query, description = "Job ID"),
        ("limit" = Option<u32>, Query, description = "Maximum rows (default 100)"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 400, description = "`job_id` is missing", body = ApiError),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn job_timeline(p: Pipe<'_>) -> Result<Value, ApiError> {
    let job_id = p.required("job_id")?;
    let rows =
        p.ch.get_job_events(job_id, p.int("limit", 100), p.account)
            .await
            .map_err(query_error("job_timeline"))?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "event_type": r.event_type,
                "job_id": r.job_id,
                "account_id": r.account_id,
                "operation": r.operation,
                "index_uid": r.index_uid,
                "pages_crawled": r.pages_crawled,
                "documents_indexed": r.documents_indexed,
                "errors": r.errors,
                "bytes_downloaded": r.bytes_downloaded,
                "duration_secs": r.duration_secs,
                "error": r.error,
                "timestamp": iso_time(r.timestamp),
            })
        })
        .collect();
    let meta = metas(&[
        ("event_type", "String"),
        ("job_id", "String"),
        ("timestamp", "DateTime"),
    ]);
    Ok(p.respond(meta, data))
}

/// Event type counts for one job.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/job_event_summary.json",
    tag = "analytics",
    params(
        ("job_id" = String, Query, description = "Job ID"),
        ("account_id" = Option<String>, Query, description = "Account filter; hosted callers may only name their own account")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 400, description = "`job_id` is missing", body = ApiError),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn job_event_summary(p: Pipe<'_>) -> Result<Value, ApiError> {
    let job_id = p.required("job_id")?;
    let rows =
        p.ch.get_job_event_summary(job_id, p.account)
            .await
            .map_err(query_error("job_event_summary"))?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "event_type": r.event_type,
                "event_count": r.event_count,
                "first_seen": iso_time(r.first_seen),
                "last_seen": iso_time(r.last_seen),
            })
        })
        .collect();
    let meta = metas(&[
        ("event_type", "String"),
        ("event_count", "UInt64"),
        ("first_seen", "DateTime"),
        ("last_seen", "DateTime"),
    ]);
    Ok(p.respond(meta, data))
}

/// Usage summary for one account (an all-zeros row when it has no data).
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/account_usage.json",
    tag = "analytics",
    params(
        ("account_id" = Option<String>, Query, description = "Defaults to the caller's account; required with the standalone admin key"),
        ("hours" = Option<u32>, Query, description = "Look-back window in hours (default 24)")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 400, description = "No account: the admin key must pass `account_id`", body = ApiError),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn account_usage(p: Pipe<'_>) -> Result<Value, ApiError> {
    let account = p.account()?;
    let r =
        p.ch.get_account_usage(account, p.hours())
            .await
            .map_err(query_error("account_usage"))?;
    let data = vec![json!({
        "account_id": r.account_id,
        "total_requests": r.total_requests,
        "successful_requests": r.successful_requests,
        "failed_requests": r.failed_requests,
        "total_bytes": r.total_bytes,
        "avg_duration_ms": r.avg_duration_ms,
        "unique_domains": r.unique_domains,
        "js_renders": r.js_renders,
        "ai_prompt_tokens": r.ai_prompt_tokens,
        "ai_completion_tokens": r.ai_completion_tokens,
    })];
    let mut meta = metas(&[
        ("account_id", "String"),
        ("total_requests", "UInt64"),
        ("successful_requests", "UInt64"),
        ("failed_requests", "UInt64"),
    ]);
    meta.extend(metas(USAGE_META_TAIL));
    Ok(p.respond(meta, data))
}

/// Daily usage for one account.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/account_daily_usage.json",
    tag = "analytics",
    params(
        ("account_id" = Option<String>, Query, description = "Defaults to the caller's account; required with the standalone admin key"),
        ("days" = Option<u32>, Query, description = "Look-back window in days (default 30)")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 400, description = "No account: the admin key must pass `account_id`", body = ApiError),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn account_daily_usage(p: Pipe<'_>) -> Result<Value, ApiError> {
    let account = p.account()?;
    let rows =
        p.ch.get_account_daily_usage(account, p.days())
            .await
            .map_err(query_error("account_daily_usage"))?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "date": iso_date(r.date),
                "requests": r.requests,
                "bytes": r.bytes,
                "js_renders": r.js_renders,
                "ai_prompt_tokens": r.ai_prompt_tokens,
                "ai_completion_tokens": r.ai_completion_tokens,
            })
        })
        .collect();
    let mut meta = metas(&[("date", "Date")]);
    meta.extend(metas(DAILY_USAGE_META));
    Ok(p.respond(meta, data))
}

/// Daily usage per operation (scrape/map/crawl) for one account.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/account_daily_usage_by_operation.json",
    tag = "analytics",
    params(
        ("account_id" = Option<String>, Query, description = "Defaults to the caller's account; required with the standalone admin key"),
        ("days" = Option<u32>, Query, description = "Look-back window in days (default 30)")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 400, description = "No account: the admin key must pass `account_id`", body = ApiError),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn account_daily_usage_by_operation(p: Pipe<'_>) -> Result<Value, ApiError> {
    let account = p.account()?;
    let rows =
        p.ch.get_account_daily_usage_by_operation(account, p.days())
            .await
            .map_err(query_error("account_daily_usage_by_operation"))?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "date": iso_date(r.date),
                "operation": r.operation,
                "requests": r.requests,
                "bytes": r.bytes,
                "js_renders": r.js_renders,
                "ai_prompt_tokens": r.ai_prompt_tokens,
                "ai_completion_tokens": r.ai_completion_tokens,
            })
        })
        .collect();
    let mut meta = metas(&[("date", "Date"), ("operation", "String")]);
    meta.extend(metas(DAILY_USAGE_META));
    Ok(p.respond(meta, data))
}

/// Usage per API key for one account.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes/api_key_usage.json",
    tag = "analytics",
    params(
        ("account_id" = Option<String>, Query, description = "Defaults to the caller's account; required with the standalone admin key"),
        ("hours" = Option<u32>, Query, description = "Look-back window in hours (default 24)")
    ),
    responses(
        (status = 200, description = "Tinybird envelope: meta, data, rows, statistics"),
        (status = 400, description = "No account: the admin key must pass `account_id`", body = ApiError),
        (status = 404, description = "ClickHouse is not configured (code `analytics_unavailable`), or `account_id` names another account (code `not_found`)", body = crate::ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn api_key_usage(p: Pipe<'_>) -> Result<Value, ApiError> {
    let account = p.account()?;
    let rows =
        p.ch.get_api_key_usage(account, p.hours())
            .await
            .map_err(query_error("api_key_usage"))?;
    let data = rows
        .iter()
        .map(|r| {
            json!({
                "api_key_id": r.api_key_id,
                "total_requests": r.total_requests,
                "successful_requests": r.successful_requests,
                "failed_requests": r.failed_requests,
                "total_bytes": r.total_bytes,
                "avg_duration_ms": r.avg_duration_ms,
                "unique_domains": r.unique_domains,
                "js_renders": r.js_renders,
                "ai_prompt_tokens": r.ai_prompt_tokens,
                "ai_completion_tokens": r.ai_completion_tokens,
            })
        })
        .collect();
    let mut meta = metas(&[
        ("api_key_id", "String"),
        ("total_requests", "UInt64"),
        ("successful_requests", "UInt64"),
        ("failed_requests", "UInt64"),
    ]);
    meta.extend(metas(USAGE_META_TAIL));
    Ok(p.respond(meta, data))
}
