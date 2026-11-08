//! URL extraction from HTML pages

use std::collections::HashSet;

use scraper::{Html, Selector};
use tracing::debug;
use url::Url;

#[cfg(test)]
use scrapix_core::url_glob::matches_glob;
use scrapix_core::url_glob::matches_include_exclude;
use scrapix_core::url_normalize::normalize_link_url;
use scrapix_core::{CrawlUrl, RawPage, UrlPatterns};

/// Most followable links [`UrlExtractor::extract_with_outlinks`] returns
/// for a page to store (incremental crawling keeps them per page).
pub const MAX_STORED_LINKS: usize = 1_000;

/// The URL relative links of `document` resolve against: its first `<base
/// href>` (itself resolved against `page_url`) when that is an http(s) URL,
/// else `page_url`.
pub fn document_base_url(document: &Html, page_url: &Url) -> Url {
    static BASE: std::sync::OnceLock<Selector> = std::sync::OnceLock::new();
    let selector = BASE.get_or_init(|| Selector::parse("base[href]").expect("valid selector"));
    document
        .select(selector)
        .next()
        .and_then(|base| base.value().attr("href"))
        .and_then(|href| page_url.join(href.trim()).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or_else(|| page_url.clone())
}

/// URL extraction configuration
#[derive(Debug, Clone)]
pub struct ExtractorConfig {
    /// URL patterns for filtering
    pub patterns: Option<UrlPatterns>,
    /// Maximum depth to follow links
    pub max_depth: u32,
    /// Whether to follow external links (different domain)
    pub follow_external: bool,
    /// Whether to follow subdomains
    pub follow_subdomains: bool,
    /// Whether to extract links from specific attributes
    pub extract_from_data_attrs: bool,
    /// Explicit allowed domains whitelist (strict, no inference)
    /// When non-empty, ONLY these exact domains are allowed.
    /// This overrides follow_external and follow_subdomains.
    pub allowed_domains: Vec<String>,
}

impl Default for ExtractorConfig {
    fn default() -> Self {
        Self {
            patterns: None,
            max_depth: u32::MAX,
            follow_external: false,
            follow_subdomains: true,
            extract_from_data_attrs: false,
            allowed_domains: vec![],
        }
    }
}

/// URL extractor for finding links in HTML
pub struct UrlExtractor {
    config: ExtractorConfig,
    link_selector: Selector,
    /// Follow `.pdf` links (the job enables `features.pdf`).
    follow_pdf: bool,
    /// Follow office-document links (the job enables `features.documents`).
    follow_documents: bool,
}

impl UrlExtractor {
    /// Get the extractor configuration
    pub fn config(&self) -> &ExtractorConfig {
        &self.config
    }

    /// Create a new URL extractor
    pub fn new(config: ExtractorConfig) -> Self {
        // Selector for anchor tags with href
        let link_selector = Selector::parse("a[href]").unwrap();

        Self {
            config,
            link_selector,
            follow_pdf: false,
            follow_documents: false,
        }
    }

    /// Keep links to documents the job will parse instead of filtering
    /// them out as non-page resources: `.pdf` when `pdf`, office formats
    /// (`.docx`, `.xlsx`, `.pptx`, ...) when `documents`.
    pub fn follow_document_links(mut self, pdf: bool, documents: bool) -> Self {
        self.follow_pdf = pdf;
        self.follow_documents = documents;
        self
    }

    /// Filter already-resolved link targets (e.g. hyperlinks found inside a
    /// PDF, or the outlinks stored for a page that answered 304) with the
    /// same rules as HTML links: normalization, non-page filtering, domain
    /// constraints, URL patterns and depth.
    pub fn extract_from_links<I>(
        &self,
        final_url: &str,
        links: I,
        parent_depth: u32,
    ) -> Vec<CrawlUrl>
    where
        I: IntoIterator<Item = String>,
    {
        if parent_depth >= self.config.max_depth {
            return vec![];
        }
        let Ok(page_url) = Url::parse(final_url) else {
            return vec![];
        };
        let page_domain = page_url.host_str().unwrap_or("").to_lowercase();
        let mut seen = HashSet::new();
        links
            .into_iter()
            .filter_map(|href| {
                let (normalized, host) = self.resolve(&href, &page_url, &page_url, &page_domain)?;
                self.accept(&normalized, &host, &page_domain, &mut seen)
                    .then(|| self.crawl_url(normalized, final_url, parent_depth))
            })
            .collect()
    }

    /// Create a new URL extractor with default configuration
    pub fn with_defaults() -> Self {
        Self::new(ExtractorConfig::default())
    }

    /// Extract URLs from a page
    pub fn extract(&self, page: &RawPage, parent_depth: u32) -> Vec<CrawlUrl> {
        self.extract_with_outlinks(page, parent_depth).0
    }

    /// Extract the links to follow from a page, and the page's links to
    /// store: every link that passes the domain, URL-pattern and non-page
    /// filters, whatever the depth, capped at [`MAX_STORED_LINKS`].
    /// Incremental crawling stores them to follow a page's links again when
    /// it later answers 304 Not Modified ([`Self::extract_from_links`]
    /// re-applies the filters and the depth limit then). The links to follow
    /// are the same ones, uncapped, and empty past `max_depth`.
    pub fn extract_with_outlinks(
        &self,
        page: &RawPage,
        parent_depth: u32,
    ) -> (Vec<CrawlUrl>, Vec<String>) {
        let page_url = match Url::parse(&page.final_url) {
            Ok(url) => url,
            Err(_) => return (vec![], vec![]),
        };
        let page_domain = page_url.host_str().unwrap_or("").to_lowercase();
        let follow = parent_depth < self.config.max_depth;

        let document = Html::parse_document(&page.html);
        let join_base = document_base_url(&document, &page_url);
        let mut seen = HashSet::new();
        let mut outlinks = Vec::new();
        let mut urls = Vec::new();

        for element in document.select(&self.link_selector) {
            let Some(href) = element.value().attr("href") else {
                continue;
            };
            let Some((normalized, host)) = self.resolve(href, &join_base, &page_url, &page_domain)
            else {
                continue;
            };
            if !self.accept(&normalized, &host, &page_domain, &mut seen) {
                continue;
            }
            if outlinks.len() < MAX_STORED_LINKS {
                outlinks.push(normalized.clone());
            }
            if !follow {
                continue;
            }
            let anchor_text = element
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string();
            let mut crawl_url = self.crawl_url(normalized, &page.final_url, parent_depth);
            if !anchor_text.is_empty() {
                crawl_url.anchor_text = Some(anchor_text);
            }
            urls.push(crawl_url);
        }

        debug!(
            url = %page.final_url,
            extracted = urls.len(),
            outlinks = outlinks.len(),
            "Extracted URLs"
        );

        (urls, outlinks)
    }

    /// Extract URLs as strings only (for implementing Parser trait)
    pub fn extract_urls(&self, page: &RawPage) -> Vec<String> {
        let page_url = match Url::parse(&page.final_url) {
            Ok(url) => url,
            Err(_) => return vec![],
        };
        let page_domain = page_url.host_str().unwrap_or("").to_lowercase();
        let document = Html::parse_document(&page.html);
        let join_base = document_base_url(&document, &page_url);
        let mut seen = HashSet::new();
        let mut urls = Vec::new();

        for element in document.select(&self.link_selector) {
            if let Some(href) = element.value().attr("href") {
                if let Some((normalized, host)) =
                    self.resolve(href, &join_base, &page_url, &page_domain)
                {
                    if self.accept(&normalized, &host, &page_domain, &mut seen) {
                        urls.push(normalized);
                    }
                }
            }
        }

        urls
    }

    fn crawl_url(&self, url: String, parent_url: &str, parent_depth: u32) -> CrawlUrl {
        CrawlUrl {
            url,
            depth: parent_depth + 1,
            priority: 0,
            parent_url: Some(parent_url.to_string()),
            anchor_text: None,
            discovered_at: chrono::Utc::now(),
            retry_count: 0,
            requires_js: false,
            etag: None,
            last_modified: None,
            not_before_ms: None,
        }
    }

    /// Resolve `href` against `join_base` (the page URL, or its `<base
    /// href>`) and normalize it. Returns the normalized URL and its host.
    /// `None` for an empty, `javascript:`, `mailto:`, `tel:`, `data:` or
    /// fragment-only href, and for a non-http(s) target.
    fn resolve(
        &self,
        href: &str,
        join_base: &Url,
        page_url: &Url,
        page_domain: &str,
    ) -> Option<(String, String)> {
        let href = href.trim();
        if href.is_empty()
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
            || href.starts_with("data:")
            || href.starts_with('#')
        {
            return None;
        }

        let mut resolved = join_base.join(href).ok()?;

        // An `http://` link to the page's own host from an `https://` page
        // is the same page: keep the crawl on `https` rather than indexing
        // both schemes.
        if resolved.scheme() == "http"
            && page_url.scheme() == "https"
            && resolved
                .host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(page_domain))
        {
            let _ = resolved.set_scheme("https");
            if resolved.port() == Some(80) {
                let _ = resolved.set_port(None);
            }
        }

        let host = resolved.host_str()?.to_lowercase();
        let normalized = normalize_link_url(&resolved)?;
        Some((normalized, host))
    }

    /// Whether a resolved link is followed: not a non-page resource, not
    /// already seen on this page, on an allowed domain and matching the URL
    /// patterns. Records it in `seen` when accepted.
    fn accept(
        &self,
        normalized: &str,
        host: &str,
        page_domain: &str,
        seen: &mut HashSet<String>,
    ) -> bool {
        if is_non_page_url_for(normalized, self.follow_pdf, self.follow_documents) {
            return false;
        }
        if seen.contains(normalized) {
            return false;
        }
        if !self.is_allowed_domain(host, page_domain) {
            return false;
        }
        if let Some(ref patterns) = self.config.patterns {
            if !matches_patterns(normalized, patterns) {
                return false;
            }
        }
        seen.insert(normalized.to_string());
        true
    }

    /// Normalize a domain by stripping the `www.` prefix for comparison.
    fn normalize_domain(domain: &str) -> &str {
        domain
            .strip_prefix("www.")
            .or_else(|| domain.strip_prefix("WWW."))
            .unwrap_or(domain)
    }

    /// Whether `host` is in an explicit `allowed_domains` whitelist, with
    /// the same matching link extraction uses (case-insensitive, a leading
    /// `www.` ignored on both sides). An empty list allows every host.
    pub fn host_in_allowed_domains(host: &str, allowed_domains: &[String]) -> bool {
        if allowed_domains.is_empty() {
            return true;
        }
        let host = Self::normalize_domain(host);
        allowed_domains
            .iter()
            .any(|d| Self::normalize_domain(d).eq_ignore_ascii_case(host))
    }

    /// Check if a domain is allowed based on configuration
    fn is_allowed_domain(&self, url_domain: &str, base_domain: &str) -> bool {
        let url_norm = Self::normalize_domain(url_domain);

        // If explicit allowed_domains whitelist is set, use ONLY that (strict mode)
        if !self.config.allowed_domains.is_empty() {
            return Self::host_in_allowed_domains(url_domain, &self.config.allowed_domains);
        }

        // Fallback to automatic domain inference
        let base_norm = Self::normalize_domain(base_domain);
        if url_norm.eq_ignore_ascii_case(base_norm) {
            return true;
        }

        if self.config.follow_subdomains {
            // Check if url_domain is a subdomain of base_domain
            // e.g., blog.example.com is a subdomain of example.com
            let suffix = format!(".{}", base_norm);
            if url_norm.ends_with(&suffix) {
                return true;
            }
            // NOTE: We intentionally do NOT check if base_domain is a subdomain of url_domain
            // That was a bug that caused domain explosion (e.g., en.wikipedia.org allowing wikipedia.org
            // which then allows all other language subdomains)
        }

        self.config.follow_external
    }
}

