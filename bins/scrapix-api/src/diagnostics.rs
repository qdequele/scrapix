//! In-memory diagnostics: `GET /stats`, `GET /errors`, `GET /domains`.
//!
//! Recent errors (a 1000-entry ring buffer) and per-domain counters, kept
//! since API startup and fed by crawl events. Every record carries the
//! account of the job it came from, and the handlers only return the
//! caller's own: an account (API key, session, OAuth, Lab service call)
//! sees its jobs, errors and domains; the standalone admin key (or auth
//! disabled) sees everything.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Extension, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use scrapix_core::JobStatus;

use crate::auth::AuthenticatedAccount;
use crate::{extract_account_context, AppState, DiagnosticsState};

/// Most recent errors kept in memory.
const MAX_RECENT_ERRORS: usize = 1000;

// ============================================================================
// Response types
// ============================================================================

/// System stats response
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct SystemStatsResponse {
    /// The engine's own Meilisearch (standalone); omitted for an account,
    /// whose Meilisearch is resolved per account.
    meilisearch: Option<MeilisearchStats>,
    jobs: JobSummary,
    diagnostics: DiagnosticsStats,
    collected_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct MeilisearchStats {
    available: bool,
    url: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct JobSummary {
    total: usize,
    running: usize,
    completed: usize,
    failed: usize,
    pending: usize,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DiagnosticsStats {
    recent_errors_count: usize,
    tracked_domains: usize,
    total_requests: u64,
    total_successes: u64,
    total_failures: u64,
}

/// Errors response
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorsResponse {
    errors: Vec<ErrorRecord>,
    total_count: usize,
    by_status: HashMap<u16, u64>,
    by_domain: Vec<(String, u64)>,
    source: String,
}

/// Error record for tracking
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorRecord {
    pub(crate) url: String,
    pub(crate) domain: String,
    pub(crate) error: String,
    pub(crate) status_code: Option<u16>,
    pub(crate) job_id: String,
    pub(crate) timestamp: String,
    pub(crate) retry_count: u32,
    /// Account of the job (`None`: standalone); used for scoping only.
    #[serde(skip)]
    pub(crate) account_id: Option<String>,
}

/// Errors query parameters
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub(crate) struct ErrorsQuery {
    #[serde(default = "default_last")]
    last: usize,
    job_id: Option<String>,
}

fn default_last() -> usize {
    20
}

/// Domains response
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DomainsResponse {
    domains: Vec<DomainInfo>,
    total_domains: usize,
    source: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DomainInfo {
    domain: String,
    total_requests: u64,
    successful_requests: u64,
    failed_requests: u64,
    avg_response_time_ms: Option<f64>,
}

/// Domains query parameters
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub(crate) struct DomainsQuery {
    #[serde(default = "default_top")]
    top: usize,
    filter: Option<String>,
}

fn default_top() -> usize {
    20
}

// ============================================================================
// Recording
// ============================================================================

/// Per-domain counters are kept per account.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DomainKey {
    account_id: Option<String>,
    domain: String,
}

/// Per-domain counter for in-memory tracking
#[derive(Debug, Clone, Default)]
pub(crate) struct DomainCounter {
    requests: u64,
    successes: u64,
    failures: u64,
    total_response_time_ms: u64,
}

impl DomainCounter {
    fn add(&mut self, other: &DomainCounter) {
        self.requests += other.requests;
        self.successes += other.successes;
        self.failures += other.failures;
        self.total_response_time_ms += other.total_response_time_ms;
    }
}

impl DiagnosticsState {
    /// A page of `account_id`'s job fetched from `domain`.
    pub(crate) fn record_success(&self, account_id: Option<String>, domain: String, ms: u64) {
        let mut counters = self.domain_counters.write();
        let counter = counters
            .entry(DomainKey { account_id, domain })
            .or_default();
        counter.requests += 1;
        counter.successes += 1;
        counter.total_response_time_ms += ms;
    }

    /// A page that failed (kept in the recent-errors buffer too).
    pub(crate) fn record_failure(&self, record: ErrorRecord) {
        {
            let mut counters = self.domain_counters.write();
            let counter = counters
                .entry(DomainKey {
                    account_id: record.account_id.clone(),
                    domain: record.domain.clone(),
                })
                .or_default();
            counter.requests += 1;
            counter.failures += 1;
        }
        let mut errors = self.recent_errors.write();
        errors.push_back(record);
        while errors.len() > MAX_RECENT_ERRORS {
            errors.pop_front();
        }
    }

    /// The domains `scope` can see, counters summed per domain.
    fn domains(&self, scope: Option<&str>) -> HashMap<String, DomainCounter> {
        let mut out: HashMap<String, DomainCounter> = HashMap::new();
        for (key, counter) in self.domain_counters.read().iter() {
            if visible(scope, key.account_id.as_deref()) {
                out.entry(key.domain.clone()).or_default().add(counter);
            }
        }
        out
    }

    /// The recent errors `scope` can see, oldest first.
    fn errors(&self, scope: Option<&str>) -> Vec<ErrorRecord> {
        self.recent_errors
            .read()
            .iter()
            .filter(|e| visible(scope, e.account_id.as_deref()))
            .cloned()
            .collect()
    }
}

/// `None` scope (admin key, auth disabled) sees every record; an account
/// only its own.
fn visible(scope: Option<&str>, owner: Option<&str>) -> bool {
    scope.is_none_or(|account| owner == Some(account))
}

// ============================================================================
// Handlers
// ============================================================================

/// System stats
///
/// Jobs, recent errors and domain counters since API startup, for the
/// caller's account (every account with the standalone admin key).
#[utoipa::path(get, path = "/stats", tag = "health", responses((status = 200, body = SystemStatsResponse), (status = 401, body = crate::ApiError)), security(("api_key" = [])))]
pub(crate) async fn handle_stats(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
) -> Json<SystemStatsResponse> {
    let ctx = extract_account_context(&account_ext).await;
    let scope = ctx.as_ref().map(|c| c.account_id.as_str());

    let mut jobs = JobSummary {
        total: 0,
        running: 0,
        completed: 0,
        failed: 0,
        pending: 0,
    };
    for job in state.crawl.jobs.read().values() {
        if !visible(scope, job.account_id.as_deref()) {
            continue;
        }
        jobs.total += 1;
        match job.status {
            JobStatus::Running => jobs.running += 1,
            JobStatus::Completed => jobs.completed += 1,
            JobStatus::Failed | JobStatus::Cancelled => jobs.failed += 1,
            JobStatus::Pending | JobStatus::Paused => jobs.pending += 1,
        }
    }

    let domains = state.diagnostics.domains(scope);
    let mut totals = DomainCounter::default();
    for counter in domains.values() {
        totals.add(counter);
    }
    let diagnostics = DiagnosticsStats {
        recent_errors_count: state.diagnostics.errors(scope).len(),
        tracked_domains: domains.len(),
        total_requests: totals.requests,
        total_successes: totals.successes,
        total_failures: totals.failures,
    };

    // The engine's own Meilisearch (from env): not an account's business.
    let meilisearch = if scope.is_some() {
        None
    } else {
        std::env::var("MEILISEARCH_URL")
            .ok()
            .map(|url| MeilisearchStats {
                available: true,
                url,
            })
    };

    Json(SystemStatsResponse {
        meilisearch,
        jobs,
        diagnostics,
        collected_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Recent errors
///
/// The last failed pages of the caller's jobs (every account's with the
/// standalone admin key), most recent first.
#[utoipa::path(get, path = "/errors", tag = "health", params(ErrorsQuery), responses((status = 200, body = ErrorsResponse), (status = 401, body = crate::ApiError)), security(("api_key" = [])))]
pub(crate) async fn handle_errors(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Query(params): Query<ErrorsQuery>,
) -> Json<ErrorsResponse> {
    let ctx = extract_account_context(&account_ext).await;
    let scope = ctx.as_ref().map(|c| c.account_id.as_str());

    let filtered: Vec<ErrorRecord> = state
        .diagnostics
        .errors(scope)
        .into_iter()
        .filter(|e| params.job_id.as_ref().is_none_or(|id| &e.job_id == id))
        .collect();
    let total_count = filtered.len();

    // Take last N errors (most recent)
    let recent: Vec<ErrorRecord> = filtered.into_iter().rev().take(params.last).collect();

    let mut by_status: HashMap<u16, u64> = HashMap::new();
    let mut domain_counts: HashMap<String, u64> = HashMap::new();
    for error in &recent {
        if let Some(code) = error.status_code {
            *by_status.entry(code).or_insert(0) += 1;
        }
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

/// Domain stats
///
/// Per-domain request counters of the caller's jobs (every account's with
/// the standalone admin key), busiest first.
#[utoipa::path(get, path = "/domains", tag = "health", params(DomainsQuery), responses((status = 200, body = DomainsResponse), (status = 401, body = crate::ApiError)), security(("api_key" = [])))]
pub(crate) async fn handle_domains(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Query(params): Query<DomainsQuery>,
) -> Json<DomainsResponse> {
    let ctx = extract_account_context(&account_ext).await;
    let scope = ctx.as_ref().map(|c| c.account_id.as_str());

    let mut sorted: Vec<(String, DomainCounter)> = state
        .diagnostics
        .domains(scope)
        .into_iter()
        .filter(|(domain, _)| params.filter.as_ref().is_none_or(|f| domain.contains(f)))
        .collect();
    let total_domains = sorted.len();
    sorted.sort_by(|a, b| b.1.requests.cmp(&a.1.requests).then(a.0.cmp(&b.0)));
    sorted.truncate(params.top);

    let domains = sorted
        .into_iter()
        .map(|(domain, counter)| DomainInfo {
            domain,
            total_requests: counter.requests,
            successful_requests: counter.successes,
            failed_requests: counter.failures,
            avg_response_time_ms: (counter.successes > 0)
                .then(|| counter.total_response_time_ms as f64 / counter.successes as f64),
        })
        .collect();

    Json(DomainsResponse {
        domains,
        total_domains,
        source: "memory".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrapix_core::JobState;
    use scrapix_queue::{ChannelBus, CrawlEvent};

    const A: &str = "7f1c2a8e-0000-4000-8000-00000000000a";
    const B: &str = "7f1c2a8e-0000-4000-8000-00000000000b";

    fn tenant(account_id: &str) -> Option<Extension<AuthenticatedAccount>> {
        Some(Extension(AuthenticatedAccount {
            account_id: account_id.to_string(),
            tier: "free".to_string(),
            api_key_id: None,
            role: None,
        }))
    }

    /// A state with one running job per account (`job-a`, `job-b`), each
    /// with one crawled and one failed page on its own domain.
    fn two_tenants() -> Arc<AppState> {
        let bus = ChannelBus::new();
        let state = crate::results::test_support::test_state(&bus);
        for (job_id, account, domain) in [("job-a", A, "a.test"), ("job-b", B, "b.test")] {
            let mut job = JobState::with_account(job_id, "idx", account);
            job.start();
            state.insert_job(job);
            state.process_event(
                job_id,
                &CrawlEvent::PageCrawled {
                    job_id: job_id.into(),
                    account_id: None,
                    url: format!("https://{domain}/"),
                    status: 200,
                    content_length: 10,
                    duration_ms: 4,
                    timestamp: 0,
                    links_published: 0,
                    url_message_id: format!("{job_id}-ok"),
                    js_rendered: false,
                    sitemap_pending: false,
                },
            );
            state.process_event(
                job_id,
                &CrawlEvent::PageFailed {
                    job_id: job_id.into(),
                    account_id: None,
                    url: format!("https://{domain}/missing"),
                    error: "404 Not Found".into(),
                    retry_count: 0,
                    timestamp: 0,
                    status: Some(404),
                    url_message_id: format!("{job_id}-ko"),
                },
            );
        }
        state
    }

    fn json<T: Serialize>(v: Json<T>) -> serde_json::Value {
        serde_json::to_value(v.0).unwrap()
    }

    fn errors_query(job_id: Option<&str>) -> Query<ErrorsQuery> {
        Query(ErrorsQuery {
            last: 20,
            job_id: job_id.map(str::to_string),
        })
    }

    fn domains_query() -> Query<DomainsQuery> {
        Query(DomainsQuery {
            top: 20,
            filter: None,
        })
    }

    #[tokio::test]
    async fn errors_are_scoped_to_the_callers_account() {
        let state = two_tenants();
        let mine = json(handle_errors(State(state.clone()), tenant(A), errors_query(None)).await);
        assert_eq!(mine["total_count"], 1);
        assert_eq!(mine["errors"][0]["job_id"], "job-a");
        assert_eq!(mine["errors"][0]["domain"], "a.test");
        assert!(mine["errors"][0].get("account_id").is_none());

        // Another account's job id finds nothing.
        let other =
            json(handle_errors(State(state.clone()), tenant(A), errors_query(Some("job-b"))).await);
        assert_eq!(other["total_count"], 0);

        // The admin key (no account) sees every account.
        let all = json(handle_errors(State(state), None, errors_query(None)).await);
        assert_eq!(all["total_count"], 2);
    }

    #[tokio::test]
    async fn domains_are_scoped_to_the_callers_account() {
        let state = two_tenants();
        let mine = json(handle_domains(State(state.clone()), tenant(B), domains_query()).await);
        assert_eq!(mine["total_domains"], 1);
        assert_eq!(mine["domains"][0]["domain"], "b.test");
        assert_eq!(mine["domains"][0]["total_requests"], 2);
        assert_eq!(mine["domains"][0]["failed_requests"], 1);

        let all = json(handle_domains(State(state), None, domains_query()).await);
        assert_eq!(all["total_domains"], 2);
    }

    #[tokio::test]
    async fn domain_counters_of_two_accounts_on_one_domain_stay_apart() {
        let bus = ChannelBus::new();
        let state = crate::results::test_support::test_state(&bus);
        state
            .diagnostics
            .record_success(Some(A.into()), "shared.test".into(), 10);
        state
            .diagnostics
            .record_success(Some(B.into()), "shared.test".into(), 30);
        let mine = json(handle_domains(State(state.clone()), tenant(A), domains_query()).await);
        assert_eq!(mine["domains"][0]["total_requests"], 1);
        assert_eq!(mine["domains"][0]["avg_response_time_ms"], 10.0);
        let all = json(handle_domains(State(state), None, domains_query()).await);
        assert_eq!(all["domains"][0]["total_requests"], 2);
        assert_eq!(all["domains"][0]["avg_response_time_ms"], 20.0);
    }

    #[tokio::test]
    async fn stats_count_only_the_callers_jobs() {
        let state = two_tenants();
        let mine = json(handle_stats(State(state.clone()), tenant(A)).await);
        assert_eq!(mine["jobs"]["total"], 1);
        assert_eq!(mine["jobs"]["running"], 1);
        assert_eq!(mine["diagnostics"]["recent_errors_count"], 1);
        assert_eq!(mine["diagnostics"]["tracked_domains"], 1);
        assert_eq!(mine["diagnostics"]["total_requests"], 2);
        assert!(mine["meilisearch"].is_null(), "no engine internals");

        let nobody = json(
            handle_stats(
                State(state.clone()),
                tenant("7f1c2a8e-0000-4000-8000-0000000000ff"),
            )
            .await,
        );
        assert_eq!(nobody["jobs"]["total"], 0);
        assert_eq!(nobody["diagnostics"]["total_requests"], 0);

        let all = json(handle_stats(State(state), None).await);
        assert_eq!(all["jobs"]["total"], 2);
        assert_eq!(all["diagnostics"]["recent_errors_count"], 2);
    }

    #[tokio::test]
    async fn recent_errors_are_capped() {
        let bus = ChannelBus::new();
        let state = crate::results::test_support::test_state(&bus);
        for i in 0..(MAX_RECENT_ERRORS + 5) {
            state.diagnostics.record_failure(ErrorRecord {
                url: format!("https://a.test/{i}"),
                domain: "a.test".into(),
                error: "boom".into(),
                status_code: None,
                job_id: "j".into(),
                timestamp: String::new(),
                retry_count: 0,
                account_id: None,
            });
        }
        let errors = state.diagnostics.errors(None);
        assert_eq!(errors.len(), MAX_RECENT_ERRORS);
        assert_eq!(errors[0].url, "https://a.test/5");
    }
}
