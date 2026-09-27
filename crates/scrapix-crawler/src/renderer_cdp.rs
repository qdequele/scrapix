//! Chrome DevTools Protocol renderer using chromiumoxide
//!
//! This module provides browser-based page rendering using Chrome/Chromium
//! via the Chrome DevTools Protocol (CDP). This is the most mature and
//! feature-rich option for JavaScript rendering.
//!
//! ## Features
//!
//! - Full JavaScript execution
//! - Network interception
//! - Screenshot capture
//! - PDF generation
//! - Cookie management
//! - Browser pool for concurrency
//!
//! ## Usage
//!
//! Enable the `browser-cdp` feature:
//!
//! ```toml
//! scrapix-crawler = { version = "0.1", features = ["browser-cdp"] }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::CookieParam;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::Page;
use chrono::Utc;
use futures::StreamExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::{debug, instrument, warn};

use scrapix_core::{CrawlUrl, RawPage, Result, ScrapixError};

use chromiumoxide::cdp::browser_protocol::network::{Headers, SetExtraHttpHeadersParams};
use chromiumoxide::ArcHttpRequest;

use crate::fetcher::FetchOptions;
use crate::robots::RobotsCache;
use crate::safe_client::reject_ip_host;
use crate::safe_dns::is_public_ip;

/// Reason a browser render is refused when the job sets a proxy: the
/// shared browser has a single, worker-level proxy.
pub const BROWSER_PROXY_UNSUPPORTED: &str = "per-job proxy is not supported for browser rendering";

/// Per-render request shaping (from the job's `FetchOptions`).
#[derive(Default)]
struct PageRequest<'a> {
    user_agent: Option<&'a str>,
    extra_headers: &'a [(String, String)],
    respect_robots: Option<bool>,
}

/// HTTP status of the main document as reported by CDP (`Network.Response.status`),
/// if it is a valid HTTP status code.
pub(crate) fn document_status(status: Option<i64>) -> Option<u16> {
    status
        .and_then(|s| u16::try_from(s).ok())
        .filter(|s| (100..=599).contains(s))
}

