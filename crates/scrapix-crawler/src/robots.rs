//! Robots.txt parsing and caching
//!
//! This module provides both in-memory and persistent (RocksDB-backed) caching
//! for robots.txt files. The persistent cache survives restarts and can be shared
//! across worker instances.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::future::{BoxFuture, FutureExt, Shared, WeakShared};
use parking_lot::{Mutex, RwLock};
use reqwest::Client;
use robotstxt::DefaultMatcher;
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument, warn};
use url::Url;

use scrapix_core::{Result, ScrapixError};

use crate::dns::CachingDnsResolver;

/// Cache key for a robots.txt entry: `scheme://host:port` of `url`.
///
/// robots.txt applies per origin (RFC 9309 §2.3), so `http://a.test`,
/// `https://a.test` and `https://a.test:8443` each get their own entry.
pub(crate) fn robots_cache_key(url: &Url) -> Result<String> {
    let host = url
        .host_str()
        .ok_or_else(|| ScrapixError::Crawl("URL has no host".to_string()))?;
    let port = url.port_or_known_default().unwrap_or(0);
    Ok(format!("{}://{}:{}", url.scheme(), host, port))
}

/// robots.txt URL and cache key for a bare domain (callers that only know a
/// domain, such as crawl-delay lookups, assume https on the default port).
fn domain_robots_url(domain: &str) -> Result<(Url, String)> {
    let url = Url::parse(&format!("https://{domain}/robots.txt"))?;
    let key = robots_cache_key(&url)?;
    Ok((url, key))
}

/// Configuration for robots.txt handling
#[derive(Debug, Clone)]
pub struct RobotsConfig {
    /// User agent to match in robots.txt
    pub user_agent: String,
    /// Cache TTL for robots.txt files
    pub cache_ttl: Duration,
    /// Timeout for fetching robots.txt
    pub fetch_timeout: Duration,
    /// Whether to respect robots.txt (can be disabled for testing)
    pub respect_robots: bool,
    /// Default crawl delay if not specified
    pub default_crawl_delay_ms: Option<u64>,
    /// Whether to allow fetching robots.txt from hosts that resolve to
    /// private/internal IP ranges. Defaults to `false` (deny) for SSRF
    /// safety, matching `FetcherConfig::allow_private_ips` — set this to the
    /// same value as the fetcher's flag wherever both are constructed
    /// together, so the robots.txt client isn't more permissive (or more
    /// restrictive) than the main page fetcher for the same crawl.
    pub allow_private_ips: bool,
}

impl Default for RobotsConfig {
    fn default() -> Self {
        Self {
            user_agent: "Scrapix".to_string(),
            cache_ttl: Duration::from_secs(3600), // 1 hour
            fetch_timeout: Duration::from_secs(10),
            respect_robots: true,
            default_crawl_delay_ms: None,
            allow_private_ips: false,
        }
    }
}

/// Largest robots.txt body kept, in bytes (500 KiB, the limit Google
/// applies). A longer body is truncated at the last complete line before
/// the limit; the rest is never read.
pub const MAX_ROBOTS_BODY_BYTES: usize = 500 * 1024;

/// Most origins [`RobotsCache`] keeps. Past it, expired entries go first,
/// then arbitrary ones.
const MAX_CACHED_ORIGINS: usize = 100_000;

/// How often an insert also drops the expired entries.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Cached robots.txt entry
struct CachedRobots {
    /// Raw robots.txt content, shared with every check (never copied).
    content: Arc<str>,
    /// When this entry was cached
    cached_at: Instant,
    /// Crawl delay from robots.txt (in milliseconds)
    crawl_delay_ms: Option<u64>,
}

/// The cached origins and when expired ones were last dropped.
struct CacheState {
    entries: HashMap<String, CachedRobots>,
    last_sweep: Instant,
}

impl CacheState {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }

    /// Insert `entry`, keeping at most `max` origins: expired entries are
    /// dropped when the cache is full (and every [`SWEEP_INTERVAL`]), then
    /// arbitrary ones (about 1% at a time, so a full cache does not scan on
    /// every insert).
    fn insert_bounded(&mut self, key: String, entry: CachedRobots, ttl: Duration, max: usize) {
        let full = self.entries.len() >= max && !self.entries.contains_key(&key);
        if full || self.last_sweep.elapsed() >= SWEEP_INTERVAL {
            self.entries.retain(|_, e| e.cached_at.elapsed() < ttl);
            self.last_sweep = Instant::now();
        }
        if self.entries.len() >= max && !self.entries.contains_key(&key) {
            let excess = self.entries.len() + 1 - max;
            let victims: Vec<String> = self
                .entries
                .keys()
                .take(excess.max(max / 100))
                .cloned()
                .collect();
            for victim in victims {
                self.entries.remove(&victim);
            }
        }
        self.entries.insert(key, entry);
    }
}

/// One robots.txt fetch, shared by every caller waiting for that origin.
type RobotsFetch = Shared<BoxFuture<'static, Arc<str>>>;

/// A [`RobotsFetch`] that does not keep the fetch alive.
type WeakRobotsFetch = WeakShared<BoxFuture<'static, Arc<str>>>;

/// State shared by the cache and its in-flight fetches.
struct RobotsShared {
    config: RobotsConfig,
    client: Client,
    cache: RwLock<CacheState>,
    /// The fetch in progress for an origin (cache key), with its id. Held
    /// weakly: the fetch lives as long as one caller awaits it, and removes
    /// its entry when it completes or is dropped ([`FlightGuard`]).
    in_flight: Mutex<HashMap<String, (u64, WeakRobotsFetch)>>,
    next_flight_id: AtomicU64,
}

impl RobotsShared {
    /// The cached content of `key`, unless missing or expired.
    fn fresh(&self, key: &str) -> Option<Arc<str>> {
        let cache = self.cache.read();
        let entry = cache.entries.get(key)?;
        (entry.cached_at.elapsed() < self.config.cache_ttl).then(|| entry.content.clone())
    }
}

