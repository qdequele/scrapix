//! `POST /crawl/suggest` + `GET /crawl/suggest/{id}` (SCR-95): propose a
//! crawl config for a website from its URL.
//!
//! 1. **Start page**: scraped first, so the map follows a redirect
//!    (`example.com` → `www.example.com/en/`).
//! 2. **Map** (`run_map`, billed as a `/map`): the site's URLs, from its
//!    sitemaps, else its links (one level deep when few are found). The
//!    pages under the start URL's path are grouped into sections by their
//!    first path segment (after a language segment).
//! 3. **Samples**: a few pages of the largest sections, through the
//!    `/scrape` pipeline (billed as scrapes): their length, schema.org
//!    types, platform, linked PDFs and office documents, and whether the
//!    HTTP response looks like a JavaScript app shell.
//! 4. **JavaScript check**: up to [`MAX_RENDERED`] suspicious pages are
//!    rendered with the browser (when the server and the plan have one);
//!    much more text means the crawl needs `crawler_type: browser`.
//! 5. **Classification**: each section, then the site, deterministically
//!    (`analysis`); then one AI call confirms or corrects it (`ai`; the
//!    server's provider standalone, the team's Lumen gateway hosted).
//!    Without AI the deterministic result stands.
//! 6. **Proposal** (`presets`): a compact `CrawlConfig`, one reason per
//!    decision, an estimate and warnings.
//!
//! The job is a regular engine job (`/jobs`, events, webhooks, cancel); the
//! sampled pages are its results (`GET /job/{id}/results`).

pub(crate) mod ai;
pub(crate) mod analysis;
pub(crate) mod presets;

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Extension, Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::{info, warn};

use scrapix_ai::{
    AiService, AiUsageContext, AiUsageTally, ExtractionConfig, AI_USAGE_CONTEXT, AI_USAGE_TALLY,
};
use scrapix_core::config::WebhookConfig;

use crate::ai_access::{resolve_ai, with_key_retry, ResolvedAi};
use crate::auth::AuthenticatedAccount;
use crate::engine_jobs;
use crate::job_kind::JobKind;
use crate::results::{self, find_owned_job, status_str};
use crate::{
    billing, check_write_permission, extract_account_context, run_map, AccountContext, ApiError,
    AppState, MapRequest,
};

use analysis::{
    classify_section, classify_site, group_sections, page_signals, rendering_reveals_content,
    word_count, MappedPage, PageSignals, SectionKind, SitePlatform, SiteSection, SiteType,
};
use presets::{Findings, JsFinding, SuggestEstimate, SuggestReason};

/// Pages sampled by default, and at most.
const DEFAULT_SAMPLES: usize = 8;
const MAX_SAMPLES: usize = 20;
/// URLs the map lists at most.
const MAP_LIMIT: usize = 2_000;
/// Below this many pages, the map follows the start page's links too.
const FEW_PAGES: usize = 20;
/// Suspicious pages rendered with a browser at most.
const MAX_RENDERED: usize = 2;
/// Pages scraped at once.
const FETCH_CONCURRENCY: usize = 4;
/// Token budget of the AI call's content.
const AI_CONTEXT_TOKENS: usize = 12_000;

// ============================================================================
// Types
// ============================================================================

/// Request body for `POST /crawl/suggest`
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct SuggestRequest {
    /// The website. A path (`https://example.com/docs`) keeps the crawl
    /// under it.
    #[schema(example = "https://example.com")]
    pub url: String,
    /// Pages sampled to analyze the site (default 8, max 20). Each is
    /// billed as a `/scrape`.
    #[serde(default)]
    pub max_samples: Option<usize>,
    /// Confirm the classification with the AI provider (default true).
    /// Without one (or with `false`), the deterministic analysis decides.
    #[serde(default = "default_true")]
    pub ai: bool,
    /// Model of the AI pass, from the team's Gateway; the default model
    /// when absent
    #[serde(default)]
    #[schema(example = "openai/gpt-4o-mini")]
    pub model: Option<String>,
    /// Render up to two pages that look like JavaScript app shells with a
    /// browser to confirm the crawl needs one (default true; needs
    /// Chrome/Chromium on the server and JS rendering on the plan).
    #[serde(default = "default_true")]
    pub check_js: bool,
    /// Webhook subscriptions (same as a crawl's `webhooks`)
    #[serde(default)]
    pub webhooks: Vec<WebhookConfig>,
}

fn default_true() -> bool {
    true
}

/// Response of `POST /crawl/suggest`
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct CreateSuggestResponse {
    pub job_id: String,
    pub status: String,
}

/// What the site is.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct SiteProfile {
    /// The URL analyzed (after redirects)
    pub url: String,
    /// The path the crawl stays under (`/` for the whole site): the URL's
    /// own, or its parent when the URL is one language of a site in several
    /// (`/en` of a site also in `/fr` and `/de` gives `/`)
    #[serde(default)]
    pub base_path: String,
    pub site_type: SiteType,
    /// How sure the analysis is, from 0 to 1
    pub confidence: f32,
    /// Why the site got this type
    pub because: String,
    /// The site builder or framework, when recognized
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<SitePlatform>,
    /// One sentence about the site (AI pass only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the AI pass confirmed or corrected the classification
    pub ai_classified: bool,
    /// Language path segments found (`fr`, `en-us`)
    pub languages: Vec<String>,
    /// The site's sections, largest first
    pub sections: Vec<SiteSection>,
}

/// A page the analysis scraped.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct SuggestSample {
    pub url: String,
    /// Its section key
    pub section: String,
    /// Whether the page was fetched
    pub success: bool,
    /// Why it could not be
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Words of its main content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub words: Option<usize>,
    /// Words once rendered with a browser (JavaScript check only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered_words: Option<usize>,
}

/// `GET /crawl/suggest/{id}` response
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct SuggestStatusResponse {
    pub job_id: String,
    /// `running`, `completed`, `failed`, `cancelled` or `paused`
    #[schema(value_type = scrapix_core::JobStatus)]
    pub status: String,
    /// The proposed crawl config (`null` until the job completed): only
    /// the fields it sets, every other field takes its default. Pass it
    /// to `POST /crawl` as is, or edit it first.
    #[schema(value_type = Option<scrapix_core::config::CrawlConfig>)]
    pub config: Option<Value>,
    /// What the site is (`null` until the job completed)
    pub profile: Option<SiteProfile>,
    /// Why the config holds each value
    pub reasons: Vec<SuggestReason>,
    /// What crawling with the config is expected to cost
    pub estimate: Option<SuggestEstimate>,
    /// The pages the analysis scraped
    pub samples: Vec<SuggestSample>,
    /// Things to check before crawling
    pub warnings: Vec<String>,
    /// Why the job failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The AI pass's LLM call and the provider cost the hosted AI gateway
    /// reported for it, in micro-USD (billed by the gateway, not in the
    /// job's credits). Omitted when no AI call was made.
    #[serde(skip_serializing_if = "scrapix_core::AiUsage::is_empty")]
    pub ai_usage: scrapix_core::AiUsage,
}

