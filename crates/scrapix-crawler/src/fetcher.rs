//! HTTP page fetcher with connection pooling, compression, and retry logic

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::Utc;
use reqwest::{
    header::{
        HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE,
        IF_MODIFIED_SINCE, IF_NONE_MATCH, RETRY_AFTER, USER_AGENT,
    },
    redirect::Policy,
    Client, Proxy, Response, StatusCode,
};
use tracing::{debug, instrument};
use url::Url;

use scrapix_core::{CrawlUrl, RawPage, Result, ScrapixError};

/// Per-fetch options that override or extend the baseline fetcher config.
///
/// Defaulting everything to the "current behavior" means adding a new field
/// here does not break existing callers — they just keep passing
/// `FetchOptions::default()` (explicitly, or via the legacy `fetch` helper).
#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// When `true`, `application/pdf` responses are accepted. The raw PDF
    /// bytes are base64-encoded into `RawPage.html` so they can traverse the
    /// Kafka `RawPageMessage.html: String` payload without a schema change.
    pub allow_pdf: bool,

    /// Maximum PDF size in bytes. When `None`, the fetcher's generic
    /// `max_body_size` applies. Only consulted when `allow_pdf` is `true`.
    pub pdf_max_size_bytes: Option<u64>,

    /// Extra request headers for this fetch (per-job `headers`). Applied on
    /// top of the fetcher's default headers, replacing any with the same
    /// name.
    pub extra_headers: Vec<(String, String)>,

    /// User-Agent for this fetch (per-job `user_agents` rotation). `None`
    /// keeps the fetcher's configured user agent.
    pub user_agent: Option<String>,

    /// Proxy URL for this fetch (per-job `proxy`). Requests go through a
    /// per-proxy client that keeps every SSRF protection of the default
    /// client. `None` connects directly.
    pub proxy: Option<String>,

    /// Per-job robots.txt override. `Some(false)` skips the robots.txt
    /// check for this fetch; `None`/`Some(true)` keep the fetcher's robots
    /// cache behavior.
    pub respect_robots: Option<bool>,
}

impl FetchOptions {
    /// Convenience constructor for the common case of enabling PDFs with a
    /// size cap (use `None` to rely on the fetcher's global `max_body_size`).
    pub fn with_pdf(max_size_bytes: Option<u64>) -> Self {
        Self {
            allow_pdf: true,
            pdf_max_size_bytes: max_size_bytes,
            ..Default::default()
        }
    }
}

use crate::dns::{CachingDnsResolver, DnsCacheStats, DnsConfig};
use crate::robots::RobotsCache;
use crate::safe_client::{reject_ip_host, safe_client_builder, safe_redirect_policy};
use crate::safe_dns::{validate_proxy_url, NonPublicAddress};

/// Conditional request headers for incremental crawling
#[derive(Debug, Clone, Default)]
pub struct ConditionalRequestHeaders {
    /// ETag from previous response (for If-None-Match header)
    pub etag: Option<String>,
    /// Last-Modified from previous response (for If-Modified-Since header)
    pub last_modified: Option<String>,
}

impl ConditionalRequestHeaders {
    /// Create new conditional headers
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the ETag
    pub fn with_etag(mut self, etag: impl Into<String>) -> Self {
        self.etag = Some(etag.into());
        self
    }

    /// Set the Last-Modified header
    pub fn with_last_modified(mut self, last_modified: impl Into<String>) -> Self {
        self.last_modified = Some(last_modified.into());
        self
    }

    /// Check if any conditional headers are set
    pub fn has_headers(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }
}

/// Result of a conditional fetch operation
#[derive(Debug)]
pub enum FetchResult {
    /// Content was fetched (status 200 or similar)
    Fetched(RawPage),
    /// Content not modified (status 304)
    NotModified {
        /// The URL that was checked
        url: String,
        /// Duration of the request
        fetch_duration_ms: u64,
    },
}