/// Removes an in-flight entry when its fetch ends, however it ends:
/// completion, or every waiting caller dropped (cancelled) before that.
struct FlightGuard {
    shared: Arc<RobotsShared>,
    key: String,
    id: u64,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        let mut in_flight = self.shared.in_flight.lock();
        if in_flight
            .get(&self.key)
            .is_some_and(|(id, _)| *id == self.id)
        {
            in_flight.remove(&self.key);
        }
    }
}

/// Cache for robots.txt files
pub struct RobotsCache {
    shared: Arc<RobotsShared>,
}

impl RobotsCache {
    /// Create a new robots.txt cache with its own HTTP client (own
    /// connection pool, system DNS).
    pub fn new(config: RobotsConfig) -> Result<Self> {
        Self::with_dns(config, None)
    }

    /// Create a robots.txt cache whose client resolves hosts through
    /// `dns_cache` (the page fetcher's resolver), so a new origin is
    /// resolved once for robots.txt and its pages. Same SSRF policy as
    /// [`Self::new`].
    pub fn with_dns_cache(
        config: RobotsConfig,
        dns_cache: Arc<CachingDnsResolver>,
    ) -> Result<Self> {
        Self::with_dns(config, Some(dns_cache))
    }

    fn with_dns(config: RobotsConfig, dns_cache: Option<Arc<CachingDnsResolver>>) -> Result<Self> {
        let client = crate::safe_client::safe_client_builder(dns_cache, config.allow_private_ips)
            .timeout(config.fetch_timeout)
            .user_agent(&config.user_agent)
            .build()
            .map_err(|e| ScrapixError::Crawl(format!("Failed to build robots client: {}", e)))?;
        Ok(Self::from_parts(config, client))
    }

    /// Create a robots.txt cache that fetches through `client`, sharing its
    /// connection pool and resolver. The caller guarantees `client` comes
    /// from [`crate::safe_client::safe_client_builder`] with
    /// `config.allow_private_ips` and the default redirect policy (see
    /// `HttpFetcherBuilder::build_with_robots`, the only caller).
    pub(crate) fn with_client(config: RobotsConfig, client: Client) -> Result<Self> {
        reqwest::header::HeaderValue::from_str(&config.user_agent)
            .map_err(|e| ScrapixError::Crawl(format!("Failed to build robots client: {}", e)))?;
        Ok(Self::from_parts(config, client))
    }

    fn from_parts(config: RobotsConfig, client: Client) -> Self {
        Self {
            shared: Arc::new(RobotsShared {
                config,
                client,
                cache: RwLock::new(CacheState::new()),
                in_flight: Mutex::new(HashMap::new()),
                next_flight_id: AtomicU64::new(0),
            }),
        }
    }

    /// Create a new robots.txt cache with default configuration
    pub fn with_defaults() -> Result<Self> {
        Self::new(RobotsConfig::default())
    }

    /// Check if a URL is allowed by robots.txt
    #[instrument(skip(self))]
    pub async fn is_allowed(&self, url: &str) -> Result<bool> {
        let config = &self.shared.config;
        if !config.respect_robots {
            return Ok(true);
        }

        let parsed = Url::parse(url)?;
        let key = robots_cache_key(&parsed)?;
        let domain = key.as_str();

        let robots_content = self.get_robots(domain, &parsed).await?;
        let path = parsed.path();

        // The robotstxt crate has no parsed form to keep: it matches while
        // it parses, so each check re-reads the (shared, uncopied) body.
        let mut matcher = DefaultMatcher::default();
        let allowed =
            matcher.one_agent_allowed_by_robots(&robots_content, &config.user_agent, path);

        debug!(url, allowed, "Robots.txt check");
        Ok(allowed)
    }

    /// robots.txt `Crawl-delay` of `url`'s origin (scheme+host+port), from
    /// the cache only (never fetches robots.txt). `None`: the origin is not
    /// cached (or its entry expired); `Some(None)`: robots.txt was fetched
    /// and sets no crawl-delay.
    pub fn cached_crawl_delay(&self, url: &str) -> Option<Option<u64>> {
        let parsed = Url::parse(url).ok()?;
        let key = robots_cache_key(&parsed).ok()?;
        let cache = self.shared.cache.read();
        let entry = cache.entries.get(&key)?;
        (entry.cached_at.elapsed() < self.shared.config.cache_ttl).then_some(entry.crawl_delay_ms)
    }

    /// Get crawl delay for a domain
    pub async fn get_crawl_delay(&self, domain: &str) -> Result<Option<u64>> {
        let config = &self.shared.config;
        let (robots_url, key) = domain_robots_url(domain)?;
        let domain = key.as_str();
        // Check cache first
        {
            let cache = self.shared.cache.read();
            if let Some(entry) = cache.entries.get(domain) {
                if entry.cached_at.elapsed() < config.cache_ttl {
                    return Ok(entry.crawl_delay_ms.or(config.default_crawl_delay_ms));
                }
            }
        }

        let _ = self.get_robots(domain, &robots_url).await?;

        // Now check cache again
        let cache = self.shared.cache.read();
        if let Some(entry) = cache.entries.get(domain) {
            return Ok(entry.crawl_delay_ms.or(config.default_crawl_delay_ms));
        }

        Ok(config.default_crawl_delay_ms)
    }

