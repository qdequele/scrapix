//! Lightpanda renderer: plain crawl renders over CDP on a running
//! `lightpanda serve`.
//!
//! [Lightpanda](https://github.com/lightpanda-io/browser) is a headless
//! browser that runs JavaScript and builds the DOM but never lays out or
//! paints the page, so a render costs a fraction of Chrome's time and
//! memory. It does not replace [`CdpRenderer`](crate::CdpRenderer): no real
//! screenshots, no touch/mobile emulation, no fingerprint hardening, and
//! weaker web-API coverage. It is meant for crawl renders (a URL, a user
//! agent, headers), with Chrome taking over when Lightpanda fails or the
//! page looks unrendered ([`LightpandaRender::fallback_reason`]).
//!
//! ## Connections
//!
//! Lightpanda 1.0 serves one browser context, and one page, per CDP
//! connection, and many connections per server (`--cdp-max-connections`,
//! default 16). The renderer keeps a pool of up to
//! [`LightpandaConfig::max_connections`] connections and renders one page
//! at a time on each. A connection that fails a render is dropped, not
//! reused.
//!
//! ## Security
//!
//! The same checks as the Chrome renderer run on every render: SSRF check
//! of the target before navigating, robots.txt, the link-local URL block,
//! and the SSRF check of the URL the page ended on. Lightpanda resolves DNS
//! itself, so also run it with `--block-private-networks`, which refuses
//! non-routable addresses for every request the page makes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::network::{Headers, SetExtraHttpHeadersParams};
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::types::{Command, Method, MethodId};
use chromiumoxide::Page;
use futures::StreamExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::{debug, instrument, warn};

use scrapix_core::{CrawlUrl, RawPage, Result, ScrapixError};

use crate::fetcher::FetchOptions;
use crate::renderer_cdp::{
    check_render_final_url, check_render_target, document_content_type, document_status,
    is_main_document, response_headers, CdpRenderer, RenderResult, BROWSER_PROXY_UNSUPPORTED,
    LINK_LOCAL_URL_PATTERNS,
};
use crate::robots::RobotsCache;

/// Fewest visible text characters (outside `<script>`, `<style>`, ...) a
/// Lightpanda render must have to be kept; below this the page most likely
/// needs a full browser and is re-rendered by Chrome.
pub const DEFAULT_MIN_TEXT_CHARS: usize = 64;

/// Configuration for [`LightpandaRenderer`].
#[derive(Debug, Clone)]
pub struct LightpandaConfig {
    /// CDP endpoint of a running `lightpanda serve`: a `ws://`/`wss://`
    /// URL, or an `http://`/`https://` URL whose `/json/version` names it.
    pub cdp_url: String,
    /// Connections (so pages rendering at once) to keep open. Keep it at
    /// or below the server's `--cdp-max-connections`.
    pub max_connections: usize,
    /// Navigation timeout of one render.
    pub timeout: Duration,
    /// Allow private targets (tests only; link-local and metadata addresses
    /// stay refused).
    pub allow_private_ips: bool,
    /// See [`DEFAULT_MIN_TEXT_CHARS`].
    pub min_text_chars: usize,
}

impl Default for LightpandaConfig {
    fn default() -> Self {
        Self {
            cdp_url: "ws://127.0.0.1:9222".to_string(),
            max_connections: 8,
            timeout: Duration::from_secs(30),
            allow_private_ips: false,
            min_text_chars: DEFAULT_MIN_TEXT_CHARS,
        }
    }
}

/// `LP.getPageStatus`: Lightpanda's own summary of the page it just loaded.
/// Every field is optional so a newer or older server never fails a render.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PageStatus {
    /// HTTP status of the main document.
    pub http_status: Option<u16>,
    /// A bot challenge (Cloudflare, ...) Lightpanda recognized on the page.
    pub bot_challenge: Option<serde_json::Value>,
    /// Why the navigation failed, if it did.
    pub navigate_error: Option<String>,
    /// Uncaught JavaScript errors and unhandled rejections.
    pub js_errors: u64,
    /// Web APIs the page called that Lightpanda does not implement, by API.
    pub unsupported: serde_json::Map<String, serde_json::Value>,
    /// Globals the page read that Lightpanda does not define.
    pub missing_globals: serde_json::Value,
}