/// What is stored as the job's summary (results store, seq 0).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SuggestSummary {
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    profile: Option<SiteProfile>,
    #[serde(default)]
    reasons: Vec<SuggestReason>,
    #[serde(default)]
    estimate: Option<SuggestEstimate>,
    #[serde(default)]
    samples: Vec<SuggestSample>,
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "scrapix_core::AiUsage::is_empty")]
    ai_usage: scrapix_core::AiUsage,
}

// ============================================================================
// Handlers
// ============================================================================

/// Suggest a crawl config for a website
///
/// Starts a job that maps the site, samples a few pages, checks whether it
/// needs JavaScript rendering, classifies it (ecommerce, blog, docs,
/// marketing, support or mixed; confirmed by the AI provider when there is
/// one) and proposes a crawl config: rendering, URL patterns, page cap,
/// Markdown, block splitting, schema.org, AI summary and extraction, PDFs
/// and office documents, each scoped to the sections it suits, with a
/// reason for every choice. Poll `GET /crawl/suggest/{id}` for the result.
/// Billed as one or two `/map`s and one `/scrape` per sampled page.
#[utoipa::path(
    post,
    path = "/crawl/suggest",
    tag = "crawl",
    request_body = SuggestRequest,
    responses(
        (status = 200, body = CreateSuggestResponse),
        (status = 400, body = ApiError),
        (status = 402, description = "Not enough credits", body = ApiError)
    ),
    security(("api_key" = []))
)]
pub(crate) async fn create_suggest(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Json(request): Json<SuggestRequest>,
) -> Result<Json<CreateSuggestResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    check_write_permission(&account_ctx)?;
    start_suggest(&state, &account_ctx, request).await.map(Json)
}

/// The URL to analyze, validated like `/map` does.
pub(crate) fn validate_url(raw: &str) -> Result<url::Url, ApiError> {
    let url = url::Url::parse(raw.trim())
        .map_err(|e| ApiError::new(format!("Invalid URL: {e}"), "validation_error"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::new(
            "Only http and https URLs are supported",
            "validation_error",
        ));
    }
    match url.host() {
        Some(url::Host::Domain(_)) => Ok(url),
        Some(_) => Err(ApiError::new(
            "Raw IP addresses are not allowed, use a hostname instead",
            "validation_error",
        )),
        None => Err(ApiError::new("The URL has no host", "validation_error")),
    }
}

/// Validation, pre-flight, job creation and runner spawn.
pub(crate) async fn start_suggest(
    state: &Arc<AppState>,
    account_ctx: &Option<AccountContext>,
    request: SuggestRequest,
) -> Result<CreateSuggestResponse, ApiError> {
    let SuggestRequest {
        url,
        max_samples,
        ai,
        model,
        check_js,
        mut webhooks,
    } = request;
    let url = validate_url(&url)?;
    let samples = max_samples.unwrap_or(DEFAULT_SAMPLES);
    if !(1..=MAX_SAMPLES).contains(&samples) {
        return Err(ApiError::new(
            format!("max_samples must be between 1 and {MAX_SAMPLES}"),
            "validation_error",
        ));
    }
    let model = model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    engine_jobs::validate_webhooks(&mut webhooks)?;
    engine_jobs::preflight(
        state,
        account_ctx.as_ref(),
        engine_jobs::PlanCheck {
            max_depth: None,
            js_rendering: false,
        },
    )
    .await?;

    let mut config = serde_json::json!({
        "url": url.as_str(),
        "max_samples": samples,
        "ai": ai,
        "model": model,
        "check_js": check_js,
        "webhooks": webhooks,
    });
    crate::webhooks::redact_webhooks_json(&mut config);
    let job = engine_jobs::start_job(
        state,
        account_ctx,
        JobKind::CrawlSuggest,
        vec![url.to_string()],
        config,
        webhooks,
    )
    .await;

    let runner = SuggestRunner {
        state: state.clone(),
        account_ctx: Arc::new(engine_jobs::clone_account_ctx(account_ctx)),
        job_id: job.job_id.clone(),
        samples,
        use_ai: ai,
        model,
        check_js,
        ai_tally: AiUsageTally::new(),
        seq: std::sync::atomic::AtomicU64::new(0),
    };
    let tally = runner.ai_tally.clone();
    tokio::spawn(AI_USAGE_TALLY.scope(tally, async move { runner.run(url).await }));

    Ok(CreateSuggestResponse {
        job_id: job.job_id,
        status: "running".to_string(),
    })
}

/// Get a config suggestion's status and result
#[utoipa::path(
    get,
    path = "/crawl/suggest/{id}",
    tag = "crawl",
    params(("id" = String, Path, description = "Suggest job ID")),
    responses((status = 200, body = SuggestStatusResponse), (status = 404, body = ApiError)),
    security(("api_key" = []))
)]
pub(crate) async fn get_suggest(
    State(state): State<Arc<AppState>>,
    account_ext: Option<Extension<AuthenticatedAccount>>,
    Path(job_id): Path<String>,
) -> Result<Json<SuggestStatusResponse>, ApiError> {
    let account_ctx = extract_account_context(&account_ext).await;
    let job = find_owned_job(&state, &account_ctx, &job_id).await?;
    if JobKind::of(&job) != JobKind::CrawlSuggest {
        return Err(ApiError::new("Suggest job not found", "not_found"));
    }
    let summary: SuggestSummary = results::load_summary(&state, &job_id)
        .await
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    Ok(Json(SuggestStatusResponse {
        job_id: job.job_id,
        status: status_str(&job.status),
        config: summary.config,
        profile: summary.profile,
        reasons: summary.reasons,
        estimate: summary.estimate,
        samples: summary.samples,
        warnings: summary.warnings,
        error: job.error_message,
        ai_usage: summary.ai_usage,
    }))
}

// ============================================================================
// Runner
// ============================================================================

struct SuggestRunner {
    state: Arc<AppState>,
    account_ctx: Arc<Option<AccountContext>>,
    job_id: String,
    samples: usize,
    use_ai: bool,
    model: Option<String>,
    check_js: bool,
    ai_tally: Arc<AiUsageTally>,
    /// Sequence number of the next stored page.
    seq: std::sync::atomic::AtomicU64,
}