    /// Get robots.txt content for an origin, fetching if needed. `domain`
    /// is the cache key from [`robots_cache_key`].
    ///
    /// Callers that miss the cache for the same origin at the same time
    /// share one fetch: the first starts it, the others await its result.
    /// The fetch is not tied to the caller that started it: it runs as long
    /// as one caller awaits it, and is dropped when the last one is.
    async fn get_robots(&self, domain: &str, url: &Url) -> Result<Arc<str>> {
        let shared = &self.shared;
        if let Some(content) = shared.fresh(domain) {
            return Ok(content);
        }

        // Raw-IP hosts bypass `SafeResolver` entirely (reqwest never calls
        // the DNS resolver for a URL whose host is already an address), so
        // they must be refused explicitly here too, exactly like raw-IP
        // seed URLs in `HttpFetcher::fetch_inner`.
        crate::safe_client::reject_ip_host(url)?;
        let robots_url = robots_txt_url(url);

        let fetch: RobotsFetch = {
            let mut in_flight = shared.in_flight.lock();
            // A fetch stores its result before leaving `in_flight`, so with
            // the lock held an origin is cached, in flight, or neither.
            if let Some(content) = shared.fresh(domain) {
                return Ok(content);
            }
            match in_flight.get(domain).and_then(|(_, fetch)| fetch.upgrade()) {
                Some(fetch) => fetch,
                None => {
                    let id = shared.next_flight_id.fetch_add(1, Ordering::Relaxed);
                    let fetch = fetch_and_cache(shared.clone(), domain.to_string(), robots_url, id)
                        .boxed()
                        .shared();
                    if let Some(weak) = fetch.downgrade() {
                        in_flight.insert(domain.to_string(), (id, weak));
                    }
                    fetch
                }
            }
        };

        Ok(fetch.await)
    }

    /// Parse crawl delay from robots.txt content
    #[cfg(test)]
    fn parse_crawl_delay(&self, content: &str) -> Option<u64> {
        parse_crawl_delay(&self.shared.config.user_agent, content)
    }

    /// Clear the cache
    pub fn clear_cache(&self) {
        let mut cache = self.shared.cache.write();
        cache.entries.clear();
    }

    /// Get cache stats
    pub fn cache_stats(&self) -> (usize, usize) {
        let cache = self.shared.cache.read();
        let total = cache.entries.len();
        let expired = cache
            .entries
            .values()
            .filter(|e| e.cached_at.elapsed() >= self.shared.config.cache_ttl)
            .count();
        (total, total - expired)
    }
}

/// The robots.txt URL of `url`'s origin. Keeps a non-default port:
/// robots.txt lives on the same authority as the page (RFC 9309 §2.3).
fn robots_txt_url(url: &Url) -> String {
    format!(
        "{}://{}{}/robots.txt",
        url.scheme(),
        url.host_str().unwrap_or(""),
        url.port().map(|p| format!(":{p}")).unwrap_or_default()
    )
}

/// Fetch `robots_url`, cache the result under `key` and leave the in-flight
/// map. Owns everything it needs, so it outlives the caller that started it.
async fn fetch_and_cache(
    shared: Arc<RobotsShared>,
    key: String,
    robots_url: String,
    id: u64,
) -> Arc<str> {
    let _guard = FlightGuard {
        shared: shared.clone(),
        key: key.clone(),
        id,
    };
    let content: Arc<str> = fetch_robots_body(&shared.client, &shared.config, &robots_url)
        .await
        .into();
    let crawl_delay_ms = parse_crawl_delay(&shared.config.user_agent, &content);
    shared.cache.write().insert_bounded(
        key,
        CachedRobots {
            content: content.clone(),
            cached_at: Instant::now(),
            crawl_delay_ms,
        },
        shared.config.cache_ttl,
        MAX_CACHED_ORIGINS,
    );
    content
}

/// Fetch a robots.txt. Never fails: a 404, another non-2xx status, a
/// network error or an unreadable body all mean "no rules" (permissive).
async fn fetch_robots_body(client: &Client, config: &RobotsConfig, robots_url: &str) -> String {
    debug!(robots_url, "Fetching robots.txt");

    // Set per request, so a client shared with the page fetcher (other
    // defaults) sends the same request as a dedicated one.
    let request = client
        .get(robots_url)
        .timeout(config.fetch_timeout)
        .header(reqwest::header::USER_AGENT, &config.user_agent)
        .header(reqwest::header::ACCEPT, "*/*");

    match request.send().await {
        Ok(response) => {
            if response.status().is_success() {
                read_capped_text(response).await
            } else if response.status().as_u16() == 404 {
                // No robots.txt means everything is allowed
                debug!(robots_url, "No robots.txt found (404)");
                String::new()
            } else {
                warn!(
                    robots_url,
                    status = response.status().as_u16(),
                    "Failed to fetch robots.txt"
                );
                // On error, be permissive
                String::new()
            }
        }
        Err(e) => {
            warn!(robots_url, error = %e, "Error fetching robots.txt");
            // On network error, be permissive
            String::new()
        }
    }
}

/// Read a robots.txt body as text, streamed, keeping at most
/// [`MAX_ROBOTS_BODY_BYTES`]: a longer (or endless) body is cut at the last
/// complete line before the limit and the rest is not read. A read error
/// gives an empty body, as before. Decoding is `Response::text`'s (the
/// `Content-Type` charset, UTF-8 by default).
async fn read_capped_text(mut response: reqwest::Response) -> String {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_ROBOTS_BODY_BYTES - bytes.len();
                if chunk.len() > room {
                    bytes.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return String::new(),
        }
    }
    if truncated {
        // Half a line could be a different rule (`Disallow: /` cut from
        // `Disallow: /private`): keep whole lines only.
        let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        bytes.truncate(keep);
        warn!(
            url = %response.url(),
            kept_bytes = bytes.len(),
            "robots.txt is over the size limit; the rest is ignored"
        );
    }
    drop(response);

    let mut decoded = hyper::Response::new(reqwest::Body::from(bytes));
    if let Some(content_type) = content_type {
        decoded
            .headers_mut()
            .insert(reqwest::header::CONTENT_TYPE, content_type);
    }
    reqwest::Response::from(decoded)
        .text()
        .await
        .unwrap_or_default()
}