impl PageStatus {
    /// Number of distinct missing globals (an array or an object of them).
    fn missing_globals_count(&self) -> usize {
        match &self.missing_globals {
            serde_json::Value::Array(a) => a.len(),
            serde_json::Value::Object(o) => o.len(),
            _ => 0,
        }
    }
}

/// The `LP.getPageStatus` command (not part of chromiumoxide's generated
/// protocol).
#[derive(Debug, Clone, Serialize)]
struct GetPageStatus {}

impl Method for GetPageStatus {
    fn identifier(&self) -> MethodId {
        "LP.getPageStatus".into()
    }
}

impl Command for GetPageStatus {
    type Response = PageStatus;
}

/// `Network.setBlockedURLs` in the form Lightpanda implements: the newer
/// CDP `urlPatterns: [{urlPattern, block}]`. chromiumoxide 0.7 only sends
/// the older `urls` list, which Lightpanda refuses (`MissingField`), so the
/// Chrome renderer's `block_link_local_urls` never applied here.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetBlockedUrlPatterns {
    url_patterns: Vec<BlockedUrlPattern>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockedUrlPattern {
    url_pattern: String,
    block: bool,
}

impl SetBlockedUrlPatterns {
    fn blocking<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            url_patterns: patterns
                .into_iter()
                .map(|p| BlockedUrlPattern {
                    url_pattern: p.to_string(),
                    block: true,
                })
                .collect(),
        }
    }
}

impl Method for SetBlockedUrlPatterns {
    fn identifier(&self) -> MethodId {
        "Network.setBlockedURLs".into()
    }
}

impl Command for SetBlockedUrlPatterns {
    type Response = serde_json::Value;
}

/// Bar `page` from [`LINK_LOCAL_URL_PATTERNS`] (link-local and cloud
/// metadata IP literals), as the Chrome renderer does. A failure is logged,
/// not fatal: the navigation checks still run on every render, and
/// Lightpanda's `--block-private-networks` covers these ranges too.
async fn block_link_local_urls(page: &Page) {
    let params = SetBlockedUrlPatterns::blocking(LINK_LOCAL_URL_PATTERNS.iter().copied());
    if let Err(e) = page.execute(params).await {
        warn!(error = %e, "Could not block link-local URLs on the Lightpanda page");
    }
}

/// One Lightpanda render: the page, plus what Chrome fallback needs.
#[derive(Debug)]
pub struct LightpandaRender {
    pub result: RenderResult,
    /// Lightpanda's page summary (`None` if the server did not answer it).
    pub status: Option<PageStatus>,
    /// Visible text characters in the rendered HTML.
    pub text_chars: usize,
    min_text_chars: usize,
}

impl LightpandaRender {
    /// Why this render should be redone in Chrome, or `None` to keep it.
    pub fn fallback_reason(&self) -> Option<&'static str> {
        if let Some(status) = &self.status {
            if status.bot_challenge.as_ref().is_some_and(|c| !c.is_null()) {
                return Some("bot challenge");
            }
            if status.navigate_error.is_some() {
                return Some("navigation error");
            }
        }
        // Error pages are kept: Chrome would get the same status.
        if !(200..300).contains(&self.result.status) {
            return None;
        }
        if self.text_chars < self.min_text_chars {
            return Some("page has almost no text");
        }
        if let Some(status) = &self.status {
            if status.js_errors > 0 && status.missing_globals_count() > 0 {
                return Some("scripts failed on missing web APIs");
            }
        }
        None
    }

    /// The crawler's `RawPage` for this render.
    pub fn raw_page(self, url: &CrawlUrl) -> RawPage {
        CdpRenderer::raw_page(url, self.result)
    }
}

/// Whether a failed Lightpanda render should be retried in Chrome. Refusals
/// (SSRF, robots.txt) and configuration errors are final.
pub fn should_retry_in_chrome(err: &ScrapixError) -> bool {
    !matches!(
        err,
        ScrapixError::Refused(_) | ScrapixError::RobotsDisallowed { .. } | ScrapixError::Config(_)
    )
}

/// Visible text characters (non-whitespace) of `html`, outside `<script>`,
/// `<style>`, `<noscript>` and `<template>`.
pub fn visible_text_chars(html: &str) -> usize {
    let doc = scraper::Html::parse_document(html);
    doc.tree
        .nodes()
        .filter_map(|node| {
            let text = node.value().as_text()?;
            let hidden = node.ancestors().any(|a| {
                a.value().as_element().is_some_and(|e| {
                    matches!(e.name(), "script" | "style" | "noscript" | "template")
                })
            });
            (!hidden).then(|| text.chars().filter(|c| !c.is_whitespace()).count())
        })
        .sum()
}