/// One scraped page.
struct Scraped {
    url: String,
    success: bool,
    error: Option<String>,
    payload: Value,
}

/// The scrape formats the analysis reads.
fn analysis_options(render_js: bool) -> Map<String, Value> {
    let mut options = Map::new();
    let formats: &[&str] = if render_js {
        &["markdown"]
    } else {
        &["markdown", "rawhtml", "schema", "links", "metadata"]
    };
    options.insert("formats".into(), serde_json::json!(formats));
    options.insert("only_main_content".into(), Value::Bool(true));
    options.insert("render_js".into(), Value::Bool(render_js));
    options
}

/// The base path a URL's crawl stays under: its path as a directory (the
/// parent one for a file such as `/index.html`), `/` for the whole site.
pub(crate) fn base_path(url: &url::Url) -> String {
    let path = url.path();
    let dir = match path.rsplit_once('/') {
        Some((parent, last)) if last.contains('.') => parent,
        _ => path.trim_end_matches('/'),
    };
    if dir.is_empty() {
        "/".to_string()
    } else {
        format!("{dir}/")
    }
}

/// The URLs to sample: round-robin over the sections worth crawling,
/// largest first, skipping those already scraped.
pub(crate) fn pick_samples(
    sections: &[analysis::Section],
    count: usize,
    already: &HashSet<String>,
    skip_versions: bool,
) -> Vec<(String, String)> {
    let candidates: Vec<&analysis::Section> = sections
        .iter()
        .filter(|s| !analysis::kind_from_key(&s.key).is_some_and(SectionKind::is_excluded))
        .filter(|s| !(skip_versions && analysis::is_version_key(&s.key)))
        .collect();
    let mut picked = Vec::new();
    let mut seen: HashSet<&str> = already.iter().map(String::as_str).collect();
    while picked.len() < count {
        let mut any = false;
        for section in &candidates {
            if picked.len() >= count {
                break;
            }
            // A URL with a query string is a filtered or sorted copy.
            let Some(url) = section
                .urls
                .iter()
                .find(|u| !seen.contains(u.as_str()) && !u.contains('?'))
            else {
                continue;
            };
            seen.insert(url);
            picked.push((url.clone(), section.key.clone()));
            any = true;
        }
        if !any {
            break;
        }
    }
    picked
}

impl SuggestRunner {
    fn account_id(&self) -> Option<String> {
        self.account_ctx
            .as_ref()
            .as_ref()
            .map(|c| c.account_id.clone())
    }

    async fn save(&self, summary: &SuggestSummary) {
        let ai_usage = self.ai_tally.totals();
        let mut value = serde_json::to_value(summary).unwrap_or_default();
        if !ai_usage.is_empty() {
            if let Some(map) = value.as_object_mut() {
                map.insert(
                    "ai_usage".to_string(),
                    serde_json::to_value(ai_usage).unwrap_or_default(),
                );
            }
            self.state
                .update_job(&self.job_id, |j| j.ai_usage = ai_usage);
            if let Some(store) = &self.state.job_store {
                let entry = (
                    self.job_id.clone(),
                    serde_json::json!({ "ai_usage": ai_usage }),
                );
                if let Err(e) = store.flush_job_accounting(&[entry]).await {
                    warn!(job_id = %self.job_id, error = %e, "Failed to store the suggestion's AI usage");
                }
            }
        }
        results::store_summary(&self.state, &self.job_id, value).await;
    }

    async fn fail(&self, summary: &SuggestSummary, error: &str) {
        self.save(summary).await;
        engine_jobs::fail_job(&self.state, &self.job_id, error);
    }

    fn stopped(&self) -> bool {
        engine_jobs::is_stopped(&self.state, &self.job_id)
    }

    /// Scrape `urls` through the `/scrape` pipeline (billed like it), each
    /// stored as a result of the job.
    async fn scrape(&self, urls: Vec<String>, render_js: bool) -> Vec<Scraped> {
        let indexed: Vec<(usize, String)> = urls.into_iter().enumerate().collect();
        let mut tasks = crate::batch::ScrapeTasks::new(
            self.state.clone(),
            self.account_ctx.clone(),
            Arc::new(analysis_options(render_js)),
            self.job_id.clone(),
            indexed.clone(),
            FETCH_CONCURRENCY,
            true,
        );
        let mut out: Vec<(usize, Scraped)> = Vec::new();
        while let Some(outcome) = tasks.next().await {
            let error = outcome
                .item
                .error
                .as_ref()
                .map(|e| format!("{}: {}", e.code, e.message));
            let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            results::store_page(
                &self.state,
                &self.job_id,
                seq,
                &outcome.source_url,
                outcome.item.success,
                outcome.payload.clone(),
            )
            .await;
            let final_url = if outcome.item.url.is_empty() {
                outcome.source_url.clone()
            } else {
                outcome.item.url.clone()
            };
            engine_jobs::page_event(
                &self.state,
                &self.job_id,
                self.account_id(),
                &final_url,
                outcome.item.status_code,
                outcome.duration_ms,
                error.as_deref(),
            );
            out.push((
                outcome.index,
                Scraped {
                    url: final_url,
                    success: outcome.item.success,
                    error,
                    payload: outcome.payload,
                },
            ));
        }
        out.sort_by_key(|(i, _)| *i);
        out.into_iter().map(|(_, s)| s).collect()
    }

    /// Whether the browser check can run: asked for, a browser on the
    /// server, and JS rendering on the plan (hosted).
    fn can_render(&self) -> Result<(), String> {
        if !self.check_js {
            return Err("the JavaScript check was turned off (check_js: false)".to_string());
        }
        if self.state.browser_renderer.is_none() {
            return Err("this server has no browser (Chrome/Chromium)".to_string());
        }
        if let (Some(_), Some(ctx)) = (&self.state.lab_api, self.account_ctx.as_ref()) {
            engine_jobs::enforce_limits(
                ctx,
                0,
                engine_jobs::PlanCheck {
                    max_depth: None,
                    js_rendering: true,
                },
            )
            .map_err(|_| "the plan does not include JavaScript rendering".to_string())?;
        }
        Ok(())
    }

