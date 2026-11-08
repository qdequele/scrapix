//! # Scrapix Crawler
//!
//! HTTP and browser-based web crawler.
//!
//! ## Features
//!
//! - High-performance HTTP fetching with reqwest
//! - Connection pooling
//! - Proxy rotation
//! - DNS caching
//! - Robots.txt compliance
//! - URL extraction with pattern filtering
//!
//! ## Browser Rendering (Optional)
//!
//! Enable browser rendering with feature flags:
//!
//! - `browser-cdp`: Chrome/Chromium via CDP (chromiumoxide)
//! - `browser-lightpanda`: Lightpanda over CDP, for fast plain crawl renders
//! - `browser-webdriver`: WebDriver protocol (fantoccini)
//! - `browser-full`: All browser options
//!
//! ### Rendering Strategy
//!
//! 1. **HTTP First** (default): Fast reqwest + scraper for 80-90% of pages
//! 2. **Lightpanda** (`browser-lightpanda`): plain crawl renders, Chrome fallback
//! 3. **Chrome/Chromium**: Fallback for complex JS sites
//! 4. **Browserbase** (optional): Hosted infrastructure for scale
//!
//! ## Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use scrapix_crawler::{
//!     fetcher::{HttpFetcher, HttpFetcherBuilder},
//!     robots::RobotsCache,
//!     extractor::UrlExtractor,
//! };
//! use scrapix_core::CrawlUrl;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Create robots.txt cache
//!     let robots_cache = Arc::new(RobotsCache::with_defaults()?);
//!
//!     // Create HTTP fetcher
//!     let fetcher = HttpFetcherBuilder::new()
//!         .user_agent("MyBot/1.0")
//!         .build(robots_cache)?;
//!
//!     // Fetch a page
//!     let url = CrawlUrl::seed("https://example.com");
//!     let page = fetcher.fetch(&url).await?;
//!
//!     // Extract links
//!     let extractor = UrlExtractor::with_defaults();
//!     let links = extractor.extract(&page, 0);
//!
//!     println!("Found {} links", links.len());
//!     Ok(())
//! }
//! ```

pub mod dns;
pub mod extractor;
pub mod fetcher;
pub mod proxy;
pub mod robots;
pub(crate) mod safe_client;
pub(crate) mod safe_dns;
pub mod sitemap;

#[cfg(feature = "browser-cdp")]
pub mod cdp_actions;
#[cfg(feature = "browser-cdp")]
pub(crate) mod cdp_stealth;
#[cfg(feature = "browser-cdp")]
pub mod renderer_cdp;

#[cfg(feature = "browser-lightpanda")]
pub mod renderer_lightpanda;

#[cfg(feature = "browser-webdriver")]
pub mod renderer_webdriver;

// Re-exports for convenience
pub use dns::{CachingDnsResolver, DnsCacheStats, DnsConfig};
pub use extractor::{
    document_base_url, is_non_page_url, is_non_page_url_for, is_non_page_url_with_pdf, url_allowed,
    url_allowed_any_scheme_or_www, ExtractorConfig, UrlExtractor, UrlExtractorBuilder,
    MAX_STORED_LINKS,
};
pub use fetcher::{
    is_unsupported_content_type, parse_retry_after, ConditionalRequestHeaders, FetchOptions,
    FetchResult, FetcherConfig, HttpFetcher, HttpFetcherBuilder, RetryConfig, HEAD_ONLY_MAX_BYTES,
    UNSUPPORTED_CONTENT_TYPE,
};
pub use proxy::{ProxyConfig, ProxyPool, RotationStrategy};
pub use robots::{
    PersistentRobotsCache, PersistentRobotsEntry, RobotsCache, RobotsCacheStats, RobotsConfig,
    RobotsPersistence, RocksDbOps, RocksRobotsPersistence,
};
pub use safe_client::safe_client_builder;
pub use safe_dns::{is_link_local_or_metadata, is_public_ip, validate_proxy_url};
pub use sitemap::{
    ChangeFrequency, SitemapConfig, SitemapContent, SitemapEntry, SitemapParser, SitemapUrl,
};

// CDP renderer re-exports
#[cfg(feature = "browser-cdp")]
pub use cdp_actions::{ACTIONS_BUDGET, ACTION_TIMEOUT};
#[cfg(feature = "browser-cdp")]
pub use renderer_cdp::{
    CdpConfig, CdpError, CdpRenderer, CdpRendererBuilder, PageOptions, RenderResult,
    ScreenshotOptions, WaitUntil, BROWSER_PROXY_UNSUPPORTED, MAX_SCREENSHOT_HEIGHT,
};

// WebDriver renderer re-exports
#[cfg(feature = "browser-webdriver")]
pub use renderer_webdriver::{
    BrowserType, PageLoadStrategy, WebDriverConfig, WebDriverCookie, WebDriverError,
    WebDriverRenderResult, WebDriverRenderer, WebDriverRendererBuilder,
};

// Lightpanda renderer re-exports
#[cfg(feature = "browser-lightpanda")]
pub use renderer_lightpanda::{
    should_retry_in_chrome, visible_text_chars, LightpandaConfig, LightpandaRender,
    LightpandaRenderer, PageStatus, DEFAULT_MIN_TEXT_CHARS,
};