/// One open CDP connection.
struct Connection {
    browser: Browser,
    handler: JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Closes the websocket: the server drops this connection's context.
        self.handler.abort();
    }
}

/// Renders pages on a running Lightpanda server.
pub struct LightpandaRenderer {
    config: LightpandaConfig,
    ws_url: String,
    idle: Mutex<Vec<Connection>>,
    slots: Arc<Semaphore>,
    robots_cache: Option<Arc<RobotsCache>>,
}

impl LightpandaRenderer {
    /// Resolve the CDP endpoint and open one connection to check the server
    /// answers (it stays in the pool).
    pub async fn new(
        config: LightpandaConfig,
        robots_cache: Option<Arc<RobotsCache>>,
    ) -> Result<Self> {
        if config.max_connections == 0 {
            return Err(ScrapixError::Config(
                "Lightpanda max_connections must be at least 1".to_string(),
            ));
        }
        let ws_url = resolve_ws_url(&config.cdp_url).await?;
        let renderer = Self {
            slots: Arc::new(Semaphore::new(config.max_connections)),
            config,
            ws_url,
            idle: Mutex::new(Vec::new()),
            robots_cache,
        };
        let first = renderer.connect().await?;
        renderer.idle.lock().push(first);
        Ok(renderer)
    }

    /// The configuration.
    pub fn config(&self) -> &LightpandaConfig {
        &self.config
    }