/// CDP response headers (a JSON object) as a lowercase-keyed map, so e.g.
/// `retry-after` is found the same way as on the HTTP path.
pub(crate) fn response_headers(headers: &serde_json::Value) -> HashMap<String, String> {
    headers
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| {
                    let value = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (k.to_ascii_lowercase(), value)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Check a URL before handing it to the browser: the same SSRF rules as the
/// HTTP fetcher (raw-IP hosts refused; hostnames must resolve only to public
/// addresses unless `allow_private_ips`) and, when `robots` is given,
/// robots.txt.
///
/// The browser does its own DNS resolution, so unlike `SafeResolver` we
/// cannot pin the checked addresses: any non-public address in the answer
/// refuses the URL outright.
pub(crate) async fn check_render_target(
    url: &str,
    allow_private_ips: bool,
    robots: Option<&RobotsCache>,
) -> Result<()> {
    let parsed = url::Url::parse(url)?;
    check_ssrf_target(&parsed, allow_private_ips).await?;

    if let Some(cache) = robots {
        if !cache.is_allowed(url).await? {
            return Err(ScrapixError::RobotsDisallowed {
                url: url.to_string(),
            });
        }
    }
    Ok(())
}

/// The SSRF half of [`check_render_target`]: raw-IP hosts are refused
/// (always), and a hostname must resolve only to public addresses (unless
/// `allow_private_ips`).
async fn check_ssrf_target(parsed: &url::Url, allow_private_ips: bool) -> Result<()> {
    reject_ip_host(parsed)?;
    if !allow_private_ips {
        let host = parsed
            .host_str()
            .ok_or_else(|| ScrapixError::Crawl(format!("URL has no host: {parsed}")))?;
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| ScrapixError::Connection(format!("DNS lookup failed for {host}: {e}")))?
            .collect();
        if addrs.is_empty() || addrs.iter().any(|a| !is_public_ip(a.ip())) {
            return Err(ScrapixError::Refused(format!(
                "{host} resolves to a non-public address"
            )));
        }
    }
    Ok(())
}

/// Check the URL the browser ended on after navigating to `requested`
/// (server redirects, meta refresh, JS `location` changes): the same SSRF
/// rules as [`check_render_target`], failing closed with
/// `ScrapixError::Refused` so the content of an internal page is never
/// returned. `requested` itself was checked before navigating, and pages
/// without a network origin (`about:blank`, `chrome-error://`, `data:`)
/// have nothing to check.
///
/// This covers where the main frame landed only. The browser may still have
/// *requested* an internal URL on the way (a redirect hop, a subresource, a
/// fetch/XHR, an iframe, a JS navigation that was redirected again); full
/// coverage needs CDP `Fetch` interception of every request, which is
/// deferred. Workers that render untrusted pages should run the browser in
/// a network namespace without access to internal addresses.
pub(crate) async fn check_render_final_url(
    final_url: &str,
    requested: &str,
    allow_private_ips: bool,
) -> Result<()> {
    if final_url == requested {
        return Ok(());
    }
    let parsed = url::Url::parse(final_url)
        .map_err(|e| ScrapixError::Refused(format!("browser ended on an unparsable URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        return Ok(());
    }
    check_ssrf_target(&parsed, allow_private_ips)
        .await
        .map_err(|e| match e {
            ScrapixError::Refused(msg) => {
                ScrapixError::Refused(format!("redirected to a refused target: {msg}"))
            }
            // A DNS failure on the final host: we cannot tell it is public.
            other => ScrapixError::Refused(format!(
                "redirected to a target that could not be checked: {other}"
            )),
        })
}

/// Errors specific to CDP rendering
#[derive(Debug, Error)]
pub enum CdpError {
    #[error("Browser launch failed: {0}")]
    LaunchFailed(String),

    #[error("Navigation failed: {0}")]
    NavigationFailed(String),

    #[error("Page timeout: {0}")]
    Timeout(String),

    #[error("JavaScript execution failed: {0}")]
    JsExecutionFailed(String),

    #[error("Screenshot failed: {0}")]
    ScreenshotFailed(String),

    #[error("Browser connection lost")]
    ConnectionLost,

    #[error("Pool exhausted")]
    PoolExhausted,
}

impl From<CdpError> for ScrapixError {
    fn from(err: CdpError) -> Self {
        ScrapixError::Crawl(err.to_string())
    }
}

/// Wait condition for page loading
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WaitUntil {
    /// Wait for load event
    #[default]
    Load,
    /// Wait for DOMContentLoaded event
    DomContentLoaded,
    /// Wait for network to be idle (no requests for 500ms)
    NetworkIdle,
    /// Wait for network to be mostly idle (max 2 requests for 500ms)
    NetworkAlmostIdle,
}

/// Configuration for the CDP renderer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdpConfig {
    /// Path to Chrome/Chromium executable (None = auto-detect)
    #[serde(default)]
    pub executable_path: Option<String>,

    /// Whether to run in headless mode
    #[serde(default = "default_true")]
    pub headless: bool,

    /// Whether to disable GPU acceleration
    #[serde(default = "default_true")]
    pub disable_gpu: bool,

    /// Whether to disable sandbox (required for Docker)
    #[serde(default)]
    pub no_sandbox: bool,

    /// Viewport width
    #[serde(default = "default_viewport_width")]
    pub viewport_width: u32,

    /// Viewport height
    #[serde(default = "default_viewport_height")]
    pub viewport_height: u32,

    /// Page load timeout
    #[serde(default = "default_timeout")]
    pub timeout: Duration,

    /// Wait condition for page loading
    #[serde(default)]
    pub wait_until: WaitUntil,

    /// Additional wait time after page load (for dynamic content)
    #[serde(default)]
    pub extra_wait: Option<Duration>,

    /// Maximum concurrent pages
    #[serde(default = "default_max_pages")]
    pub max_concurrent_pages: usize,

    /// User agent string
    #[serde(default)]
    pub user_agent: Option<String>,

    /// Whether to block images
    #[serde(default)]
    pub block_images: bool,

    /// Whether to block stylesheets
    #[serde(default)]
    pub block_stylesheets: bool,

    /// Whether to block fonts
    #[serde(default)]
    pub block_fonts: bool,

    /// Custom JavaScript to inject before page load
    #[serde(default)]
    pub inject_script: Option<String>,

    /// Proxy server URL
    #[serde(default)]
    pub proxy: Option<String>,

    /// Additional Chrome arguments
    #[serde(default)]
    pub extra_args: Vec<String>,

    /// Allow rendering hosts that resolve to private/internal addresses.
    /// Defaults to `false` (SSRF protection); tests/self-hosting only.
    #[serde(default)]
    pub allow_private_ips: bool,
}

fn default_true() -> bool {
    true
}
fn default_viewport_width() -> u32 {
    1920
}
fn default_viewport_height() -> u32 {
    1080
}
fn default_timeout() -> Duration {
    Duration::from_secs(30)
}
fn default_max_pages() -> usize {
    5
}

impl Default for CdpConfig {
    fn default() -> Self {
        Self {
            executable_path: None,
            headless: true,
            disable_gpu: true,
            no_sandbox: false,
            viewport_width: default_viewport_width(),
            viewport_height: default_viewport_height(),
            timeout: default_timeout(),
            wait_until: WaitUntil::default(),
            extra_wait: None,
            max_concurrent_pages: default_max_pages(),
            user_agent: None,
            block_images: false,
            block_stylesheets: false,
            block_fonts: false,
            inject_script: None,
            proxy: None,
            extra_args: Vec::new(),
            allow_private_ips: false,
        }
    }
}

/// Result of rendering a page
#[derive(Debug)]
pub struct RenderResult {
    /// The rendered HTML
    pub html: String,

    /// Final URL after redirects
    pub final_url: String,

    /// HTTP status code
    pub status: u16,

    /// Response headers
    pub headers: HashMap<String, String>,

    /// Content type
    pub content_type: Option<String>,

    /// Screenshot (if requested)
    pub screenshot: Option<Vec<u8>>,

    /// Console log messages
    pub console_logs: Vec<String>,

    /// JavaScript errors
    pub js_errors: Vec<String>,

    /// Render duration
    pub render_duration: Duration,
}

/// CDP-based browser renderer
pub struct CdpRenderer {
    browser: Browser,
    config: CdpConfig,
    semaphore: Arc<Semaphore>,
    robots_cache: Option<Arc<RobotsCache>>,
    console_logs: Arc<Mutex<Vec<String>>>,
    js_errors: Arc<Mutex<Vec<String>>>,
}

impl CdpRenderer {
    /// Create a new CDP renderer with the given configuration
    pub async fn new(config: CdpConfig, robots_cache: Option<Arc<RobotsCache>>) -> Result<Self> {
        let browser_config = Self::build_browser_config(&config)?;

        let (browser, mut handler) = Browser::launch(browser_config)
            .await
            .map_err(|e| CdpError::LaunchFailed(e.to_string()))?;

        // Spawn handler task
        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(e) = event {
                    warn!(error = %e, "Browser handler error");
                }
            }
        });

        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_pages));

        Ok(Self {
            browser,
            config,
            semaphore,
            robots_cache,
            console_logs: Arc::new(Mutex::new(Vec::new())),
            js_errors: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Create with default configuration
    pub async fn with_defaults(robots_cache: Option<Arc<RobotsCache>>) -> Result<Self> {
        Self::new(CdpConfig::default(), robots_cache).await
    }

    /// Build browser configuration from our config
    fn build_browser_config(config: &CdpConfig) -> Result<BrowserConfig> {
        let mut builder = BrowserConfig::builder();

        if config.headless {
            builder = builder.with_head();
        }

        if config.disable_gpu {
            builder = builder.arg("--disable-gpu");
        }

        if config.no_sandbox {
            builder = builder.arg("--no-sandbox");
            builder = builder.arg("--disable-setuid-sandbox");
        }

        // Set viewport via window size argument
        builder = builder.arg(format!(
            "--window-size={},{}",
            config.viewport_width, config.viewport_height
        ));

        // Set user agent via argument
        if let Some(ref user_agent) = config.user_agent {
            builder = builder.arg(format!("--user-agent={}", user_agent));
        }

        // Set proxy via argument
        if let Some(ref proxy) = config.proxy {
            builder = builder.arg(format!("--proxy-server={}", proxy));
        }

        // Add extra arguments
        for arg in &config.extra_args {
            builder = builder.arg(arg);
        }

        // Memory and performance optimizations
        builder = builder
            .arg("--disable-dev-shm-usage")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-translate")
            .arg("--metrics-recording-only")
            .arg("--mute-audio")
            .arg("--no-first-run");

        builder
            .build()
            .map_err(|e| ScrapixError::Crawl(format!("Failed to build browser config: {}", e)))
    }

    /// Render a page and return the result
    #[instrument(skip(self), fields(url = %url))]
    pub async fn render(&self, url: &str) -> Result<RenderResult> {
        self.render_checked(url, &PageRequest::default()).await
    }

    /// Render a page after the SSRF check and (unless `req.respect_robots`
    /// is `Some(false)`) the robots.txt check, applying the per-request user
    /// agent and extra headers to the page before navigating.
    async fn render_checked(&self, url: &str, req: &PageRequest<'_>) -> Result<RenderResult> {
        let robots = if req.respect_robots == Some(false) {
            None
        } else {
            self.robots_cache.as_deref()
        };
        check_render_target(url, self.config.allow_private_ips, robots).await?;

        // Acquire semaphore
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| CdpError::PoolExhausted)?;

        let start = Instant::now();

        // Create new page
        let page = self
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| CdpError::LaunchFailed(format!("Failed to create page: {}", e)))?;

        // Always close the page, including on error paths.
        let result = self.render_on_page(&page, url, req, start).await;
        let _ = page.close().await;
        result
    }

    async fn render_on_page(
        &self,
        page: &Page,
        url: &str,
        req: &PageRequest<'_>,
        start: Instant,
    ) -> Result<RenderResult> {
        // Setup page
        self.setup_page(page).await?;

        // Per-job user agent and headers, set on this page only (the browser
        // is shared by every job on the worker).
        if let Some(ua) = req.user_agent {
            page.set_user_agent(ua.to_string())
                .await
                .map_err(|e| CdpError::NavigationFailed(format!("set user agent: {e}")))?;
        }
        if !req.extra_headers.is_empty() {
            let headers: serde_json::Map<String, serde_json::Value> = req
                .extra_headers
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            page.execute(SetExtraHttpHeadersParams::new(Headers::new(
                serde_json::Value::Object(headers),
            )))
            .await
            .map_err(|e| CdpError::NavigationFailed(format!("set extra headers: {e}")))?;
        }

        // Navigate to URL
        page.goto(url)
            .await
            .map_err(|e| CdpError::NavigationFailed(e.to_string()))?;

        // Wait for page load based on configuration; yields the main-frame
        // document request (with its HTTP response, when CDP reported one).
        let navigation = self.wait_for_page(page).await?;

        // Extra wait if configured
        if let Some(extra_wait) = self.config.extra_wait {
            tokio::time::sleep(extra_wait).await;
        }

        // Get final URL
        let final_url = page
            .url()
            .await
            .map_err(|e| CdpError::NavigationFailed(e.to_string()))?
            .unwrap_or_else(|| url.to_string());
        // A redirect must not land the browser on an internal target.
        check_render_final_url(&final_url, url, self.config.allow_private_ips).await?;

        // Get HTML content
        let html = page
            .content()
            .await
            .map_err(|e| CdpError::NavigationFailed(format!("Failed to get content: {}", e)))?;

        // Status, headers and content type of the main document response.
        let response = navigation.as_ref().and_then(|r| r.response.as_ref());
        let status = match document_status(response.map(|r| r.status)) {
            Some(status) => status,
            None => {
                debug!(url, "No main-document HTTP status from CDP; assuming 200");
                200
            }
        };
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
            .or_else(|| Some("text/html".to_string()));

        // Collect console logs and errors
        let console_logs = std::mem::take(&mut *self.console_logs.lock());
        let js_errors = std::mem::take(&mut *self.js_errors.lock());

        let render_duration = start.elapsed();

        debug!(
            duration_ms = render_duration.as_millis(),
            final_url = %final_url,
            status = status,
            "Page rendered"
        );

        Ok(RenderResult {
            html,
            final_url,
            status,
            headers,
            content_type,
            screenshot: None,
            console_logs,
            js_errors,
            render_duration,
        })
    }

    /// Setup page with configured options
    async fn setup_page(&self, page: &Page) -> Result<()> {
        // Inject script if configured
        if let Some(ref script) = self.config.inject_script {
            page.evaluate_on_new_document(script.clone())
                .await
                .map_err(|e| CdpError::JsExecutionFailed(e.to_string()))?;
        }

        Ok(())
    }

    /// Wait for page to load based on configuration
    async fn wait_for_page(&self, page: &Page) -> Result<ArcHttpRequest> {
        let timeout = self.config.timeout;

        let (label, settle) = match self.config.wait_until {
            WaitUntil::Load => ("Page load timeout", false),
            // DOMContentLoaded is typically faster
            WaitUntil::DomContentLoaded => ("DOMContentLoaded timeout", false),
            // Wait for navigation then additional time for network
            WaitUntil::NetworkIdle | WaitUntil::NetworkAlmostIdle => ("Navigation timeout", true),
        };
        let navigation = tokio::time::timeout(timeout, page.wait_for_navigation_response())
            .await
            .map_err(|_| CdpError::Timeout(label.to_string()))?
            .map_err(|e| CdpError::NavigationFailed(e.to_string()))?;
        if settle {
            // Additional wait for network to settle
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        Ok(navigation)
    }

    /// Render a page and take a screenshot
    pub async fn render_with_screenshot(&self, url: &str) -> Result<RenderResult> {
        let mut result = self.render(url).await?;

        // Create a new page for screenshot (since we closed the original)
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| CdpError::PoolExhausted)?;

        let page = self
            .browser
            .new_page(url)
            .await
            .map_err(|e| CdpError::LaunchFailed(format!("Failed to create page: {}", e)))?;

        self.setup_page(&page).await?;
        self.wait_for_page(&page).await?;

        // Take screenshot
        let screenshot = page
            .screenshot(ScreenshotParams::builder().full_page(true).build())
            .await
            .map_err(|e| CdpError::ScreenshotFailed(e.to_string()))?;

        let _ = page.close().await;

        result.screenshot = Some(screenshot);

        Ok(result)
    }

    /// Execute JavaScript on a page and return the result
    pub async fn execute_script(&self, url: &str, script: &str) -> Result<serde_json::Value> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| CdpError::PoolExhausted)?;

        let page = self
            .browser
            .new_page(url)
            .await
            .map_err(|e| CdpError::LaunchFailed(format!("Failed to create page: {}", e)))?;

        self.setup_page(&page).await?;
        self.wait_for_page(&page).await?;

        let result = page
            .evaluate(script)
            .await
            .map_err(|e| CdpError::JsExecutionFailed(e.to_string()))?;

        let _ = page.close().await;

        Ok(result.into_value()?)
    }

    /// Set cookies for a domain
    pub async fn set_cookies(&self, cookies: Vec<CookieParam>) -> Result<()> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| CdpError::PoolExhausted)?;

        let page = self
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| CdpError::LaunchFailed(format!("Failed to create page: {}", e)))?;

        for cookie in cookies {
            page.set_cookie(cookie)
                .await
                .map_err(|e| CdpError::NavigationFailed(format!("Failed to set cookie: {}", e)))?;
        }

        let _ = page.close().await;
        Ok(())
    }

    /// Fetch a CrawlUrl and return a RawPage
    #[instrument(skip(self), fields(url = %url.url))]
    pub async fn fetch(&self, url: &CrawlUrl) -> Result<RawPage> {
        self.fetch_with_options(url, &FetchOptions::default()).await
    }

    /// Fetch a CrawlUrl with per-job options: `user_agent` and
    /// `extra_headers` are applied to this page via CDP, and
    /// `respect_robots == Some(false)` skips robots.txt (the SSRF check
    /// always runs).
    ///
    /// A per-job `proxy` cannot be honored by the shared browser, so it is
    /// refused ([`BROWSER_PROXY_UNSUPPORTED`]) rather than silently ignored.
    ///
    /// Note: `Network.setExtraHTTPHeaders` applies to every request the page
    /// makes, including subresources on other hosts.
    pub async fn fetch_with_options(
        &self,
        url: &CrawlUrl,
        options: &FetchOptions,
    ) -> Result<RawPage> {
        if options.proxy.is_some() {
            return Err(ScrapixError::Config(BROWSER_PROXY_UNSUPPORTED.to_string()));
        }
        let req = PageRequest {
            user_agent: options.user_agent.as_deref(),
            extra_headers: &options.extra_headers,
            respect_robots: options.respect_robots,
        };
        let result = self.render_checked(&url.url, &req).await?;

        Ok(RawPage {
            url: url.url.clone(),
            final_url: result.final_url,
            status: result.status,
            headers: result.headers,
            html: result.html,
            content_type: result.content_type,
            js_rendered: true,
            fetched_at: Utc::now(),
            fetch_duration_ms: result.render_duration.as_millis() as u64,
        })
    }

    /// Get the current configuration
    pub fn config(&self) -> &CdpConfig {
        &self.config
    }
}