    async fn run(self, start: url::Url) {
        let started = std::time::Instant::now();
        let mut summary = SuggestSummary::default();

        // 1. The start page (and where it redirects).
        let first = self.scrape(vec![start.to_string()], false).await;
        let Some(first) = first.into_iter().next() else {
            return self
                .fail(&summary, "The start page could not be fetched")
                .await;
        };
        if self.stopped() {
            return self.save(&summary).await;
        }
        if !first.success {
            summary.samples.push(SuggestSample {
                url: start.to_string(),
                section: analysis::HOME_KEY.to_string(),
                success: false,
                error: first.error.clone(),
                words: None,
                rendered_words: None,
            });
            let why = first.error.unwrap_or_else(|| "unknown error".to_string());
            return self
                .fail(
                    &summary,
                    &format!("The start page could not be fetched ({why})"),
                )
                .await;
        }
        let site = url::Url::parse(&first.url)
            .ok()
            .filter(|u| u.host_str().is_some())
            .unwrap_or_else(|| start.clone());
        if site.as_str() != start.as_str() {
            summary.warnings.push(format!(
                "{start} redirects to {site}: the crawl starts there"
            ));
        }
        let host = site.host_str().unwrap_or_default().to_string();
        let authority = match site.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        // The path asked for bounds the crawl when the redirect stays under
        // it (`/docs` → `/docs/getting-started/overview`); otherwise the
        // redirect target's (`/blog/` → `/resources/`).
        let mut base_path = {
            let asked = base_path(&start);
            if site.host_str() == start.host_str()
                && analysis::section_of(site.path(), &asked).is_some()
            {
                asked
            } else {
                base_path(&site)
            }
        };

        // 2. Map.
        let mapped = match self.map(&site, 0, true).await {
            Ok(links) => links,
            Err(e) => {
                return self
                    .fail(&summary, &format!("Mapping the site failed: {e}"))
                    .await
            }
        };
        // A language root (`/en`) of a site in several languages is the
        // whole site in one language: the crawl covers every language, and
        // each page's `language` tells them apart.
        let language_root = {
            let on_host: Vec<MappedPage> = mapped
                .iter()
                .filter(|p| {
                    url::Url::parse(&p.url).is_ok_and(|u| u.host_str() == Some(host.as_str()))
                })
                .cloned()
                .collect();
            analysis::language_root_parent(&base_path, &on_host)
                .map(|parent| std::mem::replace(&mut base_path, parent))
        };
        let is_under = |url: &str| {
            url::Url::parse(url).is_ok_and(|u| {
                u.host_str() == Some(host.as_str())
                    && analysis::section_of(u.path(), &base_path).is_some()
            })
        };
        let truncated = mapped.len() >= MAP_LIMIT;
        let mut under: Vec<MappedPage> = mapped.into_iter().filter(|p| is_under(&p.url)).collect();
        // The page count is known when the sitemaps list the pages under the
        // start URL (not only the start page), the map did not stop at its
        // limit, and the start page links to no page they miss.
        let mut count_known = under.len() > 1 && !truncated;
        if under.len() < FEW_PAGES && !self.stopped() {
            // No sitemap, or one filled with other parts of the site: the
            // start page's links, one level deep.
            match self.map(&site, 1, false).await {
                Ok(links) => {
                    let mut seen: HashSet<String> = under.iter().map(|p| p.url.clone()).collect();
                    for link in links.into_iter().filter(|p| is_under(&p.url)) {
                        if seen.insert(link.url.clone()) {
                            under.push(link);
                            count_known = false;
                        }
                    }
                }
                Err(e) => summary
                    .warnings
                    .push(format!("Following the start page's links failed: {e}")),
            }
        }
        if self.stopped() {
            return self.save(&summary).await;
        }
        let with_query = under.iter().filter(|p| p.url.contains('?')).count();
        let query_share = if under.is_empty() {
            0.0
        } else {
            with_query as f32 / under.len() as f32
        };
        let (mut sections, languages) = group_sections(&under, &base_path);
        if let Some(asked) = &language_root {
            let list: Vec<&str> = languages.iter().map(String::as_str).collect();
            summary.warnings.push(format!(
                "`{asked}` is one language of a site in {} ({}): the crawl covers all of them \
                 under `{base_path}`, and each page's `language` tells them apart",
                list.len(),
                list.join(", ")
            ));
        }
        if sections.is_empty() {
            sections.push(analysis::Section {
                key: analysis::HOME_KEY.to_string(),
                pages: 1,
                titles: Vec::new(),
                urls: vec![site.to_string()],
            });
        }
        let section_key = |url: &str| -> String {
            url::Url::parse(url)
                .ok()
                .and_then(|u| analysis::section_in(u.path(), &base_path, &languages))
                .map(|(_, key)| key)
                .filter(|key| sections.iter().any(|s| &s.key == key))
                .unwrap_or_else(|| analysis::OTHER_KEY.to_string())
        };

        // 3. Samples.
        let first_section = section_key(&first.url);
        let mut pages: Vec<PageSignals> = vec![page_signals(
            &first.url,
            &first_section,
            &first.payload,
            &host,
        )];
        summary.samples.push(SuggestSample {
            url: first.url.clone(),
            section: first_section,
            success: true,
            error: None,
            words: Some(pages[0].words),
            rendered_words: None,
        });
        let older_versions = analysis::has_older_versions(&sections);
        let already: HashSet<String> = [start.to_string(), first.url.clone()].into();
        let picks = pick_samples(
            &sections,
            self.samples.saturating_sub(1),
            &already,
            older_versions,
        );
        let urls: Vec<String> = picks.iter().map(|(u, _)| u.clone()).collect();
        let scraped = self.scrape(urls, false).await;
        let mut sampled: HashSet<String> = [first.url.clone()].into();
        for ((_, key), page) in picks.iter().zip(scraped) {
            // Two picks that redirect to the same page count once.
            if !sampled.insert(page.url.clone()) {
                continue;
            }
            if page.success {
                let signals = page_signals(&page.url, key, &page.payload, &host);
                summary.samples.push(SuggestSample {
                    url: page.url,
                    section: key.clone(),
                    success: true,
                    error: None,
                    words: Some(signals.words),
                    rendered_words: None,
                });
                pages.push(signals);
            } else {
                summary.samples.push(SuggestSample {
                    url: page.url,
                    section: key.clone(),
                    success: false,
                    error: page.error,
                    words: None,
                    rendered_words: None,
                });
            }
        }
        self.save(&summary).await;
        if self.stopped() {
            return;
        }

        // 4. JavaScript check.
        let mut js = JsFinding {
            sampled: pages.len(),
            suspects: pages.iter().filter(|p| p.js_suspect).count(),
            ..Default::default()
        };
        if js.suspects > 0 {
            match self.can_render() {
                Ok(()) => {
                    let suspects: Vec<(String, usize)> = pages
                        .iter()
                        .filter(|p| p.js_suspect)
                        .take(MAX_RENDERED)
                        .map(|p| (p.url.clone(), p.words))
                        .collect();
                    let urls = suspects.iter().map(|(u, _)| u.clone()).collect();
                    let rendered = self.scrape(urls, true).await;
                    for ((url, http_words), page) in suspects.iter().zip(rendered) {
                        if !page.success {
                            continue;
                        }
                        js.rendered += 1;
                        let words = word_count(
                            page.payload
                                .get("markdown")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        );
                        if let Some(sample) = summary.samples.iter_mut().find(|s| &s.url == url) {
                            sample.rendered_words = Some(words);
                        }
                        if rendering_reveals_content(*http_words, words) {
                            js.revealed += 1;
                            js.example.get_or_insert((*http_words, words));
                        }
                    }
                }
                Err(why) => summary.warnings.push(format!(
                    "{} sampled page(s) look like JavaScript app shells, but they were not \
                     rendered to confirm: {why}",
                    js.suspects
                )),
            }
        }
        if self.stopped() {
            return self.save(&summary).await;
        }

        // 5. Classification.
        let platform = site_platform(&pages);
        let context = analysis::SiteContext {
            platform,
            commerce: pages.iter().any(|p| {
                p.platform.and_then(SitePlatform::site_type) == Some(SiteType::Ecommerce)
                    || p.schema_types
                        .iter()
                        .any(|t| matches!(t.as_str(), "Product" | "ProductGroup"))
            }),
            older_versions,
        };
        let mut classified: Vec<SiteSection> = sections
            .iter()
            .map(|section| {
                let samples: Vec<&PageSignals> =
                    pages.iter().filter(|p| p.section == section.key).collect();
                let (kind, because) = classify_section(section, &samples, context);
                let mut titles = section.titles.clone();
                for t in samples.iter().filter_map(|p| p.title.clone()) {
                    if titles.len() < 5 && !titles.contains(&t) {
                        titles.push(t);
                    }
                }
                SiteSection {
                    key: section.key.clone(),
                    pages: section.pages,
                    kind,
                    because,
                    schema_types: samples
                        .iter()
                        .flat_map(|p| p.schema_types.iter().cloned())
                        .collect(),
                    avg_words: (!samples.is_empty())
                        .then(|| samples.iter().map(|p| p.words).sum::<usize>() / samples.len()),
                    avg_headings: (!samples.is_empty())
                        .then(|| samples.iter().map(|p| p.headings).sum::<usize>() / samples.len()),
                    sampled: samples.len(),
                    titles,
                }
            })
            .collect();
        let (mut site_type, mut confidence, mut because) = classify_site(&classified, platform);

        let mut ai_classified = false;
        let mut description = None;
        let mut ai_summary_hint = None;
        if self.use_ai {
            match self
                .classify_with_ai(&site, platform, &classified, &pages)
                .await
            {
                Ok(Some(answer)) => {
                    ai_classified = true;
                    for section in classified.iter_mut() {
                        if let Some(kind) = answer.sections.get(&section.key) {
                            if *kind != section.kind {
                                section.because = format!(
                                    "the AI read its pages as {} (the URLs suggested {})",
                                    kind.as_str(),
                                    section.kind.as_str()
                                );
                                section.kind = *kind;
                            }
                        }
                    }
                    (site_type, confidence, because) = classify_site(&classified, platform);
                    if let Some(ai_type) = answer.site_type.filter(|t| *t != site_type) {
                        because =
                            format!("the AI read the site as {} ({because})", ai_type.as_str());
                        site_type = ai_type;
                        confidence = confidence.max(0.6);
                    }
                    description = answer.description;
                    ai_summary_hint = answer.ai_summary;
                }
                Ok(None) => summary.warnings.push(
                    "No AI provider for this account: the classification is deterministic"
                        .to_string(),
                ),
                Err(e) => summary.warnings.push(format!(
                    "The AI classification failed ({e}): the deterministic one is used"
                )),
            }
        }

        // 6. Proposal.
        let findings = Findings {
            start_url: site.to_string(),
            host: host.clone(),
            authority,
            base_path: base_path.clone(),
            sections: classified.clone(),
            languages: languages.clone(),
            pages_found: under.len(),
            map_truncated: !count_known,
            query_share,
            js,
            pdf_links: linked(pages.iter().map(|p| &p.pdf_links)),
            document_links: linked(pages.iter().map(|p| &p.document_links)),
            ai_summary_hint,
        };
        let proposal = presets::propose(&findings);
        if let Err(e) =
            serde_json::from_value::<scrapix_core::config::CrawlConfig>(proposal.config.clone())
        {
            warn!(job_id = %self.job_id, error = %e, "The proposed config does not deserialize");
            return self
                .fail(&summary, "The proposed config is invalid (internal error)")
                .await;
        }
        summary.config = Some(proposal.config);
        summary.reasons = proposal.reasons;
        summary.estimate = Some(proposal.estimate);
        summary.warnings.extend(proposal.warnings);
        summary.profile = Some(SiteProfile {
            url: site.to_string(),
            base_path: base_path.clone(),
            site_type,
            confidence: (confidence * 100.0).round() / 100.0,
            because,
            platform,
            description,
            ai_classified,
            languages: languages.into_iter().collect(),
            sections: classified,
        });
        self.save(&summary).await;

        let failed = summary.samples.iter().filter(|s| !s.success).count() as u64;
        let succeeded = summary.samples.len() as u64 - failed;
        info!(
            job_id = %self.job_id,
            site = %site,
            site_type = site_type.as_str(),
            ai_classified,
            "Crawl config suggested"
        );
        engine_jobs::complete_job(&self.state, &self.job_id, succeeded, failed, started);
    }