    async fn connect(&self) -> Result<Connection> {
        let (browser, mut handler) =
            Browser::connect_with_config(self.ws_url.clone(), HandlerConfig::default())
                .await
                .map_err(|e| {
                    ScrapixError::Connection(format!("Lightpanda CDP connection failed: {e}"))
                })?;
        let handler = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(e) = event {
                    debug!(error = %e, "Lightpanda handler error");
                }
            }
        });
        Ok(Connection { browser, handler })
    }

    /// Render a crawl URL with the job's user agent and headers.
    pub async fn fetch_with_options(
        &self,
        url: &CrawlUrl,
        options: &FetchOptions,
    ) -> Result<LightpandaRender> {
        if options.proxy.is_some() {
            return Err(ScrapixError::Config(BROWSER_PROXY_UNSUPPORTED.to_string()));
        }
        self.render(
            &url.url,
            options.user_agent.as_deref(),
            &options.extra_headers,
            options.respect_robots,
        )
        .await
    }

    /// Render `url` after the SSRF and (unless `respect_robots` is
    /// `Some(false)`) robots.txt checks.
    #[instrument(skip(self, extra_headers), fields(url = %url))]
    pub async fn render(
        &self,
        url: &str,
        user_agent: Option<&str>,
        extra_headers: &[(String, String)],
        respect_robots: Option<bool>,
    ) -> Result<LightpandaRender> {
        let robots = if respect_robots == Some(false) {
            None
        } else {
            self.robots_cache.as_deref()
        };
        check_render_target(url, self.config.allow_private_ips, robots).await?;

        let _slot = self
            .slots
            .acquire()
            .await
            .map_err(|_| ScrapixError::Crawl("Lightpanda renderer closed".to_string()))?;
        let start = Instant::now();

        let pooled = self.idle.lock().pop();
        let conn = match pooled {
            Some(conn) => conn,
            None => self.connect().await?,
        };

        let page = match conn.browser.new_page("about:blank").await {
            Ok(page) => page,
            Err(e) => {
                // A pooled connection the server closed: retry once fresh.
                drop(conn);
                warn!(error = %e, "Lightpanda connection unusable; reconnecting");
                let conn = self.connect().await?;
                let page = conn.browser.new_page("about:blank").await.map_err(|e| {
                    ScrapixError::Crawl(format!("Lightpanda could not open a page: {e}"))
                })?;
                return self
                    .finish(conn, page, url, user_agent, extra_headers, start)
                    .await;
            }
        };
        self.finish(conn, page, url, user_agent, extra_headers, start)
            .await
    }

    /// Render on `page`, close it, and return `conn` to the pool if the
    /// render succeeded.
    async fn finish(
        &self,
        conn: Connection,
        page: Page,
        url: &str,
        user_agent: Option<&str>,
        extra_headers: &[(String, String)],
        start: Instant,
    ) -> Result<LightpandaRender> {
        let result = tokio::time::timeout(
            // Navigation is bounded by `timeout`; leave room for the rest.
            self.config.timeout + Duration::from_secs(5),
            self.render_on_page(&page, url, user_agent, extra_headers, start),
        )
        .await
        .unwrap_or_else(|_| {
            Err(ScrapixError::Timeout(format!(
                "Lightpanda render took longer than {} s",
                self.config.timeout.as_secs() + 5
            )))
        });
        let closed = page.close().await.is_ok();
        if result.is_ok() && closed {
            self.idle.lock().push(conn);
        }
        result
    }

    async fn render_on_page(
        &self,
        page: &Page,
        url: &str,
        user_agent: Option<&str>,
        extra_headers: &[(String, String)],
        start: Instant,
    ) -> Result<LightpandaRender> {
        block_link_local_urls(page).await;

        if let Some(ua) = user_agent {
            page.set_user_agent(ua.to_string())
                .await
                .map_err(|e| ScrapixError::Crawl(format!("Lightpanda set user agent: {e}")))?;
        }
        if !extra_headers.is_empty() {
            let headers: serde_json::Map<String, serde_json::Value> = extra_headers
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            page.execute(SetExtraHttpHeadersParams::new(Headers::new(
                serde_json::Value::Object(headers),
            )))
            .await
            .map_err(|e| ScrapixError::Crawl(format!("Lightpanda set extra headers: {e}")))?;
        }

        let timeout = self.config.timeout;
        let navigation = tokio::time::timeout(timeout, async {
            page.goto(url).await?;
            page.wait_for_navigation_response().await
        })
        .await
        .map_err(|_| ScrapixError::Timeout("Lightpanda page load timeout".to_string()))?
        .map_err(|e| ScrapixError::Crawl(format!("Lightpanda navigation failed: {e}")))?;

        let final_url = page
            .url()
            .await
            .map_err(|e| ScrapixError::Crawl(format!("Lightpanda page URL: {e}")))?
            .unwrap_or_else(|| url.to_string());
        check_render_final_url(&final_url, url, self.config.allow_private_ips).await?;

        let html = page
            .content()
            .await
            .map_err(|e| ScrapixError::Crawl(format!("Lightpanda page content: {e}")))?;

        let status = match page.execute(GetPageStatus {}).await {
            Ok(resp) => Some(resp.result),
            Err(e) => {
                debug!(error = %e, "LP.getPageStatus unavailable");
                None
            }
        };

        let response = navigation
            .as_deref()
            .filter(|r| is_main_document(r))
            .and_then(|r| r.response.as_ref());
        let http_status = document_status(response.map(|r| r.status))
            .or_else(|| status.as_ref().and_then(|s| s.http_status))
            .filter(|s| (100..=599).contains(s))
            .unwrap_or(200);
        let headers = response
            .map(|r| response_headers(r.headers.inner()))
            .unwrap_or_default();
        let content_type = headers
            .get("content-type")
            .cloned()
            .or_else(|| {
                response
                    .map(|r| r.mime_type.clone())
                    .filter(|m| !m.is_empty())
            })
            .or(document_content_type(page).await)
            .or_else(|| Some("text/html".to_string()));

        let text_chars = visible_text_chars(&html);
        let render_duration = start.elapsed();
        debug!(
            duration_ms = render_duration.as_millis(),
            status = http_status,
            text_chars,
            "Lightpanda page rendered"
        );

        Ok(LightpandaRender {
            result: RenderResult {
                html,
                final_url,
                status: http_status,
                headers,
                content_type,
                screenshot: None,
                javascript_returns: Vec::new(),
                console_logs: Vec::new(),
                js_errors: Vec::new(),
                render_duration,
            },
            status,
            text_chars,
            min_text_chars: self.config.min_text_chars,
        })
    }
}