/// Check if URL matches configured include/exclude glob patterns.
///
/// Exclude patterns win over include patterns. An empty include list allows
/// everything (subject to exclude). Shared by [`UrlExtractor::resolve_and_filter`]
/// and [`url_allowed`] so link extraction and sitemap discovery can't diverge.
fn matches_patterns(url: &str, patterns: &UrlPatterns) -> bool {
    matches_include_exclude(url, &patterns.include, &patterns.exclude)
}

/// Whether `url` is allowed by `patterns` — the same domain whitelist and
/// include/exclude glob matching link extraction applies
/// ([`UrlExtractor::resolve_and_filter`]). Used by sitemap discovery so a
/// sitemap URL and a discovered link are judged identically: an
/// auto-generated `https://host/path/*` include pattern that would accept a
/// link also accepts the equivalent sitemap entry.
///
/// An empty `allowed_domains` allows every host (domain filtering is opt-in).
pub fn url_allowed(patterns: &UrlPatterns, url: &str) -> bool {
    if !patterns.allowed_domains.is_empty() {
        let host_allowed = Url::parse(url).ok().and_then(|u| {
            u.host_str()
                .map(|h| UrlExtractor::host_in_allowed_domains(h, &patterns.allowed_domains))
        });
        if host_allowed != Some(true) {
            return false;
        }
    }

    matches_patterns(url, patterns)
}

