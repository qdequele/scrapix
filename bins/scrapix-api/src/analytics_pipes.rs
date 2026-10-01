//! Tinybird-style analytics pipes over the engine's ClickHouse
//! (`/analytics/v0/pipes/*`).
//!
//! A port of the Lab's Rails analytics controller, as hardened in PR #10
//! (meilisearch/lab `saas/app/controllers/analytics_controller.rb`), which
//! stays authoritative until the Rails copy is deleted: same SQL (in
//! `scrapix_storage::clickhouse`), parameters, field names and formats. The
//! Lab repo's `contracts/analytics_parity.py` diffs the two. Preserved quirks:
//!
//! - `job_timeline`'s `meta` declares 3 columns while rows carry 12 fields.
//! - `domain_stats` and `account_usage` return one all-zeros row when there is
//!   no data; `job_stats` returns zero rows.
//! - `kpis` aggregates the `top_domains` query (limit 10 000) here.
//! - Timestamps are ISO8601 UTC (`2026-09-30T12:00:00Z`); `daily_stats` and
//!   the daily usage pipes keep `YYYY-MM-DD` dates.
//!
//! Scoping ([`scope`]): a hosted caller only ever sees its own account (an
//! `account_id` naming another account is a 404); the standalone admin key
//! (or auth disabled) sees every account, with `account_id` as a filter. The
//! account-level pipes (`account_usage`, `account_daily_usage`,
//! `account_daily_usage_by_operation`, `api_key_usage`) always need one
//! account, so an admin must pass `account_id` to them. Without ClickHouse
//! every route is a bare 404, as in Rails.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use scrapix_storage::clickhouse::ClickHouseStorage;
use serde::Serialize;
use serde_json::{json, Value};

use crate::auth::AuthenticatedAccount;
use crate::{extract_account_context, AccountContext, ApiError, AppState};

pub(crate) mod pipes;

type Params = HashMap<String, String>;

/// `/pipes` and `/pipes/{name}.json`, nested at `/analytics/v0`.
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/pipes", get(list_pipes))
        .route("/pipes/{file}", get(run_pipe))
}

/// The account a pipe reads: the caller's own (a different `requested`
/// account is `404 Account not found`), or — with no account context
/// (standalone admin key, auth disabled) — `requested` as an optional filter.
/// A blank `requested` counts as absent, like Rails' `presence`.
pub(crate) fn scope(
    ctx: &Option<AccountContext>,
    requested: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let requested = requested.filter(|r| !r.trim().is_empty());
    match ctx {
        Some(ctx) => match requested {
            Some(r) if r != ctx.account_id => Err(ApiError::new("Account not found", "not_found")),
            _ => Ok(Some(ctx.account_id.clone())),
        },
        None => Ok(requested.map(str::to_string)),
    }
}

// ============================================================================
// Envelope
// ============================================================================

fn meta(name: &str, ty: &str) -> Value {
    json!({ "name": name, "type": ty })
}

fn metas(columns: &[(&str, &str)]) -> Vec<Value> {
    columns.iter().map(|(name, ty)| meta(name, ty)).collect()
}

/// `{meta, data, rows, statistics: {elapsed, rows_read, bytes_read}}`.
fn envelope(meta: Vec<Value>, data: Vec<Value>, started: Instant) -> Value {
    let rows = data.len();
    json!({
        "meta": meta,
        "data": data,
        "rows": rows,
        "statistics": {
            "elapsed": started.elapsed().as_secs_f64(),
            "rows_read": rows,
            "bytes_read": 0,
        },
    })
}

/// `part / total` as a percentage, `0.0` when `total` is zero.
fn rate(part: u64, total: u64) -> f64 {
    if total > 0 {
        part as f64 / total as f64 * 100.0
    } else {
        0.0
    }
}