/// The websocket URL for `cdp_url`: as is for `ws(s)://`, from
/// `/json/version` for `http(s)://`.
async fn resolve_ws_url(cdp_url: &str) -> Result<String> {
    let parsed = url::Url::parse(cdp_url)
        .map_err(|e| ScrapixError::Config(format!("invalid Lightpanda URL {cdp_url:?}: {e}")))?;
    match parsed.scheme() {
        "ws" | "wss" => Ok(cdp_url.to_string()),
        "http" | "https" => {
            #[derive(Deserialize)]
            struct Version {
                #[serde(rename = "webSocketDebuggerUrl")]
                ws: String,
            }
            let version_url = parsed.join("/json/version")?;
            let version: Version = reqwest::Client::new()
                .get(version_url)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map_err(|e| ScrapixError::Connection(format!("Lightpanda unreachable: {e}")))?
                .error_for_status()
                .map_err(|e| ScrapixError::Connection(format!("Lightpanda /json/version: {e}")))?
                .json()
                .await
                .map_err(|e| ScrapixError::Connection(format!("Lightpanda /json/version: {e}")))?;
            Ok(version.ws)
        }
        other => Err(ScrapixError::Config(format!(
            "Lightpanda URL must be ws://, wss://, http:// or https://, not {other}://"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn render(status: u16, text_chars: usize, page_status: Option<PageStatus>) -> LightpandaRender {
        LightpandaRender {
            result: RenderResult {
                html: String::new(),
                final_url: "https://example.com/".to_string(),
                status,
                headers: HashMap::new(),
                content_type: Some("text/html".to_string()),
                screenshot: None,
                javascript_returns: Vec::new(),
                console_logs: Vec::new(),
                js_errors: Vec::new(),
                render_duration: Duration::ZERO,
            },
            status: page_status,
            text_chars,
            min_text_chars: DEFAULT_MIN_TEXT_CHARS,
        }
    }

    #[test]
    fn blocked_urls_use_lightpandas_url_patterns_shape() {
        let cmd = SetBlockedUrlPatterns::blocking(["*://169.254.*", "*://[fe8*"]);
        assert_eq!(cmd.identifier(), "Network.setBlockedURLs");
        assert_eq!(
            serde_json::to_value(&cmd).unwrap(),
            serde_json::json!({"urlPatterns": [
                {"urlPattern": "*://169.254.*", "block": true},
                {"urlPattern": "*://[fe8*", "block": true}
            ]})
        );
        let all = SetBlockedUrlPatterns::blocking(LINK_LOCAL_URL_PATTERNS.iter().copied());
        assert_eq!(all.url_patterns.len(), LINK_LOCAL_URL_PATTERNS.len());
    }

    /// Against a real Lightpanda (`SCRAPIX_TEST_LIGHTPANDA`, skipped
    /// otherwise): the server accepts the command (it refused chromiumoxide's
    /// `urls` form) and a page's requests to a blocked URL never leave.
    #[tokio::test]
    async fn lightpanda_accepts_and_enforces_blocked_url_patterns() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let Some(cdp_url) = std::env::var("SCRAPIX_TEST_LIGHTPANDA")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("SCRAPIX_TEST_LIGHTPANDA not set; skipping Lightpanda test");
            return;
        };
        let server = MockServer::start().await;
        // The same page fetches an allowed and a blocked URL: the allowed one
        // proves fetch works, so "blocked" comes from the pattern.
        let page_html = r#"<html><body><p id="open">pending</p><p id="out">pending</p><script>
            const mark = (id, v) => { document.getElementById(id).textContent = v; };
            fetch('/open-probe').then(() => mark('open', 'open-reached'), () => mark('open', 'open-failed'));
            fetch('/blocked-probe').then(() => mark('out', 'reached'), () => mark('out', 'blocked'));
            </script></body></html>"#;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page_html, "text/html"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/open-probe"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/blocked-probe"))
            .respond_with(ResponseTemplate::new(200).set_body_string("secret"))
            .expect(0)
            .mount(&server)
            .await;
        let base = server.uri().replace("127.0.0.1", "localhost");

        let (browser, mut handler) = Browser::connect(resolve_ws_url(&cdp_url).await.unwrap())
            .await
            .unwrap();
        let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
        let page = browser.new_page("about:blank").await.unwrap();
        page.execute(SetBlockedUrlPatterns::blocking(["*/blocked-probe*"]))
            .await
            .expect("Lightpanda accepts urlPatterns");
        page.goto(format!("{base}/page")).await.unwrap();
        let _ = page.wait_for_navigation_response().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let html = page.content().await.unwrap();
        assert!(
            html.contains(">open-reached<"),
            "unblocked fetch failed: {html}"
        );
        assert!(html.contains(">blocked<"), "fetch was not refused: {html}");
        let _ = page.close().await;
        handler.abort();
        // `expect` counts (1 open, 0 blocked) are checked when the server drops.
    }

    #[test]
    fn keeps_a_page_with_text() {
        assert_eq!(render(200, 500, None).fallback_reason(), None);
        assert_eq!(
            render(200, 500, Some(PageStatus::default())).fallback_reason(),
            None
        );
    }

    #[test]
    fn falls_back_on_an_empty_page() {
        assert_eq!(
            render(200, 10, None).fallback_reason(),
            Some("page has almost no text")
        );
    }

    #[test]
    fn keeps_error_pages() {
        // A 404 with little text is what Chrome would get too.
        assert_eq!(render(404, 0, None).fallback_reason(), None);
    }

    #[test]
    fn falls_back_on_bot_challenge_and_navigation_error() {
        let challenge = PageStatus {
            bot_challenge: Some(serde_json::json!("cloudflare")),
            ..Default::default()
        };
        assert_eq!(
            render(403, 500, Some(challenge)).fallback_reason(),
            Some("bot challenge")
        );
        let nav = PageStatus {
            navigate_error: Some("ConnectionRefused".to_string()),
            ..Default::default()
        };
        assert_eq!(
            render(200, 500, Some(nav)).fallback_reason(),
            Some("navigation error")
        );
    }

    #[test]
    fn falls_back_when_scripts_fail_on_missing_globals() {
        let broken = PageStatus {
            js_errors: 2,
            missing_globals: serde_json::json!(["ResizeObserverEntry"]),
            ..Default::default()
        };
        assert_eq!(
            render(200, 500, Some(broken)).fallback_reason(),
            Some("scripts failed on missing web APIs")
        );
        // JS errors alone (analytics, ...) are common in Chrome too.
        let noisy = PageStatus {
            js_errors: 3,
            ..Default::default()
        };
        assert_eq!(render(200, 500, Some(noisy)).fallback_reason(), None);
    }

    #[test]
    fn page_status_parses_lightpanda_reply_and_tolerates_unknown_fields() {
        let reply = serde_json::json!({
            "url": "https://example.com/",
            "httpStatus": 200,
            "botChallenge": null,
            "navigateError": null,
            "jsErrors": 1,
            "unsupported": {"webgl": 2},
            "skipped": {"iframe": 1},
            "missingGlobals": [],
            "failed": {"fetch": 0},
            "blocked": {"fetch": 0},
            "resourceChallenge": null,
            "somethingNew": true
        });
        let status: PageStatus = serde_json::from_value(reply).unwrap();
        assert_eq!(status.http_status, Some(200));
        assert_eq!(status.js_errors, 1);
        assert!(status.bot_challenge.is_none());
        assert_eq!(status.missing_globals_count(), 0);
        let empty: PageStatus = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty, PageStatus::default());
    }

    #[test]
    fn counts_only_visible_text() {
        let html = r#"<html><head><title>T</title><style>body{color:red}</style>
            <script>var lots = "of script text that is not content";</script></head>
            <body><div id="root"></div><noscript>Enable JavaScript</noscript>
            <template><p>hidden</p></template></body></html>"#;
        // Only the <title> "T".
        assert_eq!(visible_text_chars(html), 1);
        assert_eq!(
            visible_text_chars("<body><p>Hello world</p></body>"),
            "Helloworld".len()
        );
    }

    #[test]
    fn refusals_are_not_retried_in_chrome() {
        assert!(!should_retry_in_chrome(&ScrapixError::Refused("x".into())));
        assert!(!should_retry_in_chrome(&ScrapixError::RobotsDisallowed {
            url: "u".into()
        }));
        assert!(!should_retry_in_chrome(&ScrapixError::Config("x".into())));
        assert!(should_retry_in_chrome(&ScrapixError::Timeout("x".into())));
        assert!(should_retry_in_chrome(&ScrapixError::Crawl("x".into())));
        assert!(should_retry_in_chrome(&ScrapixError::Connection(
            "x".into()
        )));
    }

    #[tokio::test]
    async fn rejects_bad_urls_and_zero_connections() {
        assert!(matches!(
            resolve_ws_url("ftp://x").await,
            Err(ScrapixError::Config(_))
        ));
        assert_eq!(
            resolve_ws_url("ws://127.0.0.1:9222").await.unwrap(),
            "ws://127.0.0.1:9222"
        );
        let err = LightpandaRenderer::new(
            LightpandaConfig {
                max_connections: 0,
                ..Default::default()
            },
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, ScrapixError::Config(_)));
    }
}