    /// The site's pages from `/map` (billed as one).
    async fn map(
        &self,
        site: &url::Url,
        depth: u32,
        sitemap: bool,
    ) -> Result<Vec<MappedPage>, String> {
        let request = MapRequest {
            url: site.to_string(),
            limit: MAP_LIMIT,
            depth,
            search: None,
            render_js: false,
            sitemap,
            get_title: false,
            get_description: false,
            get_lastmod: false,
            get_priority: false,
            get_changefreq: false,
        };
        let ctx = engine_jobs::clone_account_ctx(self.account_ctx.as_ref());
        run_map(&self.state, &ctx, request)
            .await
            .map(|res| {
                res.links
                    .into_iter()
                    .map(|l| MappedPage {
                        url: l.url,
                        title: l.title,
                    })
                    .collect()
            })
            .map_err(|e| e.error)
    }

    /// The AI pass: `Ok(None)` when there is no AI service.
    async fn classify_with_ai(
        &self,
        site: &url::Url,
        platform: Option<SitePlatform>,
        sections: &[SiteSection],
        pages: &[PageSignals],
    ) -> Result<Option<ai::AiClassification>, String> {
        let Some(resolved) = resolve_ai(&self.state, self.account_ctx.as_ref().as_ref(), None)
            .await
            .map_err(|e| e.error)?
        else {
            return Ok(None);
        };
        if let (Some(lab), Some(ctx)) = (&self.state.lab_api, self.account_ctx.as_ref()) {
            billing::check_credits(lab, &ctx.account_id)
                .await
                .map_err(|e| e.error)?;
        }
        let samples: Vec<&PageSignals> = pages.iter().collect();
        let content = ai::content(site.as_str(), platform, sections, &samples);
        let attribution = AiUsageContext {
            job_id: self.job_id.clone(),
            account_id: self.account_id(),
            feature: "crawl_suggest".to_string(),
            url: site.to_string(),
        };
        let call = with_key_retry(
            &self.state,
            self.account_ctx.as_ref().as_ref(),
            resolved,
            |resolved_ai: ResolvedAi| {
                let service = self.classifier(&resolved_ai);
                let content = content.clone();
                async move { service.extract(&content, ai::INSTRUCTION).await }
            },
        );
        let result = AI_USAGE_CONTEXT
            .scope(attribution, call)
            .await
            .map_err(|e| match e {
                scrapix_core::ScrapixError::AiGateway { message, .. } => message,
                other => other.to_string(),
            })?;
        let keys: Vec<&str> = sections.iter().map(|s| s.key.as_str()).collect();
        Ok(Some(ai::parse_answer(&result.data, &keys)))
    }