/// Configuration for the HTTP fetcher
#[derive(Debug, Clone)]
pub struct FetcherConfig {
    /// User agent string
    pub user_agent: String,
    /// Request timeout
    pub timeout: Duration,
    /// Connect timeout
    pub connect_timeout: Duration,
    /// Maximum redirects to follow
    pub max_redirects: usize,
    /// Whether to accept invalid SSL certificates
    pub accept_invalid_certs: bool,
    /// Maximum response body size in bytes
    pub max_body_size: usize,
    /// Whether to follow redirects
    pub follow_redirects: bool,
    /// Custom headers to include in requests
    pub custom_headers: HashMap<String, String>,
    /// Retry configuration
    pub retry_config: RetryConfig,
    /// Whether to allow fetching hosts that resolve to private/internal IP
    /// ranges. Defaults to `false` (deny) for SSRF safety. Applies to
    /// hostnames whose DNS results are private; raw-IP hosts (seed URLs and
    /// redirect targets) are always refused regardless of this flag.
    pub allow_private_ips: bool,
}

impl Default for FetcherConfig {
    fn default() -> Self {
        Self {
            user_agent: "Scrapix/1.0 (compatible; +https://github.com/quentindequelen/scrapix)"
                .to_string(),
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_redirects: 10,
            accept_invalid_certs: false,
            max_body_size: 10 * 1024 * 1024, // 10MB
            follow_redirects: true,
            custom_headers: HashMap::new(),
            retry_config: RetryConfig::default(),
            allow_private_ips: false,
        }
    }
}

/// Parse a `Retry-After` header value, which may be either a number of
/// seconds or an HTTP-date (RFC 2822 format).
///
/// Returns `None` if the value is neither form.
pub fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = chrono::DateTime::parse_from_rfc2822(v).ok()?.to_utc();
    Some((when - now).to_std().unwrap_or(Duration::ZERO))
}

/// Retry configuration
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retries
    pub max_retries: u32,
    /// Initial backoff duration
    pub initial_backoff: Duration,
    /// Maximum backoff duration
    pub max_backoff: Duration,
    /// Backoff multiplier
    pub backoff_multiplier: f64,
    /// Status codes that should trigger a retry
    pub retryable_status_codes: Vec<u16>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            retryable_status_codes: vec![429, 500, 502, 503, 504],
        }
    }
}

/// HTTP fetcher implementation
pub struct HttpFetcher {
    client: Client,
    config: FetcherConfig,
    robots_cache: Arc<RobotsCache>,
    dns_resolver: Option<Arc<CachingDnsResolver>>,
    /// Per-proxy clients (keyed by proxy URL), built lazily with the same
    /// settings as `client` plus `.proxy(..)`.
    proxy_clients: dashmap::DashMap<String, Client>,
}

/// Upper bound on cached per-proxy clients. Past it the cache is cleared
/// (clients are cheap to rebuild; this only bounds memory).
const MAX_PROXY_CLIENTS: usize = 1024;

impl HttpFetcher {
    /// Create a new HTTP fetcher with the given configuration
    pub fn new(config: FetcherConfig, robots_cache: Arc<RobotsCache>) -> Result<Self> {
        Self::new_with_dns(config, robots_cache, None)
    }

    /// Create a new HTTP fetcher with DNS caching enabled
    pub fn new_with_dns(
        config: FetcherConfig,
        robots_cache: Arc<RobotsCache>,
        dns_resolver: Option<Arc<CachingDnsResolver>>,
    ) -> Result<Self> {
        let client = Self::build_client(&config, dns_resolver.clone(), None)?;

        Ok(Self {
            client,
            config,
            robots_cache,
            dns_resolver,
            proxy_clients: dashmap::DashMap::new(),
        })
    }