/// ISO8601 UTC with a `Z` suffix, like Ruby's `Time#iso8601` on a UTC time.
fn iso_time(t: time::OffsetDateTime) -> String {
    t.to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// `YYYY-MM-DD`, as ClickHouse renders a `Date`.
fn iso_date(d: time::Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

// ============================================================================
// Catalog (`GET /analytics/v0/pipes`)
// ============================================================================

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ParamInfo {
    name: &'static str,
    #[serde(rename = "type")]
    param_type: &'static str,
    required: bool,
    default: Option<&'static str>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PipeInfo {
    name: &'static str,
    description: &'static str,
    parameters: Vec<ParamInfo>,
    endpoint: &'static str,
}

fn param(name: &'static str, param_type: &'static str, default: Option<&'static str>) -> ParamInfo {
    ParamInfo {
        name,
        param_type,
        required: false,
        default,
    }
}

fn hours() -> ParamInfo {
    param("hours", "integer", Some("24"))
}

fn days() -> ParamInfo {
    param("days", "integer", Some("30"))
}

fn account_id() -> ParamInfo {
    param("account_id", "string", None)
}

fn required(name: &'static str) -> ParamInfo {
    ParamInfo {
        required: true,
        ..param(name, "string", None)
    }
}

fn pipe(
    name: &'static str,
    description: &'static str,
    parameters: Vec<ParamInfo>,
    endpoint: &'static str,
) -> PipeInfo {
    PipeInfo {
        name,
        description,
        parameters,
        endpoint,
    }
}

/// The Rails `PIPES` list, verbatim (`account_id` is never required: it
/// defaults to the caller's account).
fn pipes_catalog() -> Vec<PipeInfo> {
    vec![
        pipe(
            "top_domains",
            "Top domains by request count",
            vec![hours(), param("limit", "integer", Some("20"))],
            "/analytics/v0/pipes/top_domains.json",
        ),
        pipe(
            "domain_stats",
            "Statistics for a specific domain",
            vec![required("domain"), hours()],
            "/analytics/v0/pipes/domain_stats.json",
        ),
        pipe(
            "hourly_stats",
            "Hourly crawl statistics",
            vec![hours()],
            "/analytics/v0/pipes/hourly_stats.json",
        ),
        pipe(
            "daily_stats",
            "Daily crawl statistics",
            vec![days()],
            "/analytics/v0/pipes/daily_stats.json",
        ),
        pipe(
            "error_distribution",
            "Error breakdown by status code",
            vec![hours()],
            "/analytics/v0/pipes/error_distribution.json",
        ),
        pipe(
            "job_stats",
            "Statistics for a specific job",
            vec![required("job_id")],
            "/analytics/v0/pipes/job_stats.json",
        ),
        pipe(
            "kpis",
            "Key performance indicators summary",
            vec![hours()],
            "/analytics/v0/pipes/kpis.json",
        ),
        pipe(
            "ai_usage",
            "AI/LLM token usage per model",
            vec![hours(), account_id()],
            "/analytics/v0/pipes/ai_usage.json",
        ),
        pipe(
            "job_timeline",
            "Job lifecycle events (started, completed, failed)",
            vec![required("job_id"), param("limit", "integer", Some("100"))],
            "/analytics/v0/pipes/job_timeline.json",
        ),
        pipe(
            "job_event_summary",
            "Event type counts for a specific job",
            vec![required("job_id")],
            "/analytics/v0/pipes/job_event_summary.json",
        ),
        pipe(
            "account_usage",
            "Account usage summary (requests, bandwidth, JS renders, AI tokens)",
            vec![account_id(), hours()],
            "/analytics/v0/pipes/account_usage.json",
        ),
        pipe(
            "account_daily_usage",
            "Daily usage breakdown for an account",
            vec![account_id(), days()],
            "/analytics/v0/pipes/account_daily_usage.json",
        ),
        pipe(
            "account_daily_usage_by_operation",
            "Daily usage breakdown per operation (scrape/map/crawl) for an account",
            vec![account_id(), days()],
            "/analytics/v0/pipes/account_daily_usage_by_operation.json",
        ),
        pipe(
            "api_key_usage",
            "Per-API-key usage breakdown for an account",
            vec![account_id(), hours()],
            "/analytics/v0/pipes/api_key_usage.json",
        ),
    ]
}

// ============================================================================
// Handlers
// ============================================================================

/// Rails' `head :not_found` (routes absent, ClickHouse not configured).
fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// ClickHouse, then the account scope — the order of Rails' before_actions.
async fn prepare<'a>(
    state: &'a AppState,
    account_ext: &Option<Extension<AuthenticatedAccount>>,
    params: &Params,
) -> Result<(&'a ClickHouseStorage, Option<String>), Box<Response>> {
    let Some(analytics) = state.analytics_store.as_ref() else {
        return Err(Box::new(not_found()));
    };
    let ctx = extract_account_context(account_ext).await;
    let account = scope(&ctx, params.get("account_id").map(String::as_str))
        .map_err(|e| Box::new(e.into_response()))?;
    Ok((&analytics.storage, account))
}

/// List the available pipes and their parameters.
#[utoipa::path(
    get,
    path = "/analytics/v0/pipes",
    tag = "analytics",
    responses(
        (status = 200, body = Vec<PipeInfo>),
        (status = 404, description = "ClickHouse is not configured, or `account_id` names another account")
    ),
    security(("api_key" = []))
)]
pub(crate) async fn list_pipes(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Query(params): Query<Params>,
) -> Response {
    match prepare(&state, &account_ext, &params).await {
        Ok(_) => Json(pipes_catalog()).into_response(),
        Err(response) => *response,
    }
}

