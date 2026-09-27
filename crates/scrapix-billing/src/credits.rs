//! Credit calculation for all Scrapix operations.
//!
//! Pure functions with no side effects — safe to call from anywhere.

use scrapix_core::{CrawlerType, FeaturesConfig};

/// Compute credits for a /scrape request.
///
/// - `feature_format_count`: number of feature formats requested
///   (Markdown, Links, Metadata, Screenshot, Schema, Blocks).
///   Base formats (Html, RawHtml, Content) are free and should NOT be counted.
/// - AI summary: +5
/// - AI extraction: +5
/// - Minimum: 1 credit (even if no features)
pub fn scrape_credits(
    feature_format_count: i64,
    has_ai_summary: bool,
    has_ai_extraction: bool,
) -> i64 {
    let ai_cost = if has_ai_summary { 5 } else { 0 } + if has_ai_extraction { 5 } else { 0 };

    // At least 1 credit per scrape
    (feature_format_count + ai_cost).max(1)
}

/// Non-AI per-page feature surcharge: +1 per enabled feature (metadata,
/// markdown, block_split, schema, custom_selectors). Shared by
/// `crawl_credits_per_page` and `crawl_credits` so the two stay consistent.
fn non_ai_feature_credits(features: &FeaturesConfig) -> i64 {
    let mut feature_count: i64 = 0;
    if features.metadata.as_ref().is_some_and(|f| f.enabled) {
        feature_count += 1;
    }
    if features.markdown.as_ref().is_some_and(|f| f.enabled) {
        feature_count += 1;
    }
    if features.block_split.as_ref().is_some_and(|f| f.enabled) {
        feature_count += 1;
    }
    if features.schema.as_ref().is_some_and(|s| s.enabled) {
        feature_count += 1;
    }
    if features
        .custom_selectors
        .as_ref()
        .is_some_and(|s| s.enabled)
    {
        feature_count += 1;
    }
    feature_count
}

/// AI per-page surcharge: +5 for AI extraction, +5 for AI summary — applied
/// only per page that was actually AI-enriched, not per page of the job
/// (see `crawl_credits`).
fn ai_surcharge_per_page(features: &FeaturesConfig) -> i64 {
    let mut surcharge: i64 = 0;
    if features.ai_extraction.as_ref().is_some_and(|a| a.enabled) {
        surcharge += 5;
    }
    if features.ai_summary.as_ref().is_some_and(|f| f.enabled) {
        surcharge += 5;
    }
    surcharge
}

/// Compute per-page credits for a /crawl job, assuming every page is of the
/// job's declared `crawler_type` and, if AI features are enabled, every page
/// got AI-enriched. Used for `/scrape` and pre-flight cost estimates, where
/// there is no per-page delivery breakdown yet to bill against.
///
/// - HTTP mode: 1 base per page
/// - Browser (JS) mode: 2 base per page
/// - +1 per enabled feature (metadata, markdown, block_split, schema, custom_selectors)
/// - +5 for AI extraction
/// - +5 for AI summary
pub fn crawl_credits_per_page(crawler_type: &CrawlerType, features: &FeaturesConfig) -> i64 {
    let base = match crawler_type {
        CrawlerType::Http => 1,
        CrawlerType::Browser => 2,
    };
    base + non_ai_feature_credits(features) + ai_surcharge_per_page(features)
}

/// Compute total credits for a completed/terminal `/crawl` job from what was
/// actually delivered (D4/R4): the browser surcharge is charged only for
/// `pages_browser` pages (not the whole job just because `crawler_type:
/// browser` was requested), and the AI surcharge only for `pages_ai` pages
/// (not every page just because an AI feature was enabled on the job).
///
/// `pages_http` and `pages_browser` are mutually exclusive counts of
/// successfully crawled pages; `pages_ai` counts (a subset of
/// `pages_http + pages_browser`) pages that were actually AI-enriched.
///
/// When every delivered page is of one kind, this reproduces
/// `crawl_credits_per_page` exactly: `crawl_credits(n, 0, 0, f) ==
/// crawl_credits_per_page(&CrawlerType::Http, f) * n` for any `f` with no AI
/// features enabled (an AI feature charges only pages that were actually
/// enriched, so the two functions intentionally diverge once AI is enabled
/// but `pages_ai < n` — that divergence is the fix this function makes).
pub fn crawl_credits(
    pages_http: u64,
    pages_browser: u64,
    pages_ai: u64,
    features: &FeaturesConfig,
) -> i64 {
    let feature_credits = non_ai_feature_credits(features);
    let http_credits = pages_http as i64 * (1 + feature_credits);
    let browser_credits = pages_browser as i64 * (2 + feature_credits);
    let ai_credits = pages_ai as i64 * ai_surcharge_per_page(features);
    http_credits + browser_credits + ai_credits
}

/// Map credits: flat 2 per call.
pub const MAP_CREDITS: i64 = 2;