    /// Build a `reqwest::Client` from `config`. Always starts from
    /// [`safe_client_builder`] (SSRF resolver, raw-IP-refusing redirect
    /// policy, `.no_proxy()`); when `proxy` is given, the explicit job proxy
    /// is added on top.
    fn build_client(
        config: &FetcherConfig,
        dns_resolver: Option<Arc<CachingDnsResolver>>,
        proxy: Option<&str>,
    ) -> Result<Client> {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,text/markdown,application/xml;q=0.8,application/pdf;q=0.5",
            ),
        );
        default_headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, deflate, br"),
        );
        default_headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));

        // Add custom headers
        for (name, value) in &config.custom_headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::try_from(name.as_str()),
                HeaderValue::from_str(value),
            ) {
                default_headers.insert(name, value);
            }
        }

        // Raw-IP redirect targets are always refused, the same as raw-IP
        // seeds (`reject_ip_host`), regardless of `allow_private_ips` — that
        // flag only relaxes the private-IP check on *resolved hostnames*
        // (see `SafeResolver`). A redirect to a link-local address such as
        // the cloud metadata endpoint (169.254.169.254) must still be
        // refused even when `allow_private_ips` is set for tests that point
        // the fetcher at a local wiremock server.
        let redirect_policy = if config.follow_redirects {
            safe_redirect_policy(config.max_redirects)
        } else {
            Policy::none()
        };

        let mut builder = safe_client_builder(dns_resolver, config.allow_private_ips);
        if let Some(proxy) = proxy {
            let proxy = Proxy::all(proxy)
                .map_err(|e| ScrapixError::Config(format!("Invalid proxy URL: {e}")))?;
            builder = builder.proxy(proxy);
        }
        builder
            .user_agent(&config.user_agent)
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .redirect(redirect_policy)
            .danger_accept_invalid_certs(config.accept_invalid_certs)
            .default_headers(default_headers)
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .pool_max_idle_per_host(200)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true)
            .build()
            .map_err(|e| ScrapixError::Crawl(format!("Failed to build HTTP client: {}", e)))
    }

    /// The client to use for one fetch: the default client, or the cached
    /// per-proxy client for `options.proxy`.
    ///
    /// The proxy URL is validated on every call (see
    /// [`validate_proxy_url`]): hyper connects to an IP-literal proxy without
    /// consulting the SSRF-safe resolver, so an unchecked tenant proxy such
    /// as `http://169.254.169.254` would bypass it. Validation goes through
    /// the DNS cache, so repeat calls are cheap.
    async fn client_for(&self, options: &FetchOptions) -> Result<Client> {
        let Some(proxy) = options.proxy.as_deref() else {
            return Ok(self.client.clone());
        };
        validate_proxy_url(
            proxy,
            self.dns_resolver.as_ref(),
            self.config.allow_private_ips,
        )
        .await?;
        if let Some(client) = self.proxy_clients.get(proxy) {
            return Ok(client.clone());
        }
        let client = Self::build_client(&self.config, self.dns_resolver.clone(), Some(proxy))?;
        if self.proxy_clients.len() >= MAX_PROXY_CLIENTS {
            self.proxy_clients.clear();
        }
        self.proxy_clients.insert(proxy.to_string(), client.clone());
        Ok(client)
    }

    /// Create a new HTTP fetcher with default configuration
    pub fn with_defaults(robots_cache: Arc<RobotsCache>) -> Result<Self> {
        Self::new(FetcherConfig::default(), robots_cache)
    }

    /// Create a new HTTP fetcher with default configuration and DNS caching
    pub fn with_defaults_and_dns(robots_cache: Arc<RobotsCache>) -> Result<Self> {
        let dns_resolver = Arc::new(CachingDnsResolver::with_defaults()?);
        Self::new_with_dns(FetcherConfig::default(), robots_cache, Some(dns_resolver))
    }

    /// Fetch a URL with retry logic (legacy entry point — equivalent to
    /// `fetch_with_options(url, FetchOptions::default())`).
    pub async fn fetch(&self, url: &CrawlUrl) -> Result<RawPage> {
        self.fetch_with_options(url, FetchOptions::default()).await
    }

    /// Fetch a URL with retry logic and per-call options (e.g., PDF support).
    pub async fn fetch_with_options(
        &self,
        url: &CrawlUrl,
        options: FetchOptions,
    ) -> Result<RawPage> {
        match self.fetch_inner(url, None, options).await? {
            FetchResult::Fetched(page) => Ok(page),
            // Can't happen without conditional headers (no If-None-Match /
            // If-Modified-Since was sent, so a well-behaved server has no
            // basis to return 304), but handle it defensively rather than
            // panicking or silently dropping the response.
            FetchResult::NotModified { url, .. } => Err(ScrapixError::Crawl(format!(
                "Received unexpected 304 Not Modified for {url} without conditional headers"
            ))),
        }
    }

    /// Fetch a URL with conditional headers for incremental crawling.
    ///
    /// Equivalent to `fetch_conditional_with_options(url, headers, FetchOptions::default())`.
    pub async fn fetch_conditional(
        &self,
        url: &CrawlUrl,
        conditional_headers: &ConditionalRequestHeaders,
    ) -> Result<FetchResult> {
        self.fetch_conditional_with_options(url, conditional_headers, FetchOptions::default())
            .await
    }

    /// Fetch a URL with conditional headers and per-call options.
    ///
    /// `FetchOptions` lets callers opt in to PDF acceptance and override the
    /// global `max_body_size` with a PDF-specific cap. Sends If-None-Match /
    /// If-Modified-Since headers if provided, allowing the server to return
    /// 304 Not Modified when content hasn't changed.
    pub async fn fetch_conditional_with_options(
        &self,
        url: &CrawlUrl,
        conditional_headers: &ConditionalRequestHeaders,
        options: FetchOptions,
    ) -> Result<FetchResult> {
        self.fetch_inner(url, Some(conditional_headers), options)
            .await
    }

    /// Shared implementation behind `fetch_with_options` and
    /// `fetch_conditional_with_options`.
    ///
    /// Retries on network errors and on statuses in
    /// `retry_config.retryable_status_codes` (429/5xx by default), honoring
    /// a server `Retry-After` hint (seconds or HTTP-date) by sleeping for
    /// `max(exponential backoff, hint)`, capped at `max_backoff`. A status is
    /// never turned into an `Err` — after the last attempt the final status
    /// is returned as `Ok(FetchResult::Fetched(RawPage { status, .. }))`;
    /// classifying success/failure from `status` is the caller's job.
    #[instrument(skip(self, conditional_headers, options), fields(url = %url.url, allow_pdf = options.allow_pdf))]
    async fn fetch_inner(
        &self,
        url: &CrawlUrl,
        conditional_headers: Option<&ConditionalRequestHeaders>,
        options: FetchOptions,
    ) -> Result<FetchResult> {
        let parsed_url = Url::parse(&url.url)?;

        // Block raw IP addresses to prevent SSRF
        reject_ip_host(&parsed_url)?;

        // Check robots.txt (unless the job opted out)
        if options.respect_robots != Some(false) && !self.robots_cache.is_allowed(&url.url).await? {
            return Err(ScrapixError::RobotsDisallowed {
                url: url.url.clone(),
            });
        }

        let client = self.client_for(&options).await?;

        let mut last_error = None;
        // `backoff` is the pure exponential series (grows every attempt,
        // independent of any server hint) — it's what determines the *next*
        // attempt's default wait. `sleep_for` is what we actually sleep for
        // before the next attempt: `max(backoff, Retry-After hint)`. Keeping
        // them separate means a large one-off Retry-After hint doesn't
        // permanently inflate the exponential backoff for later retries.
        let mut backoff = self.config.retry_config.initial_backoff;
        let mut sleep_for = backoff;

        for attempt in 0..=self.config.retry_config.max_retries {
            if attempt > 0 {
                debug!(attempt, "Retrying request after {:?}", sleep_for);
                tokio::time::sleep(sleep_for).await;
            }

            let start = Instant::now();

            let fetch_once_result = self
                .fetch_once(&client, &parsed_url, conditional_headers, &options)
                .await;

            match fetch_once_result {
                Ok((response, final_url)) => {
                    let fetch_duration = start.elapsed();

                    // Check for 304 Not Modified (only meaningful with conditional headers)
                    if conditional_headers.is_some()
                        && response.status() == StatusCode::NOT_MODIFIED
                    {
                        debug!(url = %url.url, "Content not modified (304)");
                        return Ok(FetchResult::NotModified {
                            url: url.url.clone(),
                            fetch_duration_ms: fetch_duration.as_millis() as u64,
                        });
                    }

                    let status = response.status().as_u16();
                    let retryable = self
                        .config
                        .retry_config
                        .retryable_status_codes
                        .contains(&status);
                    if retryable && attempt < self.config.retry_config.max_retries {
                        let hinted = response
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| parse_retry_after(v, Utc::now()));
                        sleep_for = hinted
                            .map_or(backoff, |h| h.max(backoff))
                            .min(self.config.retry_config.max_backoff);
                        debug!(status, attempt, ?sleep_for, "Retryable status, backing off");
                        // Advance the pure exponential series independent of
                        // the hint, so a one-off large Retry-After doesn't
                        // inflate later retries.
                        backoff = Duration::from_secs_f64(
                            (backoff.as_secs_f64() * self.config.retry_config.backoff_multiplier)
                                .min(self.config.retry_config.max_backoff.as_secs_f64()),
                        );
                        continue;
                    }

                    let page = self
                        .process_response(url, response, final_url, fetch_duration, &options)
                        .await?;
                    return Ok(FetchResult::Fetched(page));
                }
                Err(e) => {
                    // SSRF refusals (raw-IP/non-public redirect target, or a
                    // hostname resolving only to non-public addresses) are
                    // never retryable: retrying re-runs the exact same
                    // resolution/redirect and would just burn through
                    // max_retries × backoff for a request that can never
                    // succeed.
                    if matches!(e, ScrapixError::Refused(_)) {
                        return Err(e);
                    }
                    // Check if this is a non-retryable HTTP error
                    if let ScrapixError::Http { status, .. } = e {
                        if !self
                            .config
                            .retry_config
                            .retryable_status_codes
                            .contains(&status)
                        {
                            return Err(ScrapixError::Http {
                                status,
                                url: url.url.clone(),
                            });
                        }
                    }
                    last_error = Some(e);
                    // No Retry-After hint available for a transport-level
                    // error — fall back to the pure exponential backoff.
                    sleep_for = backoff;
                    backoff = Duration::from_secs_f64(
                        (backoff.as_secs_f64() * self.config.retry_config.backoff_multiplier)
                            .min(self.config.retry_config.max_backoff.as_secs_f64()),
                    );
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ScrapixError::Crawl(format!("Failed to fetch {} after retries", url.url))
        }))
    }

    /// Classify a `reqwest::Error` from `send().await` into a `ScrapixError`.
    ///
    /// Two SSRF-specific cases are checked first, both mapped to
    /// `ScrapixError::Refused` so `fetch_inner` can recognize them and skip
    /// the retry loop entirely (retrying a refusal just re-runs the same
    /// resolution/redirect and burns through the backoff for nothing):
    /// - A redirect the custom `Policy` refused (raw-IP or non-public
    ///   redirect target) surfaces as `reqwest`'s `Kind::Redirect`.
    /// - A `SafeResolver` refusal (hostname resolves only to non-public
    ///   addresses) is wrapped several layers deep inside the connect
    ///   error's source chain as a typed `NonPublicAddress`. We walk
    ///   `source()` and `downcast_ref` onto it rather than pattern-matching
    ///   text out of `reqwest::Error`'s `Debug` output (which would dump the
    ///   whole chain, not just this message).
    fn map_send_error(e: &reqwest::Error, url: &Url) -> ScrapixError {
        if e.is_redirect() {
            return ScrapixError::Refused(format!("redirect refused: {e}"));
        }
        let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
        while let Some(err) = source {
            if let Some(non_public) = err.downcast_ref::<NonPublicAddress>() {
                return ScrapixError::Refused(non_public.to_string());
            }
            source = err.source();
        }
        if e.is_timeout() {
            ScrapixError::Timeout(format!("Request timed out: {}", url))
        } else if e.is_connect() {
            ScrapixError::Connection(format!("Connection failed: {}", e))
        } else if let Some(status) = e.status() {
            ScrapixError::Http {
                status: status.as_u16(),
                url: url.to_string(),
            }
        } else {
            ScrapixError::Network(e.to_string())
        }
    }

    /// Perform a single fetch attempt, applying conditional headers (when
    /// given) and the per-fetch user agent / extra headers.
    async fn fetch_once(
        &self,
        client: &Client,
        url: &Url,
        conditional_headers: Option<&ConditionalRequestHeaders>,
        options: &FetchOptions,
    ) -> Result<(Response, String)> {
        let mut request = client.get(url.as_str());

        for (name, value) in &options.extra_headers {
            match (
                HeaderName::try_from(name.as_str()),
                HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(value)) => request = request.header(name, value),
                _ => debug!(header = %name, "Skipping invalid per-job header"),
            }
        }
        if let Some(ref ua) = options.user_agent {
            if let Ok(value) = HeaderValue::from_str(ua) {
                request = request.header(USER_AGENT, value);
            }
        }

        if let Some(conditional) = conditional_headers {
            if let Some(ref etag) = conditional.etag {
                if let Ok(value) = HeaderValue::from_str(etag) {
                    request = request.header(IF_NONE_MATCH, value);
                }
            }
            if let Some(ref last_modified) = conditional.last_modified {
                if let Ok(value) = HeaderValue::from_str(last_modified) {
                    request = request.header(IF_MODIFIED_SINCE, value);
                }
            }
        }

        let response = request
            .send()
            .await
            .map_err(|e| Self::map_send_error(&e, url))?;
        let final_url = response.url().to_string();
        Ok((response, final_url))
    }

    /// Process the response into a RawPage.
    ///
    /// The `options` param allows per-fetch overrides: `allow_pdf` gates the
    /// content-type allowlist and flips the body path to base64-encode bytes
    /// (PDFs are binary; the Kafka payload `RawPageMessage.html` is a
    /// `String`). When `options.allow_pdf` is `false`, behavior matches the
    /// pre-PDF fetcher exactly.
    async fn process_response(
        &self,
        crawl_url: &CrawlUrl,
        response: Response,
        final_url: String,
        fetch_duration: Duration,
        options: &FetchOptions,
    ) -> Result<RawPage> {
        let status = response.status().as_u16();

        // Convert headers
        let mut headers = HashMap::new();
        for (name, value) in response.headers() {
            if let Ok(v) = value.to_str() {
                headers.insert(name.to_string(), v.to_string());
            }
        }

        let content_type = headers.get("content-type").cloned();

        // Is this a PDF response? Only meaningful when PDF support is enabled.
        let is_pdf = options.allow_pdf
            && content_type
                .as_deref()
                .is_some_and(|ct| ct.contains("application/pdf"));

        let is_success = (200..=299).contains(&status);

        // Check content type — we accept HTML, markdown, and (when opted in) PDF.
        // Non-2xx responses (error pages) skip this check: they are never
        // indexed and are often served as text/plain regardless of what the
        // "real" content type would be.
        if is_success {
            if let Some(ref ct) = content_type {
                let accepted = ct.contains("text/html")
                    || ct.contains("application/xhtml")
                    || ct.contains("text/markdown")
                    || is_pdf;
                if !accepted {
                    return Err(ScrapixError::Crawl(format!(
                        "Unsupported content type: {}",
                        ct
                    )));
                }
            }
        }

        // Effective size cap: PDFs may use a feature-specific cap that supersedes
        // the generic `max_body_size` (since PDFs are often larger than HTML).
        // Non-2xx bodies are never indexed, so cap them at a small fixed size
        // regardless of the configured limit.
        let effective_cap = if is_pdf {
            options
                .pdf_max_size_bytes
                .map(|b| b as usize)
                .unwrap_or(self.config.max_body_size)
        } else {
            self.config.max_body_size
        };
        let effective_cap = if is_success {
            effective_cap
        } else {
            effective_cap.min(64 * 1024)
        };

        // Whether exceeding `effective_cap` is a hard failure (2xx — the page
        // would be indexed, so we must not silently truncate it) or just a
        // truncation point (non-2xx — the body is never indexed, it's only
        // kept for diagnostics, so R1 requires we still return `Ok` with the
        // final status rather than turning it into an `Err`).
        let hard_cap = is_success;

        // Reject up front when the server told us the size via Content-Length.
        // Only for 2xx — a non-2xx body is truncated below instead.
        if hard_cap {
            if let Some(len) = response.content_length() {
                if len as usize > effective_cap {
                    return Err(ScrapixError::Crawl(format!(
                        "Response body too large: {len} bytes (max: {effective_cap})"
                    )));
                }
            }
        }

        // Read the body as a stream so we never buffer more than the cap,
        // even when the server lies about (or omits) Content-Length.
        let mut bytes = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(0)
                .min(effective_cap as u64) as usize,
        );
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ScrapixError::Network(format!("Failed to read response body: {e}")))?
        {
            if bytes.len() + chunk.len() > effective_cap {
                if hard_cap {
                    return Err(ScrapixError::Crawl(format!(
                        "Response body too large: exceeded {effective_cap} bytes"
                    )));
                }
                // Non-2xx: keep the prefix up to the cap and stop reading —
                // never indexed, so a status is still never turned into an
                // `Err` just because the error page happened to be large.
                let remaining = effective_cap.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..remaining]);
                break;
            }
            bytes.extend_from_slice(&chunk);
        }

        // Encode body:
        // - PDFs → base64 into `html` (binary-safe over Kafka JSON payloads).
        // - Everything else → UTF-8 (with lossy fallback, matches legacy behavior).
        let html = if is_pdf {
            BASE64.encode(&bytes)
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };

        Ok(RawPage {
            url: crawl_url.url.clone(),
            final_url,
            status,
            headers,
            html,
            content_type,
            js_rendered: false,
            fetched_at: Utc::now(),
            fetch_duration_ms: fetch_duration.as_millis() as u64,
        })
    }

    /// Check if a URL is allowed by robots.txt
    pub async fn is_allowed(&self, url: &str) -> Result<bool> {
        self.robots_cache.is_allowed(url).await
    }

    /// Get crawl delay for a domain
    pub async fn get_crawl_delay(&self, domain: &str) -> Result<Option<u64>> {
        self.robots_cache.get_crawl_delay(domain).await
    }

    /// Pre-resolve DNS for a hostname (warms the cache)
    ///
    /// This can be called before fetching to ensure DNS is cached.
    /// Returns the resolved IP addresses.
    pub async fn resolve_dns(&self, hostname: &str) -> Result<Vec<std::net::IpAddr>> {
        if let Some(ref resolver) = self.dns_resolver {
            resolver.resolve(hostname).await
        } else {
            Err(ScrapixError::Crawl(
                "DNS resolver not configured".to_string(),
            ))
        }
    }

    /// Pre-resolve DNS for a URL (warms the cache)
    pub async fn resolve_url_dns(&self, url: &str) -> Result<Vec<std::net::IpAddr>> {
        let parsed = Url::parse(url)?;
        if let Some(host) = parsed.host_str() {
            self.resolve_dns(host).await
        } else {
            Err(ScrapixError::Crawl(format!("No host in URL: {}", url)))
        }
    }

    /// Get DNS cache statistics
    pub fn dns_cache_stats(&self) -> Option<DnsCacheStats> {
        self.dns_resolver.as_ref().map(|r| r.cache_stats())
    }

    /// Clear the DNS cache
    pub fn clear_dns_cache(&self) {
        if let Some(ref resolver) = self.dns_resolver {
            resolver.clear_cache();
        }
    }

    /// Check if DNS caching is enabled
    pub fn has_dns_cache(&self) -> bool {
        self.dns_resolver.is_some()
    }
}