/// [`url_allowed`], with `http`/`https` and a leading `www.` treated as the
/// same site: `url` is allowed when one of its spellings
/// ([`scrapix_core::url_normalize::scheme_and_www_variants`]) matches an
/// include pattern and none matches an exclude pattern. For judging where a
/// redirect landed: `https://example.com/docs/*` accepts a redirect to
/// `https://www.example.com/docs/a`, as it did before redirects were checked.
pub fn url_allowed_any_scheme_or_www(patterns: &UrlPatterns, url: &str) -> bool {
    if !patterns.allowed_domains.is_empty() {
        let host_allowed = Url::parse(url).ok().and_then(|u| {
            u.host_str()
                .map(|h| UrlExtractor::host_in_allowed_domains(h, &patterns.allowed_domains))
        });
        if host_allowed != Some(true) {
            return false;
        }
    }
    let variants = scrapix_core::url_normalize::scheme_and_www_variants(url);
    let any = |globs: &[String]| {
        variants.iter().any(|v| {
            globs
                .iter()
                .any(|p| scrapix_core::url_glob::matches_glob(v, p))
        })
    };
    if any(&patterns.exclude) {
        return false;
    }
    patterns.include.is_empty() || any(&patterns.include)
}

/// Check if a URL points to a non-page resource (image, PDF, CSS, JS, font, etc.)
///
/// Returns `true` if the URL's path ends with a known non-page file extension.
/// This is used to filter out URLs that won't yield useful crawlable content.
///
/// Preserves pre-PDF behavior: `.pdf` links are always filtered out. Callers
/// that support opt-in PDF scraping should use [`is_non_page_url_with_pdf`]
/// and pass `pdf_enabled = true`.
pub fn is_non_page_url(url: &str) -> bool {
    is_non_page_url_with_pdf(url, false)
}