/// Parse the crawl delay `user_agent` gets from robots.txt content
fn parse_crawl_delay(user_agent: &str, content: &str) -> Option<u64> {
    let mut in_user_agent_section = false;
    let user_agent_lower = user_agent.to_lowercase();

    for line in content.lines() {
        let line = line.trim().to_lowercase();

        if line.starts_with("user-agent:") {
            let agent = line.trim_start_matches("user-agent:").trim();
            in_user_agent_section = agent == "*" || agent == user_agent_lower;
        } else if in_user_agent_section && line.starts_with("crawl-delay:") {
            let delay_str = line.trim_start_matches("crawl-delay:").trim();
            if let Ok(delay) = delay_str.parse::<f64>() {
                if delay < 0.0 || delay.is_nan() || delay.is_infinite() {
                    return None;
                }
                // Convert seconds to milliseconds
                return Some((delay * 1000.0) as u64);
            }
        }
    }

    None
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn cache_key_is_scheme_host_port() {
        let k = |u: &str| robots_cache_key(&Url::parse(u).unwrap()).unwrap();
        assert_eq!(k("https://a.test/x"), "https://a.test:443");
        assert_eq!(k("http://a.test/x"), "http://a.test:80");
        assert_eq!(k("https://a.test:8443/x"), "https://a.test:8443");
        assert_ne!(k("http://a.test/"), k("https://a.test/"));
        assert_eq!(domain_robots_url("a.test").unwrap().1, "https://a.test:443");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_robots_config_default() {
        let config = RobotsConfig::default();
        assert_eq!(config.cache_ttl, Duration::from_secs(3600));
        assert!(config.respect_robots);
    }

    #[test]
    fn test_parse_crawl_delay() {
        let cache = RobotsCache::new(RobotsConfig::default()).unwrap();

        // Standard crawl delay
        let content = "User-agent: *\nCrawl-delay: 2";
        assert_eq!(cache.parse_crawl_delay(content), Some(2000));

        // Fractional delay
        let content = "User-agent: *\nCrawl-delay: 0.5";
        assert_eq!(cache.parse_crawl_delay(content), Some(500));

        // No crawl delay
        let content = "User-agent: *\nDisallow: /admin/";
        assert_eq!(cache.parse_crawl_delay(content), None);

        // Specific user agent
        let config = RobotsConfig {
            user_agent: "Scrapix".to_string(),
            ..Default::default()
        };
        let cache = RobotsCache::new(config).unwrap();
        let content = "User-agent: Scrapix\nCrawl-delay: 5\nUser-agent: *\nCrawl-delay: 1";
        assert_eq!(cache.parse_crawl_delay(content), Some(5000));
    }

    #[test]
    fn test_negative_crawl_delay() {
        let cache = RobotsCache::new(RobotsConfig::default()).unwrap();
        let content = "User-agent: *\nCrawl-delay: -5";
        assert_eq!(cache.parse_crawl_delay(content), None);
    }

    #[test]
    fn test_nan_crawl_delay() {
        let cache = RobotsCache::new(RobotsConfig::default()).unwrap();
        let content = "User-agent: *\nCrawl-delay: NaN";
        assert_eq!(cache.parse_crawl_delay(content), None);
    }

    #[test]
    fn test_infinity_crawl_delay() {
        let cache = RobotsCache::new(RobotsConfig::default()).unwrap();
        let content = "User-agent: *\nCrawl-delay: inf";
        assert_eq!(cache.parse_crawl_delay(content), None);
    }

    #[test]
    fn test_zero_crawl_delay() {
        let cache = RobotsCache::new(RobotsConfig::default()).unwrap();
        let content = "User-agent: *\nCrawl-delay: 0";
        assert_eq!(cache.parse_crawl_delay(content), Some(0));
    }

    #[test]
    fn test_fractional_crawl_delay() {
        let cache = RobotsCache::new(RobotsConfig::default()).unwrap();
        let content = "User-agent: *\nCrawl-delay: 0.5";
        assert_eq!(cache.parse_crawl_delay(content), Some(500));
    }
}

#[cfg(test)]
mod fetch_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config() -> RobotsConfig {
        RobotsConfig {
            allow_private_ips: true,
            ..Default::default()
        }
    }

    /// wiremock binds 127.0.0.1 and raw-IP hosts are refused: use localhost.
    fn page(server: &MockServer, p: &str) -> String {
        format!("{}{}", server.uri().replace("127.0.0.1", "localhost"), p)
    }

    async fn robots_requests(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/robots.txt")
            .count()
    }

    async fn slow_robots(delay: Duration) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("User-agent: *\nDisallow: /private\nCrawl-delay: 2\n")
                    .set_delay(delay),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn concurrent_checks_of_a_new_origin_share_one_fetch() {
        let server = slow_robots(Duration::from_millis(200)).await;
        let cache = Arc::new(RobotsCache::new(config()).unwrap());

        let checks: Vec<_> = (0..16)
            .map(|i| {
                let cache = cache.clone();
                let url = if i % 2 == 0 {
                    page(&server, "/private/x")
                } else {
                    page(&server, &format!("/public/{i}"))
                };
                tokio::spawn(async move { cache.is_allowed(&url).await.unwrap() })
            })
            .collect();
        for (i, check) in checks.into_iter().enumerate() {
            assert_eq!(check.await.unwrap(), i % 2 == 1, "check {i}");
        }

        assert_eq!(robots_requests(&server).await, 1);
        assert!(cache.shared.in_flight.lock().is_empty());
        assert_eq!(
            cache.cached_crawl_delay(&page(&server, "/")),
            Some(Some(2000))
        );
        // Cached from now on.
        assert!(!cache.is_allowed(&page(&server, "/private")).await.unwrap());
        assert_eq!(robots_requests(&server).await, 1);
    }

    #[tokio::test]
    async fn a_cancelled_first_caller_does_not_strand_the_others() {
        let server = slow_robots(Duration::from_millis(300)).await;
        let cache = Arc::new(RobotsCache::new(config()).unwrap());
        let url = page(&server, "/private/x");

        let leader = {
            let (cache, url) = (cache.clone(), url.clone());
            tokio::spawn(async move { cache.is_allowed(&url).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let follower = {
            let (cache, url) = (cache.clone(), url.clone());
            tokio::spawn(async move { cache.is_allowed(&url).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());

        let allowed = tokio::time::timeout(Duration::from_secs(5), follower)
            .await
            .expect("the follower finishes the shared fetch")
            .unwrap();
        assert!(!allowed);
        assert_eq!(robots_requests(&server).await, 1);
        assert!(cache.shared.in_flight.lock().is_empty());
    }

    #[tokio::test]
    async fn a_fetch_nobody_awaits_is_dropped_and_started_again() {
        let server = slow_robots(Duration::from_millis(300)).await;
        let cache = Arc::new(RobotsCache::new(config()).unwrap());
        let url = page(&server, "/private/x");

        let callers: Vec<_> = (0..3)
            .map(|_| {
                let (cache, url) = (cache.clone(), url.clone());
                tokio::spawn(async move { cache.is_allowed(&url).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(cache.shared.in_flight.lock().len(), 1);
        for caller in &callers {
            caller.abort();
        }
        for caller in callers {
            let _ = caller.await;
        }
        // Nothing left behind, nothing cached from the abandoned fetch.
        assert!(cache.shared.in_flight.lock().is_empty());
        assert_eq!(cache.cache_stats(), (0, 0));

        assert!(!cache.is_allowed(&url).await.unwrap());
        assert!(cache.shared.in_flight.lock().is_empty());
        assert_eq!(cache.cache_stats(), (1, 1));
    }

    #[tokio::test]
    async fn status_and_error_handling_is_permissive() {
        for status in [404, 403, 500, 503] {
            let server = MockServer::start().await;
            Mock::given(path("/robots.txt"))
                .respond_with(ResponseTemplate::new(status).set_body_string("Disallow: /"))
                .mount(&server)
                .await;
            let cache = RobotsCache::new(config()).unwrap();
            assert!(
                cache.is_allowed(&page(&server, "/x")).await.unwrap(),
                "{status}"
            );
            assert_eq!(
                cache.cached_crawl_delay(&page(&server, "/x")),
                Some(None),
                "{status} is cached as no rules"
            );
        }

        // Nothing listens there: a network error allows, and is cached.
        let cache = RobotsCache::new(config()).unwrap();
        assert!(cache.is_allowed("http://localhost:1/x").await.unwrap());
        assert_eq!(cache.cache_stats(), (1, 1));

        // Raw-IP hosts are refused, private addresses too without the opt-in.
        assert!(matches!(
            cache.is_allowed("http://127.0.0.1:1/x").await,
            Err(ScrapixError::Refused(_))
        ));
        let server = slow_robots(Duration::ZERO).await;
        let strict = RobotsCache::new(RobotsConfig::default()).unwrap();
        assert!(strict
            .is_allowed(&page(&server, "/private/x"))
            .await
            .unwrap());
        assert_eq!(robots_requests(&server).await, 0);

        // `respect_robots: false` never fetches.
        let off = RobotsCache::new(RobotsConfig {
            respect_robots: false,
            ..config()
        })
        .unwrap();
        assert!(off.is_allowed(&page(&server, "/private/x")).await.unwrap());
        assert_eq!(robots_requests(&server).await, 0);
    }

    #[tokio::test]
    async fn an_oversized_body_is_cut_at_a_line_before_the_limit() {
        let mut body = String::from("User-agent: *\nDisallow: /early\n");
        let mut i = 0;
        while body.len() < MAX_ROBOTS_BODY_BYTES + 200_000 {
            body.push_str(&format!("Disallow: /filler/{i:08}/\n"));
            i += 1;
        }
        body.push_str("Disallow: /late\n");
        let server = MockServer::start().await;
        Mock::given(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
            .mount(&server)
            .await;

        let cache = RobotsCache::new(config()).unwrap();
        assert!(!cache.is_allowed(&page(&server, "/early")).await.unwrap());
        assert!(!cache
            .is_allowed(&page(&server, "/filler/00000001/x"))
            .await
            .unwrap());
        assert!(cache.is_allowed(&page(&server, "/late")).await.unwrap());

        let key = robots_cache_key(&Url::parse(&page(&server, "/")).unwrap()).unwrap();
        let kept = cache.shared.fresh(&key).unwrap();
        assert!(kept.len() <= MAX_ROBOTS_BODY_BYTES);
        assert!(kept.len() > MAX_ROBOTS_BODY_BYTES - 64);
        assert!(kept.ends_with("/\n"), "cut at a line end");
        assert!(body.starts_with(&*kept));
    }

    #[tokio::test]
    async fn the_body_is_decoded_with_its_charset() {
        let server = MockServer::start().await;
        // "Disallow: /caf\xe9" in ISO-8859-1, and a UTF-8 BOM case below.
        Mock::given(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                b"User-agent: *\nDisallow: /caf\xe9\n".to_vec(),
                "text/plain; charset=iso-8859-1",
            ))
            .mount(&server)
            .await;
        let cache = RobotsCache::new(config()).unwrap();
        cache.is_allowed(&page(&server, "/x")).await.unwrap();
        let key = robots_cache_key(&Url::parse(&page(&server, "/")).unwrap()).unwrap();
        assert_eq!(
            &*cache.shared.fresh(&key).unwrap(),
            "User-agent: *\nDisallow: /caf\u{e9}\n"
        );
    }

    #[test]
    fn the_cache_is_bounded() {
        let ttl = Duration::from_secs(60);
        let entry = |age: Duration| CachedRobots {
            content: Arc::from(""),
            cached_at: Instant::now() - age,
            crawl_delay_ms: None,
        };
        let mut state = CacheState::new();
        for i in 0..10 {
            state.insert_bounded(format!("old{i}"), entry(ttl * 2), ttl, 20);
        }
        for i in 0..10 {
            state.insert_bounded(format!("new{i}"), entry(Duration::ZERO), ttl, 20);
        }
        assert_eq!(state.entries.len(), 20);

        // Full: the expired entries go first.
        state.insert_bounded("x".into(), entry(Duration::ZERO), ttl, 20);
        assert_eq!(state.entries.len(), 11);
        assert!(state.entries.keys().all(|k| !k.starts_with("old")));

        // Full of live entries: some are evicted, the new one is kept.
        for i in 0..100 {
            state.insert_bounded(format!("more{i}"), entry(Duration::ZERO), ttl, 20);
            assert!(state.entries.len() <= 20);
            assert!(state.entries.contains_key(&format!("more{i}")));
        }
        // Replacing a cached origin evicts nothing.
        let before = state.entries.len();
        state.insert_bounded("more99".into(), entry(Duration::ZERO), ttl, 20);
        assert_eq!(state.entries.len(), before);

        // Expired entries are dropped on a later insert even when not full.
        let mut state = CacheState::new();
        state.insert_bounded("old".into(), entry(ttl * 2), ttl, 20);
        state.last_sweep = Instant::now() - SWEEP_INTERVAL;
        state.insert_bounded("new".into(), entry(Duration::ZERO), ttl, 20);
        assert_eq!(state.entries.len(), 1);
    }
}

// ============================================================================
// Persistent Robots Cache (RocksDB-backed)
// ============================================================================

/// Serializable robots.txt cache entry for persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentRobotsEntry {
    /// Raw robots.txt content
    pub content: String,
    /// Unix timestamp when cached (seconds since epoch)
    pub cached_at_unix: u64,
    /// Crawl delay in milliseconds
    pub crawl_delay_ms: Option<u64>,
    /// Sitemap URLs discovered from robots.txt
    pub sitemaps: Vec<String>,
}

impl PersistentRobotsEntry {
    /// Check if this entry has expired
    pub fn is_expired(&self, ttl: Duration) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.cached_at_unix) > ttl.as_secs()
    }

    /// Get age of this entry
    pub fn age(&self) -> Duration {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Duration::from_secs(now.saturating_sub(self.cached_at_unix))
    }
}

/// Trait for robots.txt persistence storage
pub trait RobotsPersistence: Send + Sync {
    /// Get a cached entry by domain
    fn get(&self, domain: &str) -> Result<Option<PersistentRobotsEntry>>;

    /// Store a cache entry for a domain
    fn put(&self, domain: &str, entry: &PersistentRobotsEntry) -> Result<()>;

    /// Delete a cached entry
    fn delete(&self, domain: &str) -> Result<()>;

    /// Get all cached domains
    fn list_domains(&self) -> Result<Vec<String>>;

    /// Count cached entries
    fn count(&self) -> Result<usize>;

    /// Clear all cached entries
    fn clear(&self) -> Result<()>;
}

/// RocksDB-backed robots.txt persistence
pub struct RocksRobotsPersistence {
    storage: Arc<dyn RocksDbOps>,
    column_family: String,
}

/// Trait abstracting RocksDB operations for testability
pub trait RocksDbOps: Send + Sync {
    fn get_cf(&self, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn put_cf(&self, cf: &str, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete_cf(&self, cf: &str, key: &[u8]) -> Result<()>;
    fn prefix_iter_cf(&self, cf: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
}

impl RocksRobotsPersistence {
    /// Create a new RocksDB robots persistence with a storage backend
    pub fn new(storage: Arc<dyn RocksDbOps>, column_family: impl Into<String>) -> Self {
        Self {
            storage,
            column_family: column_family.into(),
        }
    }

    /// Create with default column family name
    pub fn with_defaults(storage: Arc<dyn RocksDbOps>) -> Self {
        Self::new(storage, "robots_cache")
    }

    fn domain_key(domain: &str) -> Vec<u8> {
        format!("robots:{}", domain).into_bytes()
    }
}

impl RobotsPersistence for RocksRobotsPersistence {
    fn get(&self, domain: &str) -> Result<Option<PersistentRobotsEntry>> {
        let key = Self::domain_key(domain);
        match self.storage.get_cf(&self.column_family, &key)? {
            Some(data) => {
                let entry: PersistentRobotsEntry = serde_json::from_slice(&data).map_err(|e| {
                    ScrapixError::Storage(format!("Failed to deserialize robots entry: {}", e))
                })?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    fn put(&self, domain: &str, entry: &PersistentRobotsEntry) -> Result<()> {
        let key = Self::domain_key(domain);
        let data = serde_json::to_vec(entry).map_err(|e| {
            ScrapixError::Storage(format!("Failed to serialize robots entry: {}", e))
        })?;
        self.storage.put_cf(&self.column_family, &key, &data)
    }

    fn delete(&self, domain: &str) -> Result<()> {
        let key = Self::domain_key(domain);
        self.storage.delete_cf(&self.column_family, &key)
    }

    fn list_domains(&self) -> Result<Vec<String>> {
        let prefix = b"robots:";
        let entries = self.storage.prefix_iter_cf(&self.column_family, prefix)?;

        let domains: Vec<String> = entries
            .into_iter()
            .filter_map(|(key, _)| {
                String::from_utf8(key)
                    .ok()
                    .and_then(|s| s.strip_prefix("robots:").map(|d| d.to_string()))
            })
            .collect();

        Ok(domains)
    }

    fn count(&self) -> Result<usize> {
        Ok(self.list_domains()?.len())
    }

    fn clear(&self) -> Result<()> {
        for domain in self.list_domains()? {
            self.delete(&domain)?;
        }
        Ok(())
    }
}

/// Persistent robots.txt cache backed by RocksDB
///
/// This cache provides:
/// - Fast in-memory lookups with L1 cache
/// - Persistent storage in RocksDB for L2 cache
/// - Automatic expiration based on TTL
/// - Sitemap discovery from robots.txt
pub struct PersistentRobotsCache {
    config: RobotsConfig,
    client: Client,
    /// L1 in-memory cache for hot entries
    memory_cache: RwLock<HashMap<String, CachedRobotsMemory>>,
    /// L2 persistent storage
    persistence: Arc<dyn RobotsPersistence>,
}

/// In-memory cache entry (hot cache)
struct CachedRobotsMemory {
    content: String,
    cached_at: Instant,
    crawl_delay_ms: Option<u64>,
    sitemaps: Vec<String>,
}

impl PersistentRobotsCache {
    /// Create a new persistent robots cache
    pub fn new(config: RobotsConfig, persistence: Arc<dyn RobotsPersistence>) -> Result<Self> {
        let client = crate::safe_client::safe_client_builder(None, config.allow_private_ips)
            .timeout(config.fetch_timeout)
            .user_agent(&config.user_agent)
            .build()
            .map_err(|e| ScrapixError::Crawl(format!("Failed to build robots client: {}", e)))?;

        Ok(Self {
            config,
            client,
            memory_cache: RwLock::new(HashMap::new()),
            persistence,
        })
    }

    /// Check if a URL is allowed by robots.txt
    #[instrument(skip(self))]
    pub async fn is_allowed(&self, url: &str) -> Result<bool> {
        if !self.config.respect_robots {
            return Ok(true);
        }

        let parsed = Url::parse(url)?;
        let key = robots_cache_key(&parsed)?;
        let domain = key.as_str();

        let robots_content = self.get_robots(domain, &parsed).await?;
        let path = parsed.path();

        let mut matcher = DefaultMatcher::default();
        let allowed =
            matcher.one_agent_allowed_by_robots(&robots_content, &self.config.user_agent, path);

        debug!(url, allowed, "Robots.txt check");
        Ok(allowed)
    }

    /// Get crawl delay for a domain
    pub async fn get_crawl_delay(&self, domain: &str) -> Result<Option<u64>> {
        let (robots_url, key) = domain_robots_url(domain)?;
        let domain = key.as_str();
        // Check L1 memory cache first
        {
            let cache = self.memory_cache.read();
            if let Some(entry) = cache.get(domain) {
                if entry.cached_at.elapsed() < self.config.cache_ttl {
                    return Ok(entry.crawl_delay_ms.or(self.config.default_crawl_delay_ms));
                }
            }
        }

        // Check L2 persistent cache
        if let Some(entry) = self.persistence.get(domain)? {
            if !entry.is_expired(self.config.cache_ttl) {
                // Promote to L1 cache
                self.update_memory_cache(domain, &entry);
                return Ok(entry.crawl_delay_ms.or(self.config.default_crawl_delay_ms));
            }
        }

        let _ = self.get_robots(domain, &robots_url).await?;

        // Now check L1 cache
        let cache = self.memory_cache.read();
        if let Some(entry) = cache.get(domain) {
            return Ok(entry.crawl_delay_ms.or(self.config.default_crawl_delay_ms));
        }

        Ok(self.config.default_crawl_delay_ms)
    }

    /// Get sitemap URLs discovered from robots.txt
    pub async fn get_sitemaps(&self, domain: &str) -> Result<Vec<String>> {
        let (robots_url, key) = domain_robots_url(domain)?;
        let domain = key.as_str();
        // Check L1 memory cache first
        {
            let cache = self.memory_cache.read();
            if let Some(entry) = cache.get(domain) {
                if entry.cached_at.elapsed() < self.config.cache_ttl {
                    return Ok(entry.sitemaps.clone());
                }
            }
        }

        // Check L2 persistent cache
        if let Some(entry) = self.persistence.get(domain)? {
            if !entry.is_expired(self.config.cache_ttl) {
                self.update_memory_cache(domain, &entry);
                return Ok(entry.sitemaps.clone());
            }
        }

        let _ = self.get_robots(domain, &robots_url).await?;

        let cache = self.memory_cache.read();
        if let Some(entry) = cache.get(domain) {
            return Ok(entry.sitemaps.clone());
        }

        Ok(vec![])
    }

    /// Get robots.txt content for an origin. `domain` is the cache key from
    /// [`robots_cache_key`] (also the persistence key).
    async fn get_robots(&self, domain: &str, url: &Url) -> Result<String> {
        // Check L1 memory cache
        {
            let cache = self.memory_cache.read();
            if let Some(entry) = cache.get(domain) {
                if entry.cached_at.elapsed() < self.config.cache_ttl {
                    return Ok(entry.content.clone());
                }
            }
        }

        // Check L2 persistent cache
        if let Some(entry) = self.persistence.get(domain)? {
            if !entry.is_expired(self.config.cache_ttl) {
                self.update_memory_cache(domain, &entry);
                return Ok(entry.content.clone());
            }
        }

        // Fetch fresh
        let content = self.fetch_robots(url).await?;
        let crawl_delay_ms = self.parse_crawl_delay(&content);
        let sitemaps = self.parse_sitemaps(&content);

        // Store in both caches
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let persistent_entry = PersistentRobotsEntry {
            content: content.clone(),
            cached_at_unix: now_unix,
            crawl_delay_ms,
            sitemaps: sitemaps.clone(),
        };

        // Store in L2 (persistent)
        if let Err(e) = self.persistence.put(domain, &persistent_entry) {
            warn!(domain, error = %e, "Failed to persist robots.txt cache");
        }

        // Store in L1 (memory)
        {
            let mut cache = self.memory_cache.write();
            cache.insert(
                domain.to_string(),
                CachedRobotsMemory {
                    content: content.clone(),
                    cached_at: Instant::now(),
                    crawl_delay_ms,
                    sitemaps,
                },
            );
        }

        Ok(content)
    }

    /// Fetch robots.txt from URL
    async fn fetch_robots(&self, url: &Url) -> Result<String> {
        // See the identical comment in `RobotsCache::get_robots`: raw-IP
        // hosts bypass `SafeResolver` entirely and must be refused here too.
        crate::safe_client::reject_ip_host(url)?;
        Ok(fetch_robots_body(&self.client, &self.config, &robots_txt_url(url)).await)
    }

    /// Parse crawl delay from robots.txt
    fn parse_crawl_delay(&self, content: &str) -> Option<u64> {
        let mut in_user_agent_section = false;
        let user_agent_lower = self.config.user_agent.to_lowercase();

        for line in content.lines() {
            let line = line.trim().to_lowercase();

            if line.starts_with("user-agent:") {
                let agent = line.trim_start_matches("user-agent:").trim();
                in_user_agent_section = agent == "*" || agent == user_agent_lower;
            } else if in_user_agent_section && line.starts_with("crawl-delay:") {
                let delay_str = line.trim_start_matches("crawl-delay:").trim();
                if let Ok(delay) = delay_str.parse::<f64>() {
                    if delay < 0.0 || delay.is_nan() || delay.is_infinite() {
                        return None;
                    }
                    return Some((delay * 1000.0) as u64);
                }
            }
        }

        None
    }

    /// Parse sitemap URLs from robots.txt
    fn parse_sitemaps(&self, content: &str) -> Vec<String> {
        content
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.to_lowercase().starts_with("sitemap:") {
                    Some(line[8..].trim().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Update L1 memory cache from persistent entry
    fn update_memory_cache(&self, domain: &str, entry: &PersistentRobotsEntry) {
        let mut cache = self.memory_cache.write();
        cache.insert(
            domain.to_string(),
            CachedRobotsMemory {
                content: entry.content.clone(),
                cached_at: Instant::now(),
                crawl_delay_ms: entry.crawl_delay_ms,
                sitemaps: entry.sitemaps.clone(),
            },
        );
    }

    /// Clear all caches
    pub fn clear_cache(&self) -> Result<()> {
        {
            let mut cache = self.memory_cache.write();
            cache.clear();
        }
        self.persistence.clear()
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> Result<RobotsCacheStats> {
        let memory_cache = self.memory_cache.read();
        let memory_total = memory_cache.len();
        let memory_valid = memory_cache
            .values()
            .filter(|e| e.cached_at.elapsed() < self.config.cache_ttl)
            .count();

        let persistent_total = self.persistence.count()?;

        Ok(RobotsCacheStats {
            memory_total,
            memory_valid,
            persistent_total,
        })
    }

    /// Prefetch robots.txt for multiple domains
    pub async fn prefetch(&self, domains: &[String]) -> Result<()> {
        for domain in domains {
            if let Ok((robots_url, key)) = domain_robots_url(domain) {
                let _ = self.get_robots(&key, &robots_url).await;
            }
        }
        Ok(())
    }

    /// Load all persistent entries into memory cache
    pub fn warm_memory_cache(&self) -> Result<usize> {
        let domains = self.persistence.list_domains()?;
        let mut loaded = 0;

        for domain in &domains {
            if let Some(entry) = self.persistence.get(domain)? {
                if !entry.is_expired(self.config.cache_ttl) {
                    self.update_memory_cache(domain, &entry);
                    loaded += 1;
                }
            }
        }

        Ok(loaded)
    }
}

/// Statistics about the robots.txt cache
#[derive(Debug, Clone)]
pub struct RobotsCacheStats {
    /// Total entries in memory cache
    pub memory_total: usize,
    /// Valid (non-expired) entries in memory cache
    pub memory_valid: usize,
    /// Total entries in persistent cache
    pub persistent_total: usize,
}

#[cfg(test)]
mod persistent_tests {
    use super::*;

    /// In-memory implementation for testing
    struct MemoryRobotsPersistence {
        data: RwLock<HashMap<String, PersistentRobotsEntry>>,
    }

    impl MemoryRobotsPersistence {
        fn new() -> Self {
            Self {
                data: RwLock::new(HashMap::new()),
            }
        }
    }

    impl RobotsPersistence for MemoryRobotsPersistence {
        fn get(&self, domain: &str) -> Result<Option<PersistentRobotsEntry>> {
            Ok(self.data.read().get(domain).cloned())
        }

        fn put(&self, domain: &str, entry: &PersistentRobotsEntry) -> Result<()> {
            self.data.write().insert(domain.to_string(), entry.clone());
            Ok(())
        }

        fn delete(&self, domain: &str) -> Result<()> {
            self.data.write().remove(domain);
            Ok(())
        }

        fn list_domains(&self) -> Result<Vec<String>> {
            Ok(self.data.read().keys().cloned().collect())
        }

        fn count(&self) -> Result<usize> {
            Ok(self.data.read().len())
        }

        fn clear(&self) -> Result<()> {
            self.data.write().clear();
            Ok(())
        }
    }

    #[test]
    fn test_persistent_entry_expiration() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let entry = PersistentRobotsEntry {
            content: "User-agent: *\nDisallow:".to_string(),
            cached_at_unix: now,
            crawl_delay_ms: None,
            sitemaps: vec![],
        };

        // Fresh entry should not be expired
        assert!(!entry.is_expired(Duration::from_secs(3600)));

        // Old entry should be expired
        let old_entry = PersistentRobotsEntry {
            cached_at_unix: now - 7200, // 2 hours ago
            ..entry.clone()
        };
        assert!(old_entry.is_expired(Duration::from_secs(3600)));
    }

    #[test]
    fn test_parse_sitemaps() {
        let config = RobotsConfig::default();
        let persistence = Arc::new(MemoryRobotsPersistence::new());
        let cache = PersistentRobotsCache::new(config, persistence).unwrap();

        let content = r#"
User-agent: *
Disallow: /admin/

Sitemap: https://example.com/sitemap.xml
Sitemap: https://example.com/sitemap-news.xml
"#;

        let sitemaps = cache.parse_sitemaps(content);
        assert_eq!(sitemaps.len(), 2);
        assert!(sitemaps.contains(&"https://example.com/sitemap.xml".to_string()));
        assert!(sitemaps.contains(&"https://example.com/sitemap-news.xml".to_string()));
    }

    #[test]
    fn test_memory_persistence() {
        let persistence = MemoryRobotsPersistence::new();

        let entry = PersistentRobotsEntry {
            content: "User-agent: *".to_string(),
            cached_at_unix: 12345,
            crawl_delay_ms: Some(1000),
            sitemaps: vec!["https://example.com/sitemap.xml".to_string()],
        };

        // Put and get
        persistence.put("example.com", &entry).unwrap();
        let retrieved = persistence.get("example.com").unwrap().unwrap();
        assert_eq!(retrieved.content, entry.content);
        assert_eq!(retrieved.crawl_delay_ms, entry.crawl_delay_ms);

        // List domains
        persistence.put("other.com", &entry).unwrap();
        let domains = persistence.list_domains().unwrap();
        assert_eq!(domains.len(), 2);

        // Count
        assert_eq!(persistence.count().unwrap(), 2);

        // Delete
        persistence.delete("example.com").unwrap();
        assert_eq!(persistence.count().unwrap(), 1);

        // Clear
        persistence.clear().unwrap();
        assert_eq!(persistence.count().unwrap(), 0);
    }
}