/// Trait implementation for the core Fetcher trait
#[async_trait]
impl scrapix_core::traits::Fetcher for CdpRenderer {
    async fn fetch(&self, url: &CrawlUrl) -> Result<RawPage> {
        CdpRenderer::fetch(self, url).await
    }

    async fn is_allowed(&self, url: &str) -> Result<bool> {
        if let Some(ref cache) = self.robots_cache {
            cache.is_allowed(url).await
        } else {
            Ok(true)
        }
    }

    async fn get_crawl_delay(&self, domain: &str) -> Result<Option<u64>> {
        if let Some(ref cache) = self.robots_cache {
            cache.get_crawl_delay(domain).await
        } else {
            Ok(None)
        }
    }
}

/// Builder for CdpRenderer
pub struct CdpRendererBuilder {
    config: CdpConfig,
    robots_cache: Option<Arc<RobotsCache>>,
}

impl CdpRendererBuilder {
    pub fn new() -> Self {
        Self {
            config: CdpConfig::default(),
            robots_cache: None,
        }
    }

    pub fn executable_path(mut self, path: impl Into<String>) -> Self {
        self.config.executable_path = Some(path.into());
        self
    }

    pub fn headless(mut self, headless: bool) -> Self {
        self.config.headless = headless;
        self
    }