/// PDF-aware variant of [`is_non_page_url`].
///
/// When `pdf_enabled` is `true`, `.pdf` URLs are *not* filtered out — the
/// crawler will fetch them and the content worker will parse them. All other
/// non-page extensions are still rejected.
pub fn is_non_page_url_with_pdf(url: &str, pdf_enabled: bool) -> bool {
    is_non_page_url_for(url, pdf_enabled, false)
}

/// Document-aware variant of [`is_non_page_url`]: `.pdf` URLs pass when
/// `pdf_enabled`, office-document URLs (`.doc(x)`, `.xls(x)`, `.ppt(x)`,
/// OpenDocument, ...) pass when `documents_enabled`. Every other non-page
/// extension is still rejected. The extension only decides which links
/// are *worth fetching*; the document format itself is decided from the
/// response's `Content-Type` and bytes.
pub fn is_non_page_url_for(url: &str, pdf_enabled: bool, documents_enabled: bool) -> bool {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    if let Some(dot_pos) = path.rfind('.') {
        let ext = &path[dot_pos..].to_ascii_lowercase();
        if pdf_enabled && ext == ".pdf" {
            return false;
        }
        if documents_enabled
            && matches!(
                ext.as_str(),
                ".doc"
                    | ".docx"
                    | ".docm"
                    | ".xls"
                    | ".xlsx"
                    | ".xlsm"
                    | ".xlsb"
                    | ".ppt"
                    | ".pptx"
                    | ".pptm"
                    | ".pps"
                    | ".ppsx"
                    | ".odt"
                    | ".ods"
                    | ".odp"
            )
        {
            return false;
        }
        matches!(
            ext.as_str(),
            // Images
            ".png" | ".jpg" | ".jpeg" | ".gif" | ".svg" | ".webp" | ".ico" | ".bmp" | ".tiff"
            // Documents
            | ".pdf" | ".doc" | ".docx" | ".xls" | ".xlsx" | ".ppt" | ".pptx" | ".odt" | ".ods"
            // Media
            | ".mp4" | ".mp3" | ".avi" | ".mov" | ".wmv" | ".flv" | ".webm" | ".mkv" | ".wav" | ".ogg"
            // Archives
            | ".zip" | ".tar" | ".gz" | ".rar" | ".7z" | ".bz2" | ".xz"
            // Code/Assets
            | ".css" | ".js" | ".mjs" | ".map"
            // Fonts
            | ".woff" | ".woff2" | ".ttf" | ".eot" | ".otf"
            // Data
            | ".xml" | ".rss" | ".atom" | ".json" | ".jsonld"
        )
    } else {
        false
    }
}

/// Builder for UrlExtractor
pub struct UrlExtractorBuilder {
    config: ExtractorConfig,
}

impl UrlExtractorBuilder {
    pub fn new() -> Self {
        Self {
            config: ExtractorConfig::default(),
        }
    }

    pub fn patterns(mut self, patterns: UrlPatterns) -> Self {
        self.config.patterns = Some(patterns);
        self
    }

    pub fn max_depth(mut self, depth: u32) -> Self {
        self.config.max_depth = depth;
        self
    }

    pub fn follow_external(mut self, follow: bool) -> Self {
        self.config.follow_external = follow;
        self
    }

    pub fn follow_subdomains(mut self, follow: bool) -> Self {
        self.config.follow_subdomains = follow;
        self
    }