/// Search credits: flat 2 per call.
pub const SEARCH_CREDITS: i64 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scrape_credits_minimum_one() {
        assert_eq!(scrape_credits(0, false, false), 1);
    }

    #[test]
    fn test_scrape_credits_base_formats_free() {
        // Base formats aren't counted, so feature_format_count = 0
        assert_eq!(scrape_credits(0, false, false), 1);
    }

    #[test]
    fn test_scrape_credits_feature_formats() {
        assert_eq!(scrape_credits(1, false, false), 1);
        assert_eq!(scrape_credits(3, false, false), 3);
        assert_eq!(scrape_credits(6, false, false), 6);
    }

    #[test]
    fn test_scrape_credits_ai_summary() {
        assert_eq!(scrape_credits(0, true, false), 5);
    }

    #[test]
    fn test_scrape_credits_ai_extraction() {
        assert_eq!(scrape_credits(0, false, true), 5);
    }

    #[test]
    fn test_scrape_credits_ai_both() {
        assert_eq!(scrape_credits(0, true, true), 10);
    }

    #[test]
    fn test_scrape_credits_combined() {
        // 2 feature formats + AI summary + AI extraction = 2 + 5 + 5 = 12
        assert_eq!(scrape_credits(2, true, true), 12);
    }

    #[test]
    fn test_crawl_credits_http_no_features() {
        let features = FeaturesConfig::default();
        assert_eq!(crawl_credits_per_page(&CrawlerType::Http, &features), 1);
    }

    #[test]
    fn test_crawl_credits_browser_base() {
        let features = FeaturesConfig::default();
        assert_eq!(crawl_credits_per_page(&CrawlerType::Browser, &features), 2);
    }

    #[test]
    fn test_crawl_credits_with_features() {
        let features = FeaturesConfig::from_cli_args(true, true, true, true, false, false, None);
        // 1 base + 4 features = 5
        assert_eq!(crawl_credits_per_page(&CrawlerType::Http, &features), 5);
    }

    #[test]
    fn test_crawl_credits_with_ai() {
        let features = FeaturesConfig::from_cli_args(
            false,
            false,
            false,
            false,
            true,
            true,
            Some("extract product info".to_string()),
        );
        // 1 base + 5 + 5 = 11
        assert_eq!(crawl_credits_per_page(&CrawlerType::Http, &features), 11);
    }

    #[test]
    fn test_crawl_credits_browser_all_features() {
        let features = FeaturesConfig::from_cli_args(
            true,
            true,
            true,
            true,
            true,
            true,
            Some("extract".to_string()),
        );
        // 2 base + 4 features + 5 ai_extraction + 5 ai_summary = 16
        assert_eq!(crawl_credits_per_page(&CrawlerType::Browser, &features), 16);
    }

    #[test]
    fn test_map_credits_constant() {
        assert_eq!(MAP_CREDITS, 2);
    }

    // ========================================================================
    // Task 16 (D4/R4): billing matches what was delivered
    // ========================================================================

    #[test]
    fn browser_and_ai_only_charged_for_delivered_pages() {
        let f = FeaturesConfig::default();
        let http_only = crawl_credits(10, 0, 0, &f);
        let with_browser = crawl_credits(8, 2, 0, &f);
        assert!(with_browser > http_only);
        assert_eq!(
            crawl_credits(10, 0, 0, &f),
            crawl_credits_per_page(&CrawlerType::Http, &f) * 10
        );
    }

    #[test]
    fn crawl_credits_reproduces_browser_per_page_rate_when_uniform() {
        let f = FeaturesConfig::default();
        assert_eq!(
            crawl_credits(0, 10, 0, &f),
            crawl_credits_per_page(&CrawlerType::Browser, &f) * 10
        );
    }

    #[test]
    fn crawl_credits_reproduces_per_page_rate_with_non_ai_features() {
        let f = FeaturesConfig::from_cli_args(true, true, true, true, false, false, None);
        assert_eq!(
            crawl_credits(10, 0, 0, &f),
            crawl_credits_per_page(&CrawlerType::Http, &f) * 10
        );
        assert_eq!(
            crawl_credits(0, 10, 0, &f),
            crawl_credits_per_page(&CrawlerType::Browser, &f) * 10
        );
    }

    /// AI surcharge test (Task 16 brief): 10 pages with only 3 AI-enriched
    /// pays the surcharge on exactly those 3 pages, not all 10.
    #[test]
    fn ai_surcharge_charged_only_for_ai_enriched_pages() {
        let f = FeaturesConfig::from_cli_args(
            false,
            false,
            false,
            false,
            true,
            true,
            Some("extract".to_string()),
        );
        let ai_surcharge_per_page = 10; // +5 ai_extraction + 5 ai_summary
                                        // `pages_ai` is a subset of `pages_http` (3 of the 10 pages counted
                                        // in `pages_http` were also AI-enriched), not additional pages.
        let none_enriched = crawl_credits(10, 0, 0, &f);
        let three_enriched = crawl_credits(10, 0, 3, &f);
        assert_eq!(
            three_enriched - none_enriched,
            ai_surcharge_per_page * 3,
            "surcharge must apply to exactly the 3 AI-enriched pages, not all 10"
        );
    }
}