    pub fn no_sandbox(mut self, no_sandbox: bool) -> Self {
        self.config.no_sandbox = no_sandbox;
        self
    }

    pub fn viewport(mut self, width: u32, height: u32) -> Self {
        self.config.viewport_width = width;
        self.config.viewport_height = height;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    pub fn wait_until(mut self, wait: WaitUntil) -> Self {
        self.config.wait_until = wait;
        self
    }

    pub fn extra_wait(mut self, duration: Duration) -> Self {
        self.config.extra_wait = Some(duration);
        self
    }

    pub fn max_concurrent_pages(mut self, max: usize) -> Self {
        self.config.max_concurrent_pages = max;
        self
    }

    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.config.user_agent = Some(user_agent.into());
        self
    }

    pub fn block_images(mut self, block: bool) -> Self {
        self.config.block_images = block;
        self
    }

    pub fn block_stylesheets(mut self, block: bool) -> Self {
        self.config.block_stylesheets = block;
        self
    }

    pub fn inject_script(mut self, script: impl Into<String>) -> Self {
        self.config.inject_script = Some(script.into());
        self
    }

    pub fn proxy(mut self, proxy: impl Into<String>) -> Self {
        self.config.proxy = Some(proxy.into());
        self
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.config.extra_args.push(arg.into());
        self
    }

