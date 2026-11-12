//! Sitemap parsing and discovery
//!
//! Supports:
//! - Standard XML sitemaps (urlset)
//! - Sitemap index files (sitemapindex)
//! - RSS / Atom feeds and plain-text sitemaps (one URL per line)
//! - Auto-discovery: robots.txt, the usual file names at the site root and
//!   in the folder of the start URL, on the host the site answers on
//! - Priority and lastmod extraction
//!
//! ## Example
//!
//! ```rust,no_run
//! use scrapix_crawler::sitemap::{SitemapParser, SitemapConfig};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let parser = SitemapParser::new(SitemapConfig::default());
//!
//!     // Parse a sitemap
//!     let urls = parser.fetch_and_parse("https://example.com/sitemap.xml").await?;
//!     for url in urls {
//!         println!("{} (priority: {:?})", url.loc, url.priority);
//!     }
//!
//!     // Auto-discover from robots.txt
//!     let sitemaps = parser.discover_from_robots("https://example.com").await?;
//!     Ok(())
//! }
//! ```

use std::collections::HashSet;
use std::io::BufRead;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::Client;
use tracing::{debug, instrument, warn};
use url::Url;

use scrapix_core::{Result, ScrapixError};

/// Sitemaps (from robots.txt, or the children of a sitemap index) fetched
/// at once. Results are still merged in order.
const SITEMAP_FETCH_CONCURRENCY: usize = 8;

/// File names tried at the site root when robots.txt declares no sitemap
/// (or only sitemaps that give nothing), in order of preference.
const DEFAULT_SITEMAP_NAMES: [&str; 5] = [
    "sitemap.xml",
    "sitemap_index.xml",
    "sitemap-index.xml",
    "sitemap.xml.gz",
    "wp-sitemap.xml",
];