/// Trait implementation for the core Fetcher trait
#[async_trait]
impl scrapix_core::traits::Fetcher for HttpFetcher {
    async fn fetch(&self, url: &CrawlUrl) -> Result<RawPage> {
        HttpFetcher::fetch(self, url).await
    }

    async fn is_allowed(&self, url: &str) -> Result<bool> {
        self.is_allowed(url).await
    }

    async fn get_crawl_delay(&self, domain: &str) -> Result<Option<u64>> {
        self.get_crawl_delay(domain).await
    }
}

/// Builder for HttpFetcher
pub struct HttpFetcherBuilder {
    config: FetcherConfig,
    dns_config: Option<DnsConfig>,
}

impl HttpFetcherBuilder {
    pub fn new() -> Self {
        Self {
            config: FetcherConfig::default(),
            dns_config: None,
        }
    }

    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.config.user_agent = user_agent.into();
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.config.connect_timeout = timeout;
        self
    }

    pub fn max_redirects(mut self, max: usize) -> Self {
        self.config.max_redirects = max;
        self
    }

    pub fn accept_invalid_certs(mut self, accept: bool) -> Self {
        self.config.accept_invalid_certs = accept;
        self
    }

    pub fn max_body_size(mut self, size: usize) -> Self {
        self.config.max_body_size = size;
        self
    }

    pub fn follow_redirects(mut self, follow: bool) -> Self {
        self.config.follow_redirects = follow;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.custom_headers.insert(name.into(), value.into());
        self
    }

    pub fn max_retries(mut self, max: u32) -> Self {
        self.config.retry_config.max_retries = max;
        self
    }

    /// Set the initial backoff duration between retries (grows by
    /// `backoff_multiplier` on each subsequent retry, capped at `max_backoff`).
    pub fn initial_backoff(mut self, backoff: Duration) -> Self {
        self.config.retry_config.initial_backoff = backoff;
        self
    }

    /// Cap on a single in-process retry wait (exponential backoff or a
    /// server `Retry-After` hint).
    pub fn max_backoff(mut self, max: Duration) -> Self {
        self.config.retry_config.max_backoff = max;
        self
    }

    /// Allow fetching hosts that resolve to private/internal IP ranges.
    /// Defaults to `false`.
    pub fn allow_private_ips(mut self, allow: bool) -> Self {
        self.config.allow_private_ips = allow;
        self
    }

    /// Enable DNS caching with default configuration
    pub fn with_dns_cache(mut self) -> Self {
        self.dns_config = Some(DnsConfig::default());
        self
    }

    /// Enable DNS caching with custom configuration
    pub fn with_dns_config(mut self, config: DnsConfig) -> Self {
        self.dns_config = Some(config);
        self
    }

    /// Set DNS cache TTL
    pub fn dns_cache_ttl(mut self, ttl: Duration) -> Self {
        let config = self.dns_config.get_or_insert_with(DnsConfig::default);
        config.cache_ttl = ttl;
        self
    }

    /// Set maximum DNS cache size
    pub fn dns_max_cache_size(mut self, size: usize) -> Self {
        let config = self.dns_config.get_or_insert_with(DnsConfig::default);
        config.max_cache_size = size;
        self
    }

    pub fn build(self, robots_cache: Arc<RobotsCache>) -> Result<HttpFetcher> {
        let dns_resolver = if let Some(dns_config) = self.dns_config {
            Some(Arc::new(CachingDnsResolver::new(dns_config)?))
        } else {
            None
        };
        HttpFetcher::new_with_dns(self.config, robots_cache, dns_resolver)
    }
}

impl Default for HttpFetcherBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fetcher_config_default() {
        let config = FetcherConfig::default();
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.max_redirects, 10);
        assert!(!config.accept_invalid_certs);
    }

    #[test]
    fn test_builder() {
        let builder = HttpFetcherBuilder::new()
            .user_agent("TestBot/1.0")
            .timeout(Duration::from_secs(60))
            .max_redirects(5)
            .header("X-Custom", "value");

        assert_eq!(builder.config.user_agent, "TestBot/1.0");
        assert_eq!(builder.config.timeout, Duration::from_secs(60));
        assert_eq!(builder.config.max_redirects, 5);
        assert_eq!(
            builder.config.custom_headers.get("X-Custom"),
            Some(&"value".to_string())
        );
    }
}