    /// Allow rendering hosts that resolve to private addresses (tests only).
    pub fn allow_private_ips(mut self, allow: bool) -> Self {
        self.config.allow_private_ips = allow;
        self
    }

    pub fn robots_cache(mut self, cache: Arc<RobotsCache>) -> Self {
        self.robots_cache = Some(cache);
        self
    }

    pub async fn build(self) -> Result<CdpRenderer> {
        CdpRenderer::new(self.config, self.robots_cache).await
    }
}

impl Default for CdpRendererBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_status_accepts_only_http_codes() {
        assert_eq!(document_status(Some(200)), Some(200));
        assert_eq!(document_status(Some(404)), Some(404));
        assert_eq!(document_status(Some(503)), Some(503));
        assert_eq!(document_status(Some(0)), None);
        assert_eq!(document_status(Some(-1)), None);
        assert_eq!(document_status(Some(70_000)), None);
        assert_eq!(document_status(None), None);
    }

    #[test]
    fn response_headers_are_lowercased() {
        let h = response_headers(&serde_json::json!({
            "Content-Type": "text/html; charset=utf-8",
            "Retry-After": "30",
            "X-Num": 5
        }));
        assert_eq!(h.get("content-type").unwrap(), "text/html; charset=utf-8");
        assert_eq!(h.get("retry-after").unwrap(), "30");
        assert_eq!(h.get("x-num").unwrap(), "5");
        assert!(response_headers(&serde_json::Value::Null).is_empty());
    }

    #[tokio::test]
    async fn render_target_refuses_raw_ips_and_private_hosts() {
        let raw = check_render_target("http://169.254.169.254/latest", false, None).await;
        assert!(matches!(raw, Err(ScrapixError::Refused(_))), "{raw:?}");
        // Raw IPs stay refused even with the private-IP opt-out.
        let raw = check_render_target("http://127.0.0.1/", true, None).await;
        assert!(matches!(raw, Err(ScrapixError::Refused(_))), "{raw:?}");
        // localhost resolves to loopback.
        let local = check_render_target("http://localhost/", false, None).await;
        assert!(matches!(local, Err(ScrapixError::Refused(_))), "{local:?}");
        // Opt-out lets a hostname resolving to loopback through.
        assert!(check_render_target("http://localhost/", true, None)
            .await
            .is_ok());
    }

    /// Final review fix 4: the page the browser ended on (after HTTP or JS
    /// redirects) gets the same SSRF check as the requested URL.
    #[tokio::test]
    async fn redirect_final_url_is_checked_like_the_target() {
        let seed = "https://example.com/start";
        for final_url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:8080/admin",
            "http://localhost:6379/",
        ] {
            let r = check_render_final_url(final_url, seed, false).await;
            assert!(
                matches!(r, Err(ScrapixError::Refused(_))),
                "{final_url}: {r:?}"
            );
        }
        // Raw IPs stay refused with the opt-out; hostnames then pass.
        let r = check_render_final_url("http://127.0.0.1/", seed, true).await;
        assert!(matches!(r, Err(ScrapixError::Refused(_))), "{r:?}");
        assert!(check_render_final_url("http://localhost/", seed, true)
            .await
            .is_ok());
        // No redirect (already checked) and non-network pages pass.
        for final_url in [seed, "about:blank", "chrome-error://chromewebdata/"] {
            assert!(
                check_render_final_url(final_url, seed, false).await.is_ok(),
                "{final_url}"
            );
        }
    }

    #[test]
    fn test_config_defaults() {
        let config = CdpConfig::default();
        assert!(config.headless);
        assert!(config.disable_gpu);
        assert!(!config.no_sandbox);
        assert_eq!(config.viewport_width, 1920);
        assert_eq!(config.viewport_height, 1080);
        assert_eq!(config.max_concurrent_pages, 5);
    }

    #[test]
    fn test_builder() {
        let builder = CdpRendererBuilder::new()
            .headless(false)
            .no_sandbox(true)
            .viewport(1280, 720)
            .timeout(Duration::from_secs(60))
            .user_agent("TestBot/1.0");

        assert!(!builder.config.headless);
        assert!(builder.config.no_sandbox);
        assert_eq!(builder.config.viewport_width, 1280);
        assert_eq!(builder.config.viewport_height, 720);
        assert_eq!(builder.config.timeout, Duration::from_secs(60));
        assert_eq!(builder.config.user_agent, Some("TestBot/1.0".to_string()));
    }
}