    /// An extraction service on `ai`'s client with the requested model.
    fn classifier(&self, ai: &ResolvedAi) -> AiService {
        let base = ai
            .service
            .extractor()
            .map(|e| e.config().clone())
            .unwrap_or_default();
        let config = ExtractionConfig {
            model: self.model.clone().unwrap_or_else(|| base.model.clone()),
            max_content_tokens: AI_CONTEXT_TOKENS,
            ..base
        };
        AiService::minimal(ai.service.client().clone()).with_extraction_config(config)
    }
}

/// The platform two thirds of the sampled pages at least were built with
/// (a docs platform under `/docs` does not make the whole site a docs site).
fn site_platform(pages: &[PageSignals]) -> Option<SitePlatform> {
    let mut counts: Vec<(SitePlatform, usize)> = Vec::new();
    for platform in pages.iter().filter_map(|p| p.platform) {
        match counts.iter_mut().find(|(p, _)| *p == platform) {
            Some((_, n)) => *n += 1,
            None => counts.push((platform, 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .filter(|(_, n)| n * 3 >= pages.len() * 2)
        .map(|(platform, _)| platform)
}

/// Distinct document links seen on the sampled pages, counted only when
/// they are not a one-off (a single download link on one page, such as a
/// docs site's "download as EPUB"): on two pages, or three links at least.
fn linked<'a>(sets: impl Iterator<Item = &'a BTreeSet<String>>) -> usize {
    let mut pages = 0;
    let mut links = BTreeSet::new();
    for set in sets.filter(|s| !s.is_empty()) {
        pages += 1;
        links.extend(set.iter());
    }
    if pages >= 2 || links.len() >= 3 {
        links.len()
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn base_paths_are_directories() {
        assert_eq!(base_path(&url("https://a.test")), "/");
        assert_eq!(base_path(&url("https://a.test/")), "/");
        assert_eq!(base_path(&url("https://a.test/docs")), "/docs/");
        assert_eq!(base_path(&url("https://a.test/docs/")), "/docs/");
        assert_eq!(base_path(&url("https://a.test/index.html")), "/");
        assert_eq!(base_path(&url("https://a.test/en/start.php")), "/en/");
    }

    #[test]
    fn urls_are_validated_like_map() {
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("http://127.0.0.1/").is_err());
        assert!(validate_url("not a url").is_err());
    }

    fn section(key: &str, urls: &[&str]) -> analysis::Section {
        analysis::Section {
            key: key.to_string(),
            pages: urls.len(),
            titles: Vec::new(),
            urls: urls.iter().map(|u| u.to_string()).collect(),
        }
    }

    #[test]
    fn samples_go_round_robin_over_crawled_sections() {
        let sections = [
            section("/products/", &["p1", "p2", "p3", "p4"]),
            section("/blog/", &["b1", "b2"]),
            section("/cart/", &["c1", "c2"]),
        ];
        let already: HashSet<String> = ["p1".to_string()].into();
        let picks = pick_samples(&sections, 5, &already, false);
        let keys: Vec<&str> = picks.iter().map(|(_, k)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["/products/", "/blog/", "/products/", "/blog/", "/products/"]
        );
        assert!(!picks.iter().any(|(u, _)| u == "p1" || u.starts_with('c')));
        let distinct: HashSet<&String> = picks.iter().map(|(u, _)| u).collect();
        assert_eq!(distinct.len(), picks.len());
        // Fewer URLs than asked: all of them, once.
        assert_eq!(pick_samples(&sections, 50, &HashSet::new(), false).len(), 6);
    }

    #[test]
    fn the_site_platform_needs_two_thirds_of_the_pages() {
        let page = |p: Option<SitePlatform>| PageSignals {
            platform: p,
            ..Default::default()
        };
        let shop = [
            page(Some(SitePlatform::Woocommerce)),
            page(Some(SitePlatform::Woocommerce)),
            page(Some(SitePlatform::Woocommerce)),
            page(Some(SitePlatform::Wordpress)),
        ];
        assert_eq!(site_platform(&shop), Some(SitePlatform::Woocommerce));
        // Mintlify under `/docs` only.
        let mixed = [
            page(Some(SitePlatform::Mintlify)),
            page(None),
            page(None),
            page(None),
        ];
        assert_eq!(site_platform(&mixed), None);
        assert_eq!(site_platform(&[]), None);
    }

    #[test]
    fn one_download_link_does_not_turn_documents_on() {
        let set =
            |links: &[&str]| -> BTreeSet<String> { links.iter().map(|l| l.to_string()).collect() };
        let one = [set(&["a.epub"]), set(&[])];
        assert_eq!(linked(one.iter()), 0);
        let two_pages = [set(&["a.pdf"]), set(&["b.pdf"])];
        assert_eq!(linked(two_pages.iter()), 2);
        let three = [set(&["a.pdf", "b.pdf", "c.pdf"])];
        assert_eq!(linked(three.iter()), 3);
    }

    // ---- end to end, with a local shop and an OpenAI-compatible mock ----

    use crate::results::test_support::test_state_with_ai;
    use scrapix_ai::{AiClient, AiClientConfig};
    use scrapix_core::config::CrawlConfig;
    use scrapix_core::url_glob::matches_glob;
    use scrapix_core::JobStatus;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn html(title: &str, head: &str, body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(
            format!(
                "<html><head><title>{title}</title>{head}\
                 <script src=\"//cdn.shopify.com/s/files/theme.js\"></script></head>\
                 <body><main><h1>{title}</h1>{body}</main></body></html>"
            ),
            "text/html",
        )
    }

    /// A Shopify-like shop: products with `Product` JSON-LD, collections,
    /// long blog posts with headings, a linked PDF, account and legal pages.
    async fn shop() -> MockServer {
        let server = MockServer::start().await;
        let base = server.uri().replace("127.0.0.1", "localhost");
        let mut paths: Vec<String> = vec!["/".into()];
        paths.extend((1..=5).map(|i| format!("/products/shoe-{i}")));
        paths.extend(["/collections/men".into(), "/collections/women".into()]);
        paths.extend((1..=3).map(|i| format!("/blog/post-{i}")));
        paths.extend(["/account/login".into(), "/account/orders".into()]);
        paths.extend(["/legal/privacy".into(), "/legal/terms".into()]);
        let urls: String = paths
            .iter()
            .map(|p| format!("<url><loc>{base}{p}</loc></url>"))
            .collect();
        Mock::given(method("GET"))
            .and(path("/sitemap.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{urls}</urlset>"#
            )))
            .mount(&server)
            .await;

        let words = |n: usize| -> String { (0..n).map(|i| format!("word{i} ")).collect() };
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(
                "Shoe Shop",
                "",
                &format!(
                    "<p>{}</p><a href=\"/files/catalog.pdf\">Catalog</a><a href=\"/files/sizes.pdf\">Sizes</a>\
                     <a href=\"/files/care.pdf\">Care</a><a href=\"/products/shoe-1\">Shoe</a>",
                    words(120)
                ),
            ))
            .mount(&server)
            .await;
        for i in 1..=5 {
            let json_ld = format!(
                r#"<script type="application/ld+json">{{"@context":"https://schema.org","@type":"Product","name":"Shoe {i}","offers":{{"@type":"Offer","price":"{i}0.00","priceCurrency":"EUR"}}}}</script>"#
            );
            Mock::given(method("GET"))
                .and(path(format!("/products/shoe-{i}")))
                .respond_with(html(
                    &format!("Shoe {i}"),
                    &json_ld,
                    &format!("<p>{}</p>", words(90)),
                ))
                .mount(&server)
                .await;
        }
        for name in ["men", "women"] {
            Mock::given(method("GET"))
                .and(path(format!("/collections/{name}")))
                .respond_with(html(name, "", &format!("<p>{}</p>", words(60))))
                .mount(&server)
                .await;
        }
        for i in 1..=3 {
            let body: String = (1..=3)
                .map(|h| format!("<h2>Part {h}</h2><p>{}</p>", words(200)))
                .collect();
            Mock::given(method("GET"))
                .and(path(format!("/blog/post-{i}")))
                .respond_with(html(&format!("Post {i}"), "", &body))
                .mount(&server)
                .await;
        }
        server
    }

    async fn llm(reply: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1,
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": reply.to_string() },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    fn ai_service(llm: &MockServer) -> Arc<AiService> {
        let client = AiClient::new(AiClientConfig {
            api_key: "test".into(),
            provider: "openai".into(),
            base_url: Some(format!("{}/v1", llm.uri())),
            max_retries: 1,
            retry_delay_ms: 10,
            ..Default::default()
        })
        .unwrap();
        Arc::new(AiService::new(Arc::new(client)))
    }

    async fn wait_terminal(state: &AppState, job_id: &str) -> scrapix_core::JobState {
        for _ in 0..1200 {
            let job = state.get_job(job_id).unwrap();
            if crate::is_terminal(&job.status) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("suggest job did not finish");
    }

    async fn suggest(state: &Arc<AppState>, body: serde_json::Value) -> SuggestStatusResponse {
        let request: SuggestRequest = serde_json::from_value(body).unwrap();
        let created = start_suggest(state, &None, request).await.unwrap();
        let job = wait_terminal(state, &created.job_id).await;
        let response = get_suggest(State(state.clone()), None, Path(created.job_id))
            .await
            .unwrap()
            .0;
        assert_eq!(job.status, JobStatus::Completed, "{:?}", response.error);
        response
    }

    #[tokio::test]
    async fn a_shop_gets_a_scoped_config_with_reasons() {
        let site = shop().await;
        let base = site.uri().replace("127.0.0.1", "localhost");
        let llm = llm(serde_json::json!({
            "site_type": "ecommerce",
            "sections": {"/collections/": "listing", "/blog/": "article"},
            "ai_summary": false,
            "description": "An online shoe shop."
        }))
        .await;
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state_with_ai(&bus, Some(ai_service(&llm)));

        let response = suggest(&state, serde_json::json!({ "url": format!("{base}/") })).await;

        let profile = response.profile.as_ref().unwrap();
        assert_eq!(profile.site_type, SiteType::Ecommerce);
        assert_eq!(profile.platform, Some(SitePlatform::Shopify));
        assert!(profile.ai_classified);
        assert_eq!(profile.description.as_deref(), Some("An online shoe shop."));
        let kind = |key: &str| {
            profile
                .sections
                .iter()
                .find(|s| s.key == key)
                .unwrap_or_else(|| panic!("no section {key}"))
                .kind
        };
        assert_eq!(kind("/products/"), SectionKind::Product);
        assert_eq!(kind("/collections/"), SectionKind::Listing);
        assert_eq!(kind("/blog/"), SectionKind::Article);
        assert_eq!(kind("/account/"), SectionKind::Account);
        assert_eq!(kind("/legal/"), SectionKind::Legal);

        let config: CrawlConfig = serde_json::from_value(response.config.clone().unwrap()).unwrap();
        assert_eq!(config.crawler_type, scrapix_core::config::CrawlerType::Http);
        let exclude = &config.url_patterns.exclude;
        for url in [
            "/account/login",
            "/legal/privacy",
            "/collections/men?sort=price",
        ] {
            let url = format!("{base}{url}");
            assert!(
                exclude.iter().any(|g| matches_glob(&url, g)),
                "{url} not excluded"
            );
        }
        for url in ["/products/shoe-1", "/blog/post-1", "/collections/men"] {
            let url = format!("{base}{url}");
            assert!(
                !exclude.iter().any(|g| matches_glob(&url, g)),
                "{url} excluded"
            );
        }
        // Product schema on products; block split on the long blog posts
        // only; the AI said no to summaries; the catalog PDF is indexed.
        let schema = config.features.schema.as_ref().unwrap();
        assert!(schema.only_types.contains(&"Product".to_string()));
        assert!(schema
            .include_pages
            .iter()
            .any(|g| matches_glob(&format!("{base}/products/shoe-3"), g)));
        let blocks = config.features.block_split.as_ref().unwrap();
        assert!(blocks
            .include_pages
            .iter()
            .any(|g| matches_glob(&format!("{base}/blog/post-2"), g)));
        assert!(!blocks
            .include_pages
            .iter()
            .any(|g| matches_glob(&format!("{base}/products/shoe-2"), g)));
        assert!(config.features.ai_summary.is_none());
        assert!(config.features.is_pdf_enabled());
        assert!(config.max_pages.is_some());

        assert_eq!(response.samples.len(), DEFAULT_SAMPLES);
        assert!(response.samples.iter().all(|s| s.success));
        assert!(!response
            .samples
            .iter()
            .any(|s| s.url.contains("/account/") || s.url.contains("/legal/")));
        for field in [
            "crawler_type",
            "features.schema",
            "features.block_split",
            "features.pdf",
        ] {
            assert!(
                response.reasons.iter().any(|r| r.field == field),
                "no reason for {field}"
            );
        }
        assert!(!response.ai_usage.is_empty());
    }

    #[tokio::test]
    async fn without_ai_the_deterministic_classification_is_used() {
        let site = shop().await;
        let base = site.uri().replace("127.0.0.1", "localhost");
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state_with_ai(&bus, None);

        let response = suggest(
            &state,
            serde_json::json!({ "url": format!("{base}/"), "max_samples": 4 }),
        )
        .await;

        let profile = response.profile.as_ref().unwrap();
        assert!(!profile.ai_classified);
        assert_eq!(profile.site_type, SiteType::Ecommerce);
        assert_eq!(response.samples.len(), 4);
        assert!(response
            .warnings
            .iter()
            .any(|w| w.contains("deterministic")));
        assert!(response.config.is_some());
    }

    /// A tourism site in French, English and German with translated
    /// section slugs (nous-larochelle.fr), suggested from its English root:
    /// the crawl covers every language, not `/en/**` only.
    #[tokio::test]
    async fn a_language_root_of_a_multilingual_site_covers_every_language() {
        let server = MockServer::start().await;
        let base = server.uri().replace("127.0.0.1", "localhost");
        let mut paths: Vec<String> = vec!["/fr".into(), "/en".into(), "/de".into()];
        for (language, section) in [
            ("fr", "information-touristique"),
            ("en", "turistic-information"),
            ("de", "touristen-information"),
        ] {
            paths.extend((1..=4).map(|i| format!("/{language}/{section}/hotel-{i}")));
        }
        let urls: String = paths
            .iter()
            .map(|p| format!("<url><loc>{base}{p}</loc></url>"))
            .collect();
        Mock::given(method("GET"))
            .and(path("/sitemap.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{urls}</urlset>"#
            )))
            .mount(&server)
            .await;
        let words = |n: usize| -> String { (0..n).map(|i| format!("word{i} ")).collect() };
        for p in &paths {
            Mock::given(method("GET"))
                .and(path(p.as_str()))
                .respond_with(html(p, "", &format!("<p>{}</p>", words(120))))
                .mount(&server)
                .await;
        }
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state_with_ai(&bus, None);

        let response = suggest(
            &state,
            serde_json::json!({ "url": format!("{base}/en"), "max_samples": 3 }),
        )
        .await;

        let profile = response.profile.as_ref().unwrap();
        assert_eq!(profile.base_path, "/");
        assert_eq!(profile.languages, ["de", "en", "fr"]);
        let keys: Vec<&str> = profile.sections.iter().map(|s| s.key.as_str()).collect();
        assert!(keys.contains(&"/information-touristique/"), "{keys:?}");
        assert!(keys.contains(&"/touristen-information/"), "{keys:?}");
        assert!(
            response
                .warnings
                .iter()
                .any(|w| w.contains("`/en/` is one language of a site in 3 (de, en, fr)")),
            "{:?}",
            response.warnings
        );
        let config: CrawlConfig = serde_json::from_value(response.config.clone().unwrap()).unwrap();
        assert!(
            config.url_patterns.include.is_empty(),
            "{:?}",
            config.url_patterns.include
        );
        assert_eq!(config.start_urls, [format!("{base}/en")]);
        // Every language's pages are counted for the page cap.
        assert!(config.max_pages.unwrap() >= paths.len() as u64);
    }

    /// A JavaScript app with no browser on the server: the shells are
    /// enough to propose browser rendering, with a warning to verify it.
    #[tokio::test]
    async fn an_app_shell_site_gets_browser_rendering_with_a_warning() {
        let server = MockServer::start().await;
        let base = server.uri().replace("127.0.0.1", "localhost");
        let shell = ResponseTemplate::new(200).set_body_raw(
            "<html><head><title>App</title><script src=\"/main.js\"></script></head>\
             <body><noscript>You need to enable JavaScript to run this app.</noscript>\
             <div id=\"root\"></div></body></html>",
            "text/html",
        );
        Mock::given(method("GET"))
            .and(path("/sitemap.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>{base}/</loc></url><url><loc>{base}/features/a</loc></url><url><loc>{base}/features/b</loc></url></urlset>"#
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(shell)
            .mount(&server)
            .await;
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state_with_ai(&bus, None);

        let response = suggest(&state, serde_json::json!({ "url": format!("{base}/") })).await;

        let config = response.config.as_ref().unwrap();
        assert_eq!(config["crawler_type"], "browser");
        assert!(response
            .warnings
            .iter()
            .any(|w| w.contains("not rendered to confirm")));
        assert!(response
            .reasons
            .iter()
            .any(|r| r.field == "crawler_type" && r.reason.contains("app shells")));
        assert!(response.estimate.as_ref().unwrap().js_rendering);
    }

    #[tokio::test]
    async fn bad_requests_are_refused_before_any_job() {
        let bus = scrapix_queue::ChannelBus::new();
        let state = test_state_with_ai(&bus, None);
        for body in [
            serde_json::json!({ "url": "ftp://a.test" }),
            serde_json::json!({ "url": "https://a.test", "max_samples": 0 }),
            serde_json::json!({ "url": "https://a.test", "max_samples": 21 }),
        ] {
            let request: SuggestRequest = serde_json::from_value(body).unwrap();
            let err = start_suggest(&state, &None, request).await.unwrap_err();
            assert_eq!(err.code, "validation_error");
        }
        assert!(state.list_jobs(10, 0).is_empty());
    }
}