/// `GET /pipes/{name}.json`: resolve ClickHouse and the scope, run the pipe.
async fn run_pipe(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Path(file): Path<String>,
    Query(params): Query<Params>,
) -> Response {
    let Some(name) = file.strip_suffix(".json") else {
        return not_found();
    };
    let (ch, account) = match prepare(&state, &account_ext, &params).await {
        Ok(prepared) => prepared,
        Err(response) => return *response,
    };
    let p = Pipe {
        ch,
        account: account.as_deref(),
        params: &params,
        started: Instant::now(),
    };
    let result = match name {
        "top_domains" => pipes::top_domains(p).await,
        "domain_stats" => pipes::domain_stats(p).await,
        "hourly_stats" => pipes::hourly_stats(p).await,
        "daily_stats" => pipes::daily_stats(p).await,
        "error_distribution" => pipes::error_distribution(p).await,
        "job_stats" => pipes::job_stats(p).await,
        "kpis" => pipes::kpis(p).await,
        "ai_usage" => pipes::ai_usage(p).await,
        "job_timeline" => pipes::job_timeline(p).await,
        "job_event_summary" => pipes::job_event_summary(p).await,
        "account_usage" => pipes::account_usage(p).await,
        "account_daily_usage" => pipes::account_daily_usage(p).await,
        "account_daily_usage_by_operation" => pipes::account_daily_usage_by_operation(p).await,
        "api_key_usage" => pipes::api_key_usage(p).await,
        _ => return not_found(),
    };
    match result {
        Ok(body) => Json(body).into_response(),
        Err(e) => e.into_response(),
    }
}

/// One pipe call: the store, the resolved account, the query string.
pub(crate) struct Pipe<'a> {
    ch: &'a ClickHouseStorage,
    account: Option<&'a str>,
    params: &'a Params,
    started: Instant,
}

impl Pipe<'_> {
    /// An integer parameter; absent or invalid falls back to `default`.
    fn int(&self, name: &str, default: u32) -> u32 {
        self.params
            .get(name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    fn hours(&self) -> u32 {
        self.int("hours", 24)
    }

    fn days(&self) -> u32 {
        self.int("days", 30)
    }

    /// A required, non-blank string parameter (Rails' `params.require`).
    fn required(&self, name: &str) -> Result<&str, ApiError> {
        self.params
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| missing(name))
    }

    /// The one account the account-level pipes read.
    fn account(&self) -> Result<&str, ApiError> {
        self.account.ok_or_else(|| missing("account_id"))
    }

    fn respond(&self, meta: Vec<Value>, data: Vec<Value>) -> Value {
        envelope(meta, data, self.started)
    }
}