/// Longest wait for robots.txt during discovery: it only names sitemaps,
/// and the usual locations are tried without it.
const ROBOTS_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Sitemap files already fetched during one discovery, by URL (requested
/// and after redirects), so a file reachable twice is read once.
type SeenSitemaps = Mutex<HashSet<String>>;

/// Whether two hosts name the same site, ignoring case and a leading
/// `www.` (`example.com` and `www.example.com`).
pub fn hosts_match_ignoring_www(a: &str, b: &str) -> bool {
    fn bare(host: &str) -> &str {
        if host.len() > 4 && host[..4].eq_ignore_ascii_case("www.") {
            &host[4..]
        } else {
            host
        }
    }
    bare(a).eq_ignore_ascii_case(bare(b))
}

/// The folder a start URL points into, with a leading and a trailing `/`:
/// `/docs/` for `/docs`, `/docs/` and `/docs/intro.html`. `None` at the
/// site root. A last segment without a `.` is taken as a folder.
pub fn section_prefix(url: &Url) -> Option<String> {
    let path = url.path();
    let folder = if path.ends_with('/') {
        path.to_string()
    } else {
        let (parent, last) = path.rsplit_once('/')?;
        if last.contains('.') {
            format!("{parent}/")
        } else {
            format!("{path}/")
        }
    };
    (folder != "/").then_some(folder)
}

/// What sitemap discovery found for a site.
#[derive(Debug, Default)]
pub struct SitemapDiscovery {
    /// The URLs, without duplicates, highest priority first.
    pub urls: Vec<SitemapUrl>,
    /// The sitemaps (as declared or tried, not their children) that gave
    /// at least one URL.
    pub sitemaps: Vec<String>,
    /// Origin the sitemaps were looked up on: the start URL's, or the one
    /// its robots.txt redirected to when it differs by `www.` or scheme.
    pub origin: String,
}

/// robots.txt as discovery reads it.
struct RobotsSitemaps {
    /// See [`SitemapDiscovery::origin`].
    origin: Url,
    /// `Sitemap:` lines, resolved.
    declared: Vec<String>,
}

/// Configuration for sitemap parsing
#[derive(Debug, Clone)]
pub struct SitemapConfig {
    /// User agent for requests
    pub user_agent: String,
    /// Request timeout
    pub timeout: Duration,
    /// Maximum sitemap size in bytes (default 50MB)
    pub max_size: usize,
    /// Maximum URLs to extract from a single sitemap
    pub max_urls: usize,
    /// Maximum depth for sitemap index recursion
    pub max_depth: u32,
    /// Follow sitemap index files
    pub follow_index: bool,
    /// Whether to allow fetching sitemaps/robots.txt from hosts that resolve
    /// to private/internal IP ranges. Defaults to `false` (deny) for SSRF
    /// safety, matching `FetcherConfig::allow_private_ips`.
    pub allow_private_ips: bool,
}

impl Default for SitemapConfig {
    fn default() -> Self {
        Self {
            user_agent: "Scrapix/1.0".to_string(),
            timeout: Duration::from_secs(30),
            max_size: 50 * 1024 * 1024, // 50MB
            max_urls: 100_000,
            max_depth: 3,
            follow_index: true,
            allow_private_ips: false,
        }
    }
}

/// A URL entry from a sitemap
#[derive(Debug, Clone)]
pub struct SitemapUrl {
    /// The URL location
    pub loc: String,
    /// Last modification time
    pub lastmod: Option<DateTime<Utc>>,
    /// Change frequency hint
    pub changefreq: Option<ChangeFrequency>,
    /// Priority (0.0 to 1.0)
    pub priority: Option<f32>,
}

impl SitemapUrl {
    /// Create a new sitemap URL
    pub fn new(loc: impl Into<String>) -> Self {
        Self {
            loc: loc.into(),
            lastmod: None,
            changefreq: None,
            priority: None,
        }
    }

    /// Get the priority or default (0.5)
    pub fn priority_or_default(&self) -> f32 {
        self.priority.unwrap_or(0.5)
    }
}

/// Change frequency hint from sitemap
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeFrequency {
    Always,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
    Never,
}

impl ChangeFrequency {
    /// Parse from string
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "always" => Some(Self::Always),
            "hourly" => Some(Self::Hourly),
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "monthly" => Some(Self::Monthly),
            "yearly" => Some(Self::Yearly),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// Convert to approximate seconds
    pub fn to_seconds(&self) -> Option<u64> {
        match self {
            Self::Always => Some(0),
            Self::Hourly => Some(3600),
            Self::Daily => Some(86400),
            Self::Weekly => Some(604800),
            Self::Monthly => Some(2592000),
            Self::Yearly => Some(31536000),
            Self::Never => None,
        }
    }
}

/// A sitemap entry (URL to a sitemap file)
#[derive(Debug, Clone)]
pub struct SitemapEntry {
    /// Location of the sitemap
    pub loc: String,
    /// Last modification time
    pub lastmod: Option<DateTime<Utc>>,
}

/// Result of parsing a sitemap
#[derive(Debug)]
pub enum SitemapContent {
    /// A regular sitemap with URLs
    UrlSet(Vec<SitemapUrl>),
    /// A sitemap index with links to other sitemaps
    Index(Vec<SitemapEntry>),
}

/// Sitemap parser
pub struct SitemapParser {
    config: SitemapConfig,
    client: Client,
}

impl SitemapParser {
    /// Create a new sitemap parser
    pub fn new(config: SitemapConfig) -> Self {
        let client = crate::safe_client::safe_client_builder(None, config.allow_private_ips)
            .timeout(config.timeout)
            .user_agent(&config.user_agent)
            .gzip(true)
            .build()
            .expect("Failed to build HTTP client");

        Self { config, client }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(SitemapConfig::default())
    }

    /// Discover sitemaps from robots.txt: its `Sitemap:` lines, or
    /// `/sitemap.xml` when it declares none or cannot be read.
    #[instrument(skip(self))]
    pub async fn discover_from_robots(&self, base_url: &str) -> Result<Vec<String>> {
        let parsed = Url::parse(base_url)?;
        // Raw-IP hosts bypass `SafeResolver` entirely, so they must be
        // refused explicitly — `base_url` here can be user-supplied (e.g.
        // the /map endpoint's request URL).
        crate::safe_client::reject_ip_host(&parsed)?;
        let robots = self.read_robots(&parsed).await;
        if robots.declared.is_empty() {
            return Ok(vec![root_file(&robots.origin, DEFAULT_SITEMAP_NAMES[0])]);
        }
        Ok(robots.declared)
    }

    /// GET `robots.txt` on `origin`'s host. `None` on a network error.
    async fn get_robots(&self, origin: &Url) -> Option<reqwest::Response> {
        let robots_url = root_file(origin, "robots.txt");
        debug!(robots_url, "Fetching robots.txt for sitemap discovery");
        match self
            .client
            .get(&robots_url)
            .timeout(self.config.timeout.min(ROBOTS_DISCOVERY_TIMEOUT))
            .send()
            .await
        {
            Ok(response) => Some(response),
            Err(e) => {
                debug!(robots_url, error = %e, "robots.txt not reachable");
                None
            }
        }
    }

    /// Read the site's robots.txt for its sitemaps and for the host the
    /// site answers on. Never fails: without a robots.txt there is nothing
    /// declared, and the start URL's origin is kept.
    ///
    /// When the host does not answer at all, the same host with `www.`
    /// added (or removed) is tried once.
    async fn read_robots(&self, start: &Url) -> RobotsSitemaps {
        // `origin()` keeps a non-default port (`host_str()` alone drops it).
        let mut origin =
            Url::parse(&start.origin().ascii_serialization()).unwrap_or_else(|_| start.clone());
        let mut response = self.get_robots(&origin).await;
        if response.is_none() {
            if let Some(other) = www_toggled(&origin) {
                response = self.get_robots(&other).await;
                if response.is_some() {
                    origin = other;
                }
            }
        }
        let Some(response) = response else {
            return RobotsSitemaps {
                origin,
                declared: Vec::new(),
            };
        };

        // A redirect to the `www.` (or bare) host, or to https, tells where
        // the site lives. A redirect anywhere else is not followed for the
        // other locations.
        let final_url = response.url().clone();
        if let (Some(to), Some(from)) = (final_url.host_str(), origin.host_str()) {
            if hosts_match_ignoring_www(to, from) {
                if let Ok(redirected) = Url::parse(&final_url.origin().ascii_serialization()) {
                    origin = redirected;
                }
            }
        }

        let mut declared = Vec::new();
        if response.status().is_success() {
            let content = response.text().await.unwrap_or_default();
            for line in content.lines() {
                let line = line.trim();
                // `get`: a multi-byte character can sit across byte 8.
                let Some(directive) = line.get(..8) else {
                    continue;
                };
                if !directive.eq_ignore_ascii_case("sitemap:") {
                    continue;
                }
                let sitemap_url = line[8..].trim();
                if sitemap_url.is_empty() {
                    continue;
                }
                let resolved = match final_url.join(sitemap_url) {
                    Ok(resolved) => resolved.to_string(),
                    Err(_) => sitemap_url.to_string(),
                };
                if !declared.contains(&resolved) {
                    declared.push(resolved);
                }
            }
        }
        debug!(count = declared.len(), origin = %origin, "Sitemaps declared in robots.txt");
        RobotsSitemaps { origin, declared }
    }

    /// Fetch and parse a sitemap, following index files if configured
    #[instrument(skip(self))]
    pub async fn fetch_and_parse(&self, url: &str) -> Result<Vec<SitemapUrl>> {
        let seen = SeenSitemaps::default();
        self.fetch_and_parse_recursive(url, 0, &seen).await
    }

    /// Recursive sitemap parsing with depth limit
    fn fetch_and_parse_recursive<'a>(
        &'a self,
        url: &'a str,
        depth: u32,
        seen: &'a SeenSitemaps,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<SitemapUrl>>> + Send + 'a>>
    {
        Box::pin(async move {
            if depth > self.config.max_depth {
                warn!(url, depth, "Max sitemap depth exceeded");
                return Ok(Vec::new());
            }

            if !seen.lock().unwrap().insert(url.to_string()) {
                return Ok(Vec::new());
            }
            debug!(url, depth, "Fetching sitemap");

            let (content, final_url) = self.fetch_sitemap(url).await?;
            // Two names for one file (a redirect): read it once.
            if final_url != url && !seen.lock().unwrap().insert(final_url) {
                return Ok(Vec::new());
            }
            let parsed = self.parse_content(&content)?;

            match parsed {
                SitemapContent::UrlSet(urls) => {
                    let count = urls.len().min(self.config.max_urls);
                    debug!(url, count, "Parsed URL sitemap");
                    Ok(urls.into_iter().take(self.config.max_urls).collect())
                }
                SitemapContent::Index(entries) => {
                    if !self.config.follow_index {
                        debug!(url, "Sitemap index found but follow_index is disabled");
                        return Ok(Vec::new());
                    }

                    debug!(url, count = entries.len(), "Parsed sitemap index");

                    // Children are fetched a few at a time but merged in
                    // index order, so the result is the same as a
                    // sequential walk; the ones not yet started are skipped
                    // once `max_urls` is reached.
                    // (Built up front as boxed futures: a `.map` closure
                    // here trips rustc's higher-ranked `Send` check on this
                    // recursive future.)
                    type Child<'e> = std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = (&'e SitemapEntry, Result<Vec<SitemapUrl>>),
                                > + Send
                                + 'e,
                        >,
                    >;
                    let pending: Vec<Child<'_>> = entries
                        .iter()
                        .map(|entry| {
                            let child = self.fetch_and_parse_recursive(&entry.loc, depth + 1, seen);
                            Box::pin(async move { (entry, child.await) }) as Child<'_>
                        })
                        .collect();
                    let mut children = stream::iter(pending).buffered(SITEMAP_FETCH_CONCURRENCY);
                    let mut all_urls = Vec::new();
                    while let Some((entry, result)) = children.next().await {
                        match result {
                            Ok(urls) => {
                                all_urls.extend(urls);
                                if all_urls.len() >= self.config.max_urls {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!(sitemap = entry.loc, error = %e, "Failed to parse sub-sitemap");
                            }
                        }
                    }

                    Ok(all_urls.into_iter().take(self.config.max_urls).collect())
                }
            }
        })
    }

    /// Fetch a sitemap: its content and the URL it was served from.
    async fn fetch_sitemap(&self, url: &str) -> Result<(String, String)> {
        // Raw-IP hosts bypass `SafeResolver` entirely, so they must be
        // refused explicitly. This also covers sitemap-index entries, whose
        // `loc` values are attacker-controlled content from a fetched
        // sitemap (a malicious index could point sub-sitemap entries at an
        // internal address).
        crate::safe_client::reject_ip_host(&Url::parse(url)?)?;

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ScrapixError::Crawl(format!("Failed to fetch sitemap: {}", e)))?;

        if !response.status().is_success() {
            return Err(ScrapixError::Crawl(format!(
                "Sitemap returned status {}",
                response.status()
            )));
        }

        // Check content length
        if let Some(len) = response.content_length() {
            if len as usize > self.config.max_size {
                return Err(ScrapixError::Crawl(format!(
                    "Sitemap too large: {} bytes (max {})",
                    len, self.config.max_size
                )));
            }
        }

        let final_url = response.url().to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ScrapixError::Crawl(format!("Failed to read sitemap: {}", e)))?;

        // Check if gzipped (magic bytes: 1f 8b)
        if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
            // Decompress gzip
            // Read one byte past the limit to tell "too large" apart.
            let limit = self.config.max_size as u64 + 1;
            let mut decoder = std::io::Read::take(flate2::read::GzDecoder::new(&bytes[..]), limit);
            let mut content = String::new();
            std::io::Read::read_to_string(&mut decoder, &mut content)
                .map_err(|e| ScrapixError::Crawl(format!("Failed to decompress sitemap: {}", e)))?;
            if content.len() > self.config.max_size {
                return Err(ScrapixError::Crawl(format!(
                    "Sitemap too large once decompressed (max {} bytes)",
                    self.config.max_size
                )));
            }
            Ok((content, final_url))
        } else {
            String::from_utf8(bytes.to_vec())
                .map(|content| (content, final_url))
                .map_err(|e| ScrapixError::Crawl(format!("Invalid UTF-8 in sitemap: {}", e)))
        }
    }

    /// Parse what a sitemap URL served: XML (urlset, sitemap index, RSS or
    /// Atom feed) or plain text, one URL per line. Anything else, such as
    /// an HTML page served for a missing sitemap, is an error.
    fn parse_content(&self, content: &str) -> Result<SitemapContent> {
        let content = content.trim_start_matches('\u{feff}').trim_start();
        if content.starts_with('<') {
            self.parse_xml(content)
        } else {
            parse_text_sitemap(content)
        }
    }

    /// Parse sitemap XML content
    fn parse_xml(&self, content: &str) -> Result<SitemapContent> {
        let mut reader = Reader::from_str(content);
        reader.config_mut().trim_text(true);

        // Detect sitemap type by looking for root element
        let mut buf = Vec::new();
        let mut root = String::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    root = String::from_utf8_lossy(e.name().local_name().as_ref())
                        .to_ascii_lowercase();
                    break;
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(ScrapixError::Crawl(format!("XML parse error: {}", e)));
                }
                _ => {}
            }
            buf.clear();
        }

        // Reset reader
        let mut reader = Reader::from_str(content);
        reader.config_mut().trim_text(true);

        match root.as_str() {
            "sitemapindex" => self.parse_sitemap_index(&mut reader),
            "urlset" => self.parse_urlset(&mut reader),
            "rss" | "feed" | "rdf" => Ok(parse_feed(&mut reader)),
            "" => Err(ScrapixError::Crawl("Not a sitemap: empty document".into())),
            other => Err(ScrapixError::Crawl(format!(
                "Not a sitemap: root element <{other}>"
            ))),
        }
    }

    /// Parse a urlset sitemap
    fn parse_urlset<B: BufRead>(&self, reader: &mut Reader<B>) -> Result<SitemapContent> {
        let mut urls = Vec::new();
        let mut buf = Vec::new();

        let mut current_url: Option<SitemapUrl> = None;
        let mut current_tag = String::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let name = e.name();
                    let local = String::from_utf8_lossy(name.local_name().as_ref()).to_string();
                    current_tag = local.clone();

                    if local == "url" {
                        current_url = Some(SitemapUrl::new(""));
                    }
                }
                Ok(Event::Text(e)) => {
                    if let Some(ref mut url) = current_url {
                        let text = e.unescape().unwrap_or_default().to_string();
                        match current_tag.as_str() {
                            "loc" => url.loc = text,
                            "lastmod" => {
                                url.lastmod = parse_datetime(&text);
                            }
                            "changefreq" => {
                                url.changefreq = ChangeFrequency::parse(&text);
                            }
                            "priority" => {
                                url.priority = text.parse().ok();
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Event::End(e)) => {
                    let name = e.name();
                    let local = String::from_utf8_lossy(name.local_name().as_ref()).to_string();
                    if local == "url" {
                        if let Some(url) = current_url.take() {
                            if !url.loc.is_empty() {
                                urls.push(url);
                            }
                        }
                    }
                    current_tag.clear();
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    warn!(error = %e, "XML parse error, returning partial results");
                    break;
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(SitemapContent::UrlSet(urls))
    }

    /// Parse a sitemap index
    fn parse_sitemap_index<B: BufRead>(&self, reader: &mut Reader<B>) -> Result<SitemapContent> {
        let mut entries = Vec::new();
        let mut buf = Vec::new();

        let mut current_entry: Option<SitemapEntry> = None;
        let mut current_tag = String::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let name = e.name();
                    let local = String::from_utf8_lossy(name.local_name().as_ref()).to_string();
                    current_tag = local.clone();

                    if local == "sitemap" {
                        current_entry = Some(SitemapEntry {
                            loc: String::new(),
                            lastmod: None,
                        });
                    }
                }
                Ok(Event::Text(e)) => {
                    if let Some(ref mut entry) = current_entry {
                        let text = e.unescape().unwrap_or_default().to_string();
                        match current_tag.as_str() {
                            "loc" => entry.loc = text,
                            "lastmod" => {
                                entry.lastmod = parse_datetime(&text);
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Event::End(e)) => {
                    let name = e.name();
                    let local = String::from_utf8_lossy(name.local_name().as_ref()).to_string();
                    if local == "sitemap" {
                        if let Some(entry) = current_entry.take() {
                            if !entry.loc.is_empty() {
                                entries.push(entry);
                            }
                        }
                    }
                    current_tag.clear();
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    warn!(error = %e, "XML parse error, returning partial results");
                    break;
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(SitemapContent::Index(entries))
    }

    /// Discover and fetch all URLs from a site's sitemaps (see
    /// [`Self::discover`]).
    #[instrument(skip(self))]
    pub async fn discover_all_urls(&self, base_url: &str) -> Result<Vec<SitemapUrl>> {
        Ok(self.discover(base_url).await?.urls)
    }

    /// Find a site's sitemaps and read their URLs.
    ///
    /// 1. robots.txt gives the declared sitemaps and the host the site
    ///    answers on (`www.` or not). It may be missing or unreachable.
    /// 2. `sitemap.xml` in the folder of `start_url` and in its first
    ///    folder is read, then the declared sitemaps, or without any the
    ///    usual file names at the root ([`DEFAULT_SITEMAP_NAMES`]).
    /// 3. If declared sitemaps gave no URL, the usual names are tried.
    ///
    /// A location that is missing, or serves something that is not a
    /// sitemap, is skipped. Only an invalid or raw-IP `start_url` fails.
    #[instrument(skip(self))]
    pub async fn discover(&self, start_url: &str) -> Result<SitemapDiscovery> {
        let start = Url::parse(start_url)?;
        crate::safe_client::reject_ip_host(&start)?;

        let robots = self.read_robots(&start).await;
        let defaults: Vec<String> = DEFAULT_SITEMAP_NAMES
            .iter()
            .map(|name| root_file(&robots.origin, name))
            .collect();

        // The section's own sitemaps go first: on a large site the root
        // sitemap alone can reach `max_urls`.
        let mut first = section_sitemaps(&robots.origin, &start);
        let site_wide = if robots.declared.is_empty() {
            &defaults
        } else {
            &robots.declared
        };
        for sitemap in site_wide {
            if !first.contains(sitemap) {
                first.push(sitemap.clone());
            }
        }

        let seen = SeenSitemaps::default();
        let mut found = SitemapDiscovery {
            origin: robots.origin.origin().ascii_serialization(),
            ..Default::default()
        };
        let mut locs = HashSet::new();
        self.read_roots(&first, &seen, &mut locs, &mut found).await;
        if found.urls.is_empty() && !robots.declared.is_empty() {
            self.read_roots(&defaults, &seen, &mut locs, &mut found)
                .await;
        }

        // Highest priority first; the sort is stable, so sitemap order is
        // kept among equals.
        found.urls.sort_by(|a, b| {
            b.priority_or_default()
                .partial_cmp(&a.priority_or_default())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        found.urls.truncate(self.config.max_urls);
        debug!(
            urls = found.urls.len(),
            sitemaps = found.sitemaps.len(),
            "Sitemap discovery done"
        );
        Ok(found)
    }

    /// Read `roots` a few at a time, merged in order, into `found`; `locs`
    /// holds the URLs already found. Stops once `max_urls` are found.
    async fn read_roots(
        &self,
        roots: &[String],
        seen: &SeenSitemaps,
        locs: &mut HashSet<String>,
        found: &mut SitemapDiscovery,
    ) {
        // Boxed up front, like the index children in
        // `fetch_and_parse_recursive`, to keep the future `Send` for spawns.
        type Fetch<'s> = std::pin::Pin<
            Box<
                dyn std::future::Future<Output = (&'s String, Result<Vec<SitemapUrl>>)> + Send + 's,
            >,
        >;
        let pending: Vec<Fetch<'_>> = roots
            .iter()
            .map(|sitemap| {
                let fetch = self.fetch_and_parse_recursive(sitemap, 0, seen);
                Box::pin(async move { (sitemap, fetch.await) }) as Fetch<'_>
            })
            .collect();
        let mut fetches = stream::iter(pending).buffered(SITEMAP_FETCH_CONCURRENCY);
        while let Some((sitemap, result)) = fetches.next().await {
            match result {
                Ok(urls) => {
                    let before = found.urls.len();
                    for url in urls {
                        if locs.insert(url.loc.clone()) {
                            found.urls.push(url);
                        }
                    }
                    if found.urls.len() > before {
                        found.sitemaps.push(sitemap.clone());
                    }
                    if found.urls.len() >= self.config.max_urls {
                        break;
                    }
                }
                // Expected for the locations tried on the off chance.
                Err(e) => debug!(sitemap, error = %e, "No sitemap read at this location"),
            }
        }
    }
}

/// `{origin}/{name}`.
fn root_file(origin: &Url, name: &str) -> String {
    format!("{}/{name}", origin.origin().ascii_serialization())
}

/// `origin` with `www.` added to its host, or removed from it.
fn www_toggled(origin: &Url) -> Option<Url> {
    let host = origin.host_str()?;
    let other = match host.get(..4) {
        Some(prefix) if prefix.eq_ignore_ascii_case("www.") => host[4..].to_string(),
        _ => format!("www.{host}"),
    };
    let mut toggled = origin.clone();
    toggled.set_host(Some(&other)).ok()?;
    Some(toggled)
}

/// `sitemap.xml` in the folder `start` points into and in its first
/// folder (`/docs/guide/x` gives `/docs/guide/x/` and `/docs/`): sites
/// often publish one per section.
fn section_sitemaps(origin: &Url, start: &Url) -> Vec<String> {
    let Some(folder) = section_prefix(start) else {
        return Vec::new();
    };
    let base = origin.origin().ascii_serialization();
    let mut candidates = vec![format!("{base}{folder}sitemap.xml")];
    if let Some(first) = folder.trim_start_matches('/').split('/').next() {
        let top = format!("{base}/{first}/sitemap.xml");
        if !first.is_empty() && !candidates.contains(&top) {
            candidates.push(top);
        }
    }
    candidates
}

/// A plain-text sitemap: one http(s) URL per line.
fn parse_text_sitemap(content: &str) -> Result<SitemapContent> {
    let mut urls = Vec::new();
    let mut other_lines = 0usize;
    for line in content.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match Url::parse(line) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => {
                urls.push(SitemapUrl::new(line));
            }
            _ => other_lines += 1,
        }
    }
    if urls.is_empty() || other_lines > urls.len() {
        return Err(ScrapixError::Crawl(
            "Not a sitemap: neither XML nor a list of URLs".into(),
        ));
    }
    Ok(SitemapContent::UrlSet(urls))
}

/// The entries of an RSS (`<item><link>url</link>`) or Atom
/// (`<entry><link href="url"/>`) feed, with their date as `lastmod`.
fn parse_feed<B: BufRead>(reader: &mut Reader<B>) -> SitemapContent {
    fn local_name(e: &quick_xml::events::BytesStart<'_>) -> String {
        String::from_utf8_lossy(e.name().local_name().as_ref()).to_ascii_lowercase()
    }
    // An Atom link: `href`, unless `rel` says it is not the entry's page.
    fn atom_href(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
        let mut href = None;
        for attr in e.attributes().flatten() {
            let value = attr.unescape_value().ok()?.to_string();
            match attr.key.local_name().as_ref() {
                b"href" => href = Some(value),
                b"rel" if value != "alternate" => return None,
                _ => {}
            }
        }
        href
    }
    fn set_field(entry: &mut SitemapUrl, tag: &str, text: &str) {
        let text = text.trim();
        match tag {
            "link" if entry.loc.is_empty() => entry.loc = text.to_string(),
            "pubdate" => {
                entry.lastmod = DateTime::parse_from_rfc2822(text)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc));
            }
            "updated" | "date" => entry.lastmod = parse_datetime(text),
            _ => {}
        }
    }

    let mut urls = Vec::new();
    let mut buf = Vec::new();
    let mut current: Option<SitemapUrl> = None;
    let mut current_tag = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(&e);
                if local == "item" || local == "entry" {
                    current = Some(SitemapUrl::new(""));
                } else if let Some(entry) = current.as_mut() {
                    if local == "link" && entry.loc.is_empty() {
                        if let Some(href) = atom_href(&e) {
                            entry.loc = href;
                        }
                    }
                }
                current_tag = local;
            }
            Ok(Event::Empty(e)) => {
                if let Some(entry) = current.as_mut() {
                    if local_name(&e) == "link" && entry.loc.is_empty() {
                        if let Some(href) = atom_href(&e) {
                            entry.loc = href;
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if let Some(entry) = current.as_mut() {
                    set_field(entry, &current_tag, &e.unescape().unwrap_or_default());
                }
            }
            Ok(Event::CData(e)) => {
                if let Some(entry) = current.as_mut() {
                    set_field(entry, &current_tag, &String::from_utf8_lossy(&e));
                }
            }
            Ok(Event::End(e)) => {
                let local =
                    String::from_utf8_lossy(e.name().local_name().as_ref()).to_ascii_lowercase();
                if local == "item" || local == "entry" {
                    if let Some(entry) = current.take() {
                        if !entry.loc.is_empty() {
                            urls.push(entry);
                        }
                    }
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                warn!(error = %e, "XML parse error in feed, returning partial results");
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    SitemapContent::UrlSet(urls)
}

/// Parse various datetime formats used in sitemaps
fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    // Try ISO 8601 formats
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }

    // Try YYYY-MM-DD format
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc());
    }

    // Try YYYY-MM-DDTHH:MM:SS format without timezone
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_urlset() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <url>
                <loc>https://example.com/page1</loc>
                <lastmod>2024-01-15</lastmod>
                <changefreq>weekly</changefreq>
                <priority>0.8</priority>
            </url>
            <url>
                <loc>https://example.com/page2</loc>
                <priority>0.5</priority>
            </url>
        </urlset>"#;

        let parser = SitemapParser::with_defaults();
        let result = parser.parse_xml(xml).unwrap();

        match result {
            SitemapContent::UrlSet(urls) => {
                assert_eq!(urls.len(), 2);
                assert_eq!(urls[0].loc, "https://example.com/page1");
                assert_eq!(urls[0].priority, Some(0.8));
                assert!(urls[0].lastmod.is_some());
                assert_eq!(urls[0].changefreq, Some(ChangeFrequency::Weekly));
                assert_eq!(urls[1].loc, "https://example.com/page2");
            }
            _ => panic!("Expected UrlSet"),
        }
    }

    #[test]
    fn test_parse_sitemap_index() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <sitemap>
                <loc>https://example.com/sitemap1.xml</loc>
                <lastmod>2024-01-15</lastmod>
            </sitemap>
            <sitemap>
                <loc>https://example.com/sitemap2.xml</loc>
            </sitemap>
        </sitemapindex>"#;

        let parser = SitemapParser::with_defaults();
        let result = parser.parse_xml(xml).unwrap();

        match result {
            SitemapContent::Index(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].loc, "https://example.com/sitemap1.xml");
                assert!(entries[0].lastmod.is_some());
            }
            _ => panic!("Expected Index"),
        }
    }

    #[test]
    fn www_is_ignored_when_comparing_hosts() {
        assert!(hosts_match_ignoring_www("example.com", "www.example.com"));
        assert!(hosts_match_ignoring_www("WWW.Example.com", "example.COM"));
        assert!(hosts_match_ignoring_www(
            "docs.example.com",
            "docs.example.com"
        ));
        assert!(!hosts_match_ignoring_www("docs.example.com", "example.com"));
        assert!(!hosts_match_ignoring_www("www.", "example.com"));
    }

    #[test]
    fn section_prefix_is_the_folder_of_the_start_url() {
        let section = |u: &str| section_prefix(&Url::parse(u).unwrap());
        assert_eq!(section("https://example.com"), None);
        assert_eq!(section("https://example.com/"), None);
        assert_eq!(section("https://example.com/index.html"), None);
        assert_eq!(
            section("https://example.com/docs").as_deref(),
            Some("/docs/")
        );
        assert_eq!(
            section("https://example.com/docs/").as_deref(),
            Some("/docs/")
        );
        assert_eq!(
            section("https://example.com/docs/intro.html?x=1").as_deref(),
            Some("/docs/")
        );
    }

    #[test]
    fn section_sitemaps_cover_the_folder_and_its_first_ancestor() {
        let origin = Url::parse("https://example.com").unwrap();
        let start = Url::parse("https://example.com/docs/guide/x").unwrap();
        assert_eq!(
            section_sitemaps(&origin, &start),
            vec![
                "https://example.com/docs/guide/x/sitemap.xml",
                "https://example.com/docs/sitemap.xml"
            ]
        );
        let top = Url::parse("https://example.com/docs/").unwrap();
        assert_eq!(
            section_sitemaps(&origin, &top),
            vec!["https://example.com/docs/sitemap.xml"]
        );
    }

    #[test]
    fn content_that_is_not_a_sitemap_is_refused() {
        let parser = SitemapParser::with_defaults();
        assert!(parser
            .parse_content("<html><body>404</body></html>")
            .is_err());
        assert!(parser.parse_content("Not found").is_err());
        assert!(parser.parse_content("").is_err());
        // A byte-order mark and blank lines before the XML are fine.
        let xml = "\u{feff}\n<urlset><url><loc>https://example.com/a</loc></url></urlset>";
        assert!(matches!(
            parser.parse_content(xml),
            Ok(SitemapContent::UrlSet(urls)) if urls.len() == 1
        ));
    }

    #[test]
    fn test_change_frequency() {
        assert_eq!(
            ChangeFrequency::parse("daily"),
            Some(ChangeFrequency::Daily)
        );
        assert_eq!(
            ChangeFrequency::parse("WEEKLY"),
            Some(ChangeFrequency::Weekly)
        );
        assert_eq!(ChangeFrequency::parse("invalid"), None);

        assert_eq!(ChangeFrequency::Daily.to_seconds(), Some(86400));
        assert_eq!(ChangeFrequency::Never.to_seconds(), None);
    }

    #[test]
    fn test_parse_datetime() {
        // ISO 8601 with timezone
        assert!(parse_datetime("2024-01-15T10:30:00+00:00").is_some());
        // Date only
        assert!(parse_datetime("2024-01-15").is_some());
        // ISO without timezone
        assert!(parse_datetime("2024-01-15T10:30:00").is_some());
        // Invalid
        assert!(parse_datetime("not-a-date").is_none());
    }
}