    pub fn allowed_domains(mut self, domains: Vec<String>) -> Self {
        self.config.allowed_domains = domains;
        self
    }

    pub fn build(self) -> UrlExtractor {
        UrlExtractor::new(self.config)
    }
}

impl Default for UrlExtractorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_page(url: &str, html: &str) -> RawPage {
        RawPage {
            url: url.to_string(),
            final_url: url.to_string(),
            status: 200,
            headers: Default::default(),
            html: html.to_string(),
            content_type: Some("text/html".to_string()),
            js_rendered: false,
            fetched_at: chrono::Utc::now(),
            fetch_duration_ms: 100,
        }
    }

    #[test]
    fn test_extract_absolute_urls() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://example.com",
            "<html><body><a href=\"https://example.com/page1\">Page 1</a><a href=\"https://example.com/page2\">Page 2</a></body></html>",
        );

        let urls = extractor.extract(&page, 0);
        assert_eq!(urls.len(), 2);
        assert!(urls.iter().any(|u| u.url == "https://example.com/page1"));
        assert!(urls.iter().any(|u| u.url == "https://example.com/page2"));
    }

    #[test]
    fn test_extract_relative_urls() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://example.com/section/",
            "<html><body><a href=\"/page1\">Page 1</a><a href=\"page2\">Page 2</a><a href=\"../other\">Other</a></body></html>",
        );

        let urls = extractor.extract_urls(&page);
        assert!(urls.contains(&"https://example.com/page1".to_string()));
        assert!(urls.contains(&"https://example.com/section/page2".to_string()));
        assert!(urls.contains(&"https://example.com/other".to_string()));
    }

    #[test]
    fn test_skip_javascript_urls() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://example.com",
            "<html><body><a href=\"javascript:void(0)\">Click</a><a href=\"mailto:test@example.com\">Email</a><a href=\"#section\">Anchor</a><a href=\"https://example.com/valid\">Valid</a></body></html>",
        );

        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "https://example.com/valid");
    }

    #[test]
    fn test_external_links_blocked_by_default() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://example.com",
            "<html><body><a href=\"https://example.com/internal\">Internal</a><a href=\"https://other.com/external\">External</a></body></html>",
        );

        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("example.com"));
    }

    #[test]
    fn test_subdomain_following() {
        let extractor = UrlExtractor::new(ExtractorConfig {
            follow_subdomains: true,
            ..Default::default()
        });
        let page = make_page(
            "https://example.com",
            "<html><body><a href=\"https://blog.example.com/post\">Blog</a><a href=\"https://other.com/page\">Other</a></body></html>",
        );

        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("blog.example.com"));
    }

    #[test]
    fn test_allowed_domains_whitelist() {
        // When allowed_domains is set, only those exact domains are allowed
        let extractor = UrlExtractor::new(ExtractorConfig {
            allowed_domains: vec!["en.wikipedia.org".to_string()],
            ..Default::default()
        });
        let page = make_page(
            "https://en.wikipedia.org/wiki/Main",
            "<html><body>\
                <a href=\"https://en.wikipedia.org/wiki/Article\">Same domain</a>\
                <a href=\"https://fr.wikipedia.org/wiki/Article\">French wiki</a>\
                <a href=\"https://wikipedia.org/\">Parent domain</a>\
                <a href=\"https://wikidata.org/\">External</a>\
            </body></html>",
        );

        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("en.wikipedia.org"));
    }

    #[test]
    fn test_no_parent_domain_escape() {
        // Verify that parent domains don't escape the filter
        // (the old bug where en.wikipedia.org would allow wikipedia.org)
        let extractor = UrlExtractor::new(ExtractorConfig {
            follow_subdomains: true,
            follow_external: false,
            ..Default::default()
        });
        let page = make_page(
            "https://en.wikipedia.org/wiki/Main",
            "<html><body>\
                <a href=\"https://en.wikipedia.org/wiki/Article\">Same</a>\
                <a href=\"https://wikipedia.org/\">Parent</a>\
            </body></html>",
        );

        let urls = extractor.extract_urls(&page);
        // Only same domain should be allowed, NOT the parent
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("en.wikipedia.org"));
        assert!(!urls.iter().any(|u| u == "https://wikipedia.org"));
    }

    #[test]
    fn test_www_normalization_in_allowed_domains() {
        // When allowed_domains contains "meilisearch.com", links to
        // "www.meilisearch.com" should also be allowed (and vice versa).
        let extractor = UrlExtractor::new(ExtractorConfig {
            allowed_domains: vec!["meilisearch.com".to_string()],
            ..Default::default()
        });
        let page = make_page(
            "https://meilisearch.com",
            "<html><body>\
                <a href=\"https://www.meilisearch.com/docs\">Docs</a>\
                <a href=\"https://meilisearch.com/blog\">Blog</a>\
                <a href=\"https://other.com/\">Other</a>\
            </body></html>",
        );

        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 2);
        assert!(urls.iter().any(|u| u.contains("meilisearch.com/docs")));
        assert!(urls.iter().any(|u| u.contains("meilisearch.com/blog")));
    }

    #[test]
    fn test_www_normalization_base_domain() {
        // When crawling from "meilisearch.com" (no allowed_domains set),
        // links to "www.meilisearch.com" should be followed.
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://meilisearch.com",
            "<html><body>\
                <a href=\"https://www.meilisearch.com/docs\">Docs</a>\
                <a href=\"https://other.com/\">Other</a>\
            </body></html>",
        );

        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("meilisearch.com/docs"));
    }

    #[test]
    fn test_glob_patterns() {
        assert!(matches_glob(
            "https://example.com/docs/page",
            "https://example.com/docs/**"
        ));
        assert!(matches_glob(
            "https://example.com/docs/deep/page",
            "https://example.com/docs/**"
        ));
        assert!(!matches_glob(
            "https://example.com/blog/page",
            "https://example.com/docs/**"
        ));
    }

    #[test]
    fn same_host_http_links_on_an_https_page_stay_https() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://quotes.toscrape.com/",
            r#"<a href="http://quotes.toscrape.com/login">a</a>
               <a href="http://quotes.toscrape.com:80/tag/love/">b</a>
               <a href="/login">dup</a>
               <a href="http://other.test/x">ext</a>"#,
        );
        let urls = extractor.extract_urls(&page);
        assert_eq!(
            urls,
            vec![
                "https://quotes.toscrape.com/login".to_string(),
                "https://quotes.toscrape.com/tag/love".to_string(),
            ]
        );

        // An http page keeps its links as they are.
        let page = make_page(
            "http://a.test/",
            r#"<a href="/x">x</a><a href="https://a.test/y">y</a>"#,
        );
        assert_eq!(
            extractor.extract_urls(&page),
            vec![
                "http://a.test/x".to_string(),
                "https://a.test/y".to_string()
            ]
        );
    }

    #[test]
    fn test_trailing_slash_http_scheme() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "http://example.com",
            r#"<html><body><a href="http://example.com/path/">Link</a></body></html>"#,
        );
        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "http://example.com/path");
    }

    #[test]
    fn test_trailing_slash_https_scheme() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://example.com",
            r#"<html><body><a href="https://example.com/path/">Link</a></body></html>"#,
        );
        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "https://example.com/path");
    }

    #[test]
    fn test_trailing_slash_root_preserved() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://example.com",
            r#"<html><body><a href="https://example.com/">Link</a></body></html>"#,
        );
        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "https://example.com/");
    }

    #[test]
    fn test_trailing_slash_deep_path() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://example.com",
            r#"<html><body><a href="https://example.com/a/b/c/">Link</a></body></html>"#,
        );
        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "https://example.com/a/b/c");
    }

    #[test]
    fn test_glob_multiple_double_star() {
        // Pattern with multiple ** segments currently only handles 2-part split
        assert!(matches_glob(
            "https://example.com/docs/v2/api",
            "https://example.com/docs/**/api"
        ));
    }

    #[test]
    fn test_glob_single_star_no_slash_crossing() {
        // Single * should NOT cross directory boundaries
        assert!(!matches_glob(
            "https://example.com/docs/a/b/page",
            "https://example.com/docs/*/page"
        ));
    }

    #[test]
    fn test_glob_exact_match() {
        assert!(matches_glob(
            "https://example.com/page",
            "https://example.com/page"
        ));
        assert!(!matches_glob(
            "https://example.com/other",
            "https://example.com/page"
        ));
    }

    #[test]
    fn test_is_non_page_url() {
        // Images
        assert!(is_non_page_url("https://example.com/image.png"));
        assert!(is_non_page_url("https://example.com/photo.jpg"));
        assert!(is_non_page_url("https://example.com/icon.svg"));

        // Documents
        assert!(is_non_page_url("https://example.com/report.pdf"));
        assert!(is_non_page_url("https://example.com/doc.xlsx"));

        // Media
        assert!(is_non_page_url("https://example.com/video.mp4"));

        // Assets
        assert!(is_non_page_url("https://example.com/style.css"));
        assert!(is_non_page_url("https://example.com/app.js"));
        assert!(is_non_page_url("https://example.com/font.woff2"));

        // Data
        assert!(is_non_page_url("https://example.com/feed.xml"));
        assert!(is_non_page_url("https://example.com/data.json"));

        // Archives
        assert!(is_non_page_url("https://example.com/archive.zip"));

        // Normal pages should NOT be filtered
        assert!(!is_non_page_url("https://example.com/page"));
        assert!(!is_non_page_url("https://example.com/about.html"));
        assert!(!is_non_page_url("https://example.com/docs/intro"));
        assert!(!is_non_page_url("https://example.com/"));

        // Query params shouldn't affect the check
        assert!(is_non_page_url("https://example.com/image.png?w=100"));
        assert!(!is_non_page_url("https://example.com/page?format=json"));

        // Case insensitive
        assert!(is_non_page_url("https://example.com/IMAGE.PNG"));
        assert!(is_non_page_url("https://example.com/style.CSS"));
    }

    #[test]
    fn test_is_non_page_url_for_documents() {
        assert!(is_non_page_url_for(
            "https://example.com/r.docx",
            false,
            false
        ));
        assert!(!is_non_page_url_for(
            "https://example.com/r.docx",
            false,
            true
        ));
        assert!(!is_non_page_url_for(
            "https://example.com/r.XLSX?v=1",
            false,
            true
        ));
        assert!(is_non_page_url_for(
            "https://example.com/r.pdf",
            false,
            true
        ));
        assert!(is_non_page_url_for("https://example.com/a.png", true, true));
    }

    #[test]
    fn test_document_links_follow_the_opt_ins() {
        let page = RawPage {
            url: "https://example.com/".into(),
            final_url: "https://example.com/".into(),
            status: 200,
            headers: Default::default(),
            html: r#"<a href="/a.pdf">a</a><a href="/b.docx">b</a><a href="/c">c</a>"#.into(),
            content_type: Some("text/html".into()),
            js_rendered: false,
            fetched_at: chrono::Utc::now(),
            fetch_duration_ms: 0,
        };
        let urls = |e: UrlExtractor| -> Vec<String> {
            e.extract(&page, 0).into_iter().map(|u| u.url).collect()
        };
        assert_eq!(
            urls(UrlExtractor::with_defaults()),
            vec!["https://example.com/c"]
        );
        assert_eq!(
            urls(UrlExtractor::with_defaults().follow_document_links(true, true)).len(),
            3
        );
        let from_pdf = UrlExtractor::with_defaults().extract_from_links(
            "https://example.com/doc.pdf",
            vec![
                "https://example.com/x".to_string(),
                "https://evil.test/y".to_string(),
            ],
            0,
        );
        assert_eq!(from_pdf.len(), 1, "domain rules apply to PDF links");
    }

    #[test]
    fn test_is_non_page_url_with_pdf_flag() {
        // When PDF is disabled (default), `.pdf` still filtered out.
        assert!(is_non_page_url_with_pdf(
            "https://example.com/report.pdf",
            false
        ));
        // When PDF is enabled, `.pdf` URLs are *not* filtered.
        assert!(!is_non_page_url_with_pdf(
            "https://example.com/report.pdf",
            true
        ));
        // But other non-page extensions are still filtered regardless.
        assert!(is_non_page_url_with_pdf(
            "https://example.com/doc.docx",
            true
        ));
        assert!(is_non_page_url_with_pdf(
            "https://example.com/style.css",
            true
        ));
        // Case-insensitive: `.PDF` is also passed through.
        assert!(!is_non_page_url_with_pdf(
            "https://example.com/Report.PDF",
            true
        ));
        // Query params don't affect the check.
        assert!(!is_non_page_url_with_pdf(
            "https://example.com/doc.pdf?v=2",
            true
        ));
        // Normal pages are unaffected by the flag.
        assert!(!is_non_page_url_with_pdf("https://example.com/page", true));
    }

    #[test]
    fn test_extractor_filters_non_page_urls() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://example.com",
            r#"<html><body>
                <a href="https://example.com/page1">Page</a>
                <a href="https://example.com/image.png">Image</a>
                <a href="https://example.com/style.css">CSS</a>
                <a href="https://example.com/app.js">JS</a>
                <a href="https://example.com/doc.pdf">PDF</a>
                <a href="https://example.com/page2">Page 2</a>
            </body></html>"#,
        );

        let urls = extractor.extract_urls(&page);
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"https://example.com/page1".to_string()));
        assert!(urls.contains(&"https://example.com/page2".to_string()));
    }

    #[test]
    fn url_allowed_matches_auto_generated_path_prefix() {
        let p = UrlPatterns {
            include: vec!["https://docs.a.test/guide/*".into()],
            ..Default::default()
        };
        assert!(url_allowed(&p, "https://docs.a.test/guide/intro"));
        assert!(!url_allowed(&p, "https://docs.a.test/blog/post"));
    }

    #[test]
    fn url_allowed_respects_allowed_domains_with_www_normalization() {
        let p = UrlPatterns {
            allowed_domains: vec!["a.test".into()],
            ..Default::default()
        };
        assert!(url_allowed(&p, "https://www.a.test/x"));
        assert!(!url_allowed(&p, "https://b.test/x"));
    }

    #[test]
    fn base_href_is_honored_for_relative_links() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://a.test/app/deep/page",
            r#"<html><head><base href="/app/"></head><body>
               <a href="settings">s</a><a href="/root">r</a>
               <a href="https://a.test/abs">a</a></body></html>"#,
        );
        let urls = extractor.extract_urls(&page);
        assert_eq!(
            urls,
            vec![
                "https://a.test/app/settings",
                "https://a.test/root",
                "https://a.test/abs"
            ]
        );
        // A non-http base is ignored.
        let page = make_page(
            "https://a.test/x/y",
            r#"<base href="javascript:void(0)"><a href="z">z</a>"#,
        );
        assert_eq!(extractor.extract_urls(&page), vec!["https://a.test/x/z"]);
    }

    #[test]
    fn query_strings_are_removed_from_links() {
        let extractor = UrlExtractor::with_defaults();
        let page = make_page(
            "https://a.test/list",
            r#"<a href="?page=2">2</a><a href="/w/index.php?action=edit">e</a>
               <a href="/post/?replytocom=1#c">p</a>"#,
        );
        assert_eq!(
            extractor.extract_urls(&page),
            vec![
                "https://a.test/list",
                "https://a.test/w/index.php",
                "https://a.test/post"
            ]
        );
    }

    #[test]
    fn stored_links_are_filtered_capped_and_kept_past_max_depth() {
        let extractor = UrlExtractorBuilder::new()
            .patterns(UrlPatterns {
                include: vec!["https://a.test/docs/*".into()],
                ..Default::default()
            })
            .max_depth(1)
            .build();
        let page = make_page(
            "https://a.test/docs",
            r#"<a href="/docs/a/">a</a><a href="/blog">b</a>
               <a href="https://other.test/x">o</a><a href="/docs/a">dup</a>
               <a href="/docs/logo.png">img</a>"#,
        );
        let (links, stored) = extractor.extract_with_outlinks(&page, 0);
        let links: Vec<_> = links.into_iter().map(|u| u.url).collect();
        assert_eq!(links, vec!["https://a.test/docs/a"]);
        // Only in-scope links are stored.
        assert_eq!(stored, vec!["https://a.test/docs/a"]);
        let replayed: Vec<_> = extractor
            .extract_from_links("https://a.test/docs", stored, 0)
            .into_iter()
            .map(|u| u.url)
            .collect();
        assert_eq!(replayed, links);

        // At max_depth nothing is followed, but the links are still stored
        // (a later crawl may go deeper).
        let (links, stored) = extractor.extract_with_outlinks(&page, 1);
        assert!(links.is_empty());
        assert_eq!(stored, vec!["https://a.test/docs/a"]);

        // At most MAX_STORED_LINKS are stored.
        let html: String = (0..MAX_STORED_LINKS + 50)
            .map(|i| format!(r#"<a href="/docs/p{i}">p</a>"#))
            .collect();
        let (links, stored) =
            extractor.extract_with_outlinks(&make_page("https://a.test/docs", &html), 0);
        assert_eq!(stored.len(), MAX_STORED_LINKS);
        assert_eq!(links.len(), MAX_STORED_LINKS + 50);
    }

    #[test]
    fn redirect_targets_match_patterns_across_scheme_and_www() {
        let p = UrlPatterns {
            include: vec!["https://example.com/docs/*".into()],
            exclude: vec!["https://example.com/docs/private/*".into()],
            ..Default::default()
        };
        assert!(url_allowed_any_scheme_or_www(
            &p,
            "https://www.example.com/docs/a"
        ));
        assert!(url_allowed_any_scheme_or_www(
            &p,
            "http://example.com/docs/a"
        ));
        assert!(!url_allowed_any_scheme_or_www(
            &p,
            "https://www.example.com/blog/a"
        ));
        assert!(!url_allowed_any_scheme_or_www(
            &p,
            "https://www.example.com/docs/private/x"
        ));
        let d = UrlPatterns {
            allowed_domains: vec!["example.com".into()],
            ..Default::default()
        };
        assert!(url_allowed_any_scheme_or_www(
            &d,
            "https://www.example.com/x"
        ));
        assert!(!url_allowed_any_scheme_or_www(&d, "https://other.test/x"));
    }
}