fn missing(name: &str) -> ApiError {
    ApiError::new(
        format!("param is missing or the value is empty: {name}"),
        "bad_request",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCT: &str = "11111111-1111-1111-1111-111111111111";

    fn ctx(account: &str) -> Option<crate::AccountContext> {
        Some(crate::AccountContext {
            account_id: account.into(),
            api_key_id: None,
            tier: "free".into(),
            user_role: None,
        })
    }

    #[test]
    fn tenant_is_scoped_to_its_account() {
        assert_eq!(scope(&ctx(ACCT), None).unwrap(), Some(ACCT.to_string()));
        assert_eq!(
            scope(&ctx(ACCT), Some(ACCT)).unwrap(),
            Some(ACCT.to_string())
        );
        assert!(scope(&ctx(ACCT), Some("22222222-2222-2222-2222-222222222222")).is_err());
    }

    #[test]
    fn admin_sees_all_or_filters() {
        assert_eq!(scope(&None, None).unwrap(), None);
        assert_eq!(scope(&None, Some(ACCT)).unwrap(), Some(ACCT.to_string()));
    }

    #[test]
    fn another_account_is_rails_not_found() {
        let err = scope(&ctx(ACCT), Some("other")).unwrap_err();
        assert_eq!(
            (err.error.as_str(), err.code.as_str()),
            ("Account not found", "not_found")
        );
    }

    #[test]
    fn blank_account_id_is_absent() {
        assert_eq!(scope(&ctx(ACCT), Some("")).unwrap(), Some(ACCT.to_string()));
        assert_eq!(scope(&None, Some("  ")).unwrap(), None);
    }

    #[test]
    fn envelope_shape() {
        let v = envelope(
            vec![meta("domain", "String")],
            vec![serde_json::json!({"domain": "a"})],
            std::time::Instant::now(),
        );
        assert_eq!(v["rows"], 1);
        assert_eq!(v["meta"][0]["name"], "domain");
        assert_eq!(v["statistics"]["rows_read"], 1);
        assert_eq!(v["statistics"]["bytes_read"], 0);
        assert!(v["statistics"]["elapsed"].is_number());
    }

    #[test]
    fn rate_matches_rails() {
        assert_eq!(rate(1, 4), 25.0);
        assert_eq!(rate(0, 0), 0.0);
    }

    #[test]
    fn times_are_iso8601_utc_and_dates_plain() {
        let t = time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        assert_eq!(iso_time(t), "2026-09-21T14:13:20Z");
        let d = time::Date::from_calendar_date(2026, time::Month::March, 5).unwrap();
        assert_eq!(iso_date(d), "2026-03-05");
    }

    #[test]
    fn domain_row_carries_the_rails_fields() {
        let row = pipes::domain_row(&scrapix_storage::clickhouse::DomainStats {
            domain: "example.com".into(),
            total_requests: 4,
            successful_requests: 3,
            failed_requests: 1,
            avg_duration_ms: 12.5,
            total_bytes: 100,
        });
        let fields: Vec<_> = pipes::DOMAIN_META.iter().map(|(name, _)| *name).collect();
        let mut keys: Vec<_> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = fields.clone();
        keys.sort_unstable();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(row["success_rate"], 75.0);
    }

    #[test]
    fn pipes_listing_matches_the_rails_catalog() {
        let names: Vec<_> = pipes_catalog().iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            [
                "top_domains",
                "domain_stats",
                "hourly_stats",
                "daily_stats",
                "error_distribution",
                "job_stats",
                "kpis",
                "ai_usage",
                "job_timeline",
                "job_event_summary",
                "account_usage",
                "account_daily_usage",
                "account_daily_usage_by_operation",
                "api_key_usage"
            ]
        );
        assert!(pipes_catalog()
            .iter()
            .flat_map(|p| &p.parameters)
            .filter(|p| p.name == "account_id")
            .all(|p| !p.required));
    }

    #[test]
    fn catalog_endpoints_follow_the_names() {
        for p in pipes_catalog() {
            assert_eq!(p.endpoint, format!("/analytics/v0/pipes/{}.json", p.name));
        }
    }

    /// A state whose ClickHouse is configured but unreachable (connection
    /// refused): every check before the query runs for real, and any query
    /// fails fast.
    async fn state_with_unreachable_clickhouse() -> Arc<AppState> {
        state_with_clickhouse("http://127.0.0.1:1").await
    }

    async fn state_with_clickhouse(url: &str) -> Arc<AppState> {
        let bus = scrapix_queue::ChannelBus::new();
        let mut state = crate::results::test_support::test_state(&bus);
        let storage = ClickHouseStorage::new(scrapix_storage::clickhouse::ClickHouseConfig {
            url: url.into(),
            auto_create_tables: false,
            ..Default::default()
        })
        .await
        .unwrap();
        Arc::get_mut(&mut state).unwrap().analytics_store = Some(Arc::new(
            crate::analytics::AnalyticsState::with_storage(storage),
        ));
        state
    }

    fn tenant(account: &str) -> Option<Extension<AuthenticatedAccount>> {
        Some(Extension(AuthenticatedAccount {
            account_id: account.into(),
            tier: "free".into(),
            api_key_id: None,
            role: None,
        }))
    }

    /// Status and JSON body (`Null` when empty) of `GET /pipes/{file}?{query}`.
    async fn call(
        caller: Option<Extension<AuthenticatedAccount>>,
        file: &str,
        query: &[(&str, &str)],
    ) -> (u16, Value) {
        let params = query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let res = run_pipe(
            State(state_with_unreachable_clickhouse().await),
            caller,
            Path(file.to_string()),
            Query(params),
        )
        .await;
        let status = res.status().as_u16();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn another_accounts_id_is_a_404_before_any_query() {
        let (status, body) = call(tenant(ACCT), "kpis.json", &[("account_id", "other")]).await;
        assert_eq!(status, 404);
        assert_eq!(
            body,
            json!({"error": "Account not found", "code": "not_found"})
        );
    }

    #[tokio::test]
    async fn unknown_pipe_or_extension_is_404() {
        for file in ["nope.json", "kpis", "kpis.csv", "pipes.json"] {
            assert_eq!(call(None, file, &[]).await.0, 404, "{file}");
        }
    }

    #[tokio::test]
    async fn required_parameters_are_400() {
        for (file, param) in [
            ("domain_stats.json", "domain"),
            ("job_stats.json", "job_id"),
            ("job_timeline.json", "job_id"),
            ("job_event_summary.json", "job_id"),
        ] {
            let (status, body) = call(tenant(ACCT), file, &[(param, " ")]).await;
            assert_eq!(status, 400, "{file}");
            assert_eq!(body["code"], "bad_request", "{file}");
        }
    }

    #[tokio::test]
    async fn account_pipes_need_an_account_for_the_admin() {
        for file in [
            "account_usage.json",
            "account_daily_usage.json",
            "account_daily_usage_by_operation.json",
            "api_key_usage.json",
        ] {
            let (status, body) = call(None, file, &[]).await;
            assert_eq!(status, 400, "{file}");
            assert_eq!(
                body["error"], "param is missing or the value is empty: account_id",
                "{file}"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_query_is_a_500_query_error() {
        let (status, body) = call(tenant(ACCT), "account_usage.json", &[]).await;
        assert_eq!(status, 500);
        assert_eq!(body["code"], "QUERY_ERROR");
    }

    async fn list(
        caller: Option<Extension<AuthenticatedAccount>>,
        account_id: Option<&str>,
    ) -> Response {
        let params = account_id
            .map(|a| Params::from([("account_id".to_string(), a.to_string())]))
            .unwrap_or_default();
        list_pipes(
            State(state_with_unreachable_clickhouse().await),
            caller,
            Query(params),
        )
        .await
    }

    #[tokio::test]
    async fn listing_needs_clickhouse_and_the_callers_account() {
        let res = list(tenant(ACCT), None).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let pipes: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(pipes.as_array().unwrap().len(), 14);
        assert_eq!(pipes[0]["parameters"][0]["type"], "integer");
        assert_eq!(
            list(tenant(ACCT), Some("other")).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    /// A local HTTP server standing in for ClickHouse: it records the SQL of
    /// every request and answers 200 with an empty body (zero rows). The client
    /// inlines binds into the SQL, so the recorded text is what ClickHouse
    /// would run.
    struct RecordingClickHouse {
        url: String,
        sql: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl RecordingClickHouse {
        async fn start() -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let sql = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = sql.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let recorded = recorded.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 4096];
                        // Read headers, then the body: by Content-Length, or
                        // up to the terminating chunk when chunked.
                        loop {
                            let n = socket.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                            let text = String::from_utf8_lossy(&buf).to_string();
                            let Some(end) = text.find("\r\n\r\n") else {
                                continue;
                            };
                            let head = text[..end].to_ascii_lowercase();
                            let body = &text[end + 4..];
                            let complete = if let Some(len) = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                            {
                                body.len() >= len
                            } else if head.contains("transfer-encoding: chunked") {
                                body.ends_with("0\r\n\r\n")
                            } else {
                                true
                            };
                            if complete {
                                break;
                            }
                        }
                        let text = String::from_utf8_lossy(&buf).to_string();
                        // The client sends the SQL in the `query` URL parameter
                        // (GET) or in the body (POST); keep both, decoded.
                        let target = text
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("/");
                        let in_url = url::Url::parse(&format!("http://clickhouse{target}"))
                            .map(|u| {
                                u.query_pairs()
                                    .filter(|(k, _)| k == "query")
                                    .map(|(_, v)| v.into_owned())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                        recorded.lock().unwrap().push(format!("{in_url}\n{body}"));
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                    });
                }
            });
            Self { url, sql }
        }

        /// Drain what was recorded since the last call.
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.sql.lock().unwrap())
        }
    }

    const ACCOUNT_FILTERED: [&str; 10] = [
        "top_domains",
        "domain_stats",
        "hourly_stats",
        "daily_stats",
        "error_distribution",
        "job_stats",
        "kpis",
        "ai_usage",
        "job_timeline",
        "job_event_summary",
    ];

    /// Runs `file` against the recording server; returns the status and the
    /// SQL that reached it.
    async fn run_recorded(
        ch: &RecordingClickHouse,
        caller: Option<Extension<AuthenticatedAccount>>,
        file: &str,
    ) -> (u16, Vec<String>) {
        let params: Params = [("domain", "example.com"), ("job_id", "job_1")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ch.take();
        let res = run_pipe(
            State(state_with_clickhouse(&ch.url).await),
            caller,
            Path(file.to_string()),
            Query(params),
        )
        .await;
        (res.status().as_u16(), ch.take())
    }

    #[tokio::test]
    async fn pipes_always_bind_the_tenant_account() {
        let ch = RecordingClickHouse::start().await;
        let predicate = format!("account_id = '{ACCT}'");
        for name in ACCOUNT_FILTERED {
            let (status, queries) = run_recorded(&ch, tenant(ACCT), &format!("{name}.json")).await;
            assert_eq!(status, 200, "{name}");
            assert!(!queries.is_empty(), "{name} sent no SQL");
            for sql in &queries {
                assert!(sql.contains("SELECT"), "{name} sent no query: {sql}");
                assert!(
                    sql.contains(&predicate),
                    "{name} is not account-scoped: {sql}"
                );
            }
        }
        // The account-level pipes read one account by construction.
        for name in [
            "account_usage",
            "account_daily_usage",
            "account_daily_usage_by_operation",
            "api_key_usage",
        ] {
            let (status, queries) = run_recorded(&ch, tenant(ACCT), &format!("{name}.json")).await;
            assert_eq!(status, 200, "{name}");
            assert!(!queries.is_empty(), "{name} sent no SQL");
            for sql in &queries {
                assert!(sql.contains("SELECT"), "{name} sent no query: {sql}");
                assert!(
                    sql.contains(&predicate),
                    "{name} is not account-scoped: {sql}"
                );
            }
        }
    }

    #[tokio::test]
    async fn admin_pipes_without_account_carry_no_account_predicate() {
        let ch = RecordingClickHouse::start().await;
        for name in ACCOUNT_FILTERED {
            let (status, queries) = run_recorded(&ch, None, &format!("{name}.json")).await;
            assert_eq!(status, 200, "{name}");
            assert!(!queries.is_empty(), "{name} sent no SQL");
            for sql in &queries {
                assert!(sql.contains("SELECT"), "{name} sent no query: {sql}");
                assert!(
                    !sql.contains("account_id ="),
                    "{name} filters an admin call by account: {sql}"
                );
            }
        }
    }
}
