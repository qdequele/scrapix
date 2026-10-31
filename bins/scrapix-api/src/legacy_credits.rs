//! Pre-contract-v2 credit formula, sent as `credits` on usage events during
//! the transition release; delete with the `credits` field next release.
//!
//! A byte-for-byte port of what the engine charged before contract v2
//! (`scrapix-billing`'s `credits.rs` and the API's `billing.rs` wrappers at
//! commit f2ab8d2). The Lab debits `credits` as-is for product `scrapix`
//! (lab-events schema `usageData.credits`) and stores the `units` next to
//! it. Never used for a pre-check: pre-checks stay "balance > 0" plus the
//! plan limits. The feature counters it prices are the permanent
//! `usage_units` ones (also reported as `feature_pages`), so deleting this
//! module touches nothing else.

use crate::usage_units::{feature_format_count, page_feature_count};
use crate::ScrapeFormat;
use scrapix_core::FeaturesConfig;

/// Credits for a `/scrape` (and a document scrape or `/parse` upload): +1
/// per feature format, at least 1. The delivered AI summary and extraction
/// (f2ab8d2: +5 each) add nothing: the team's AI gateway meters them.
pub(crate) fn scrape_credits(
    formats: &[ScrapeFormat],
    _has_ai_summary: bool,
    _has_ai_extraction: bool,
) -> i64 {
    (feature_format_count(formats) as i64).max(1)
}

/// Non-AI per-page feature surcharge of a crawl: +1 per enabled feature
/// (metadata, markdown, block_split, schema, custom_selectors).
fn non_ai_feature_credits(features: &FeaturesConfig) -> i64 {
    page_feature_count(features) as i64
}

/// Credits for a terminal `/crawl` job from what was delivered: 1 per HTTP
/// page, 2 per browser page, +1 per enabled non-AI feature per page.
/// `pages_http` and `pages_browser` are disjoint. The AI-enriched pages
/// (`pages_ai`, f2ab8d2: +5 per AI feature each) add nothing: the team's AI
/// gateway meters them.
pub(crate) fn crawl_credits(
    pages_http: u64,
    pages_browser: u64,
    _pages_ai: u64,
    features: &FeaturesConfig,
) -> i64 {
    let feature_credits = non_ai_feature_credits(features);
    let http_credits = pages_http as i64 * (1 + feature_credits);
    let browser_credits = pages_browser as i64 * (2 + feature_credits);
    http_credits + browser_credits
}

/// Credits for `pages` OCR'd pages: none (f2ab8d2: +5 each). Vision OCR is
/// AI work, metered by the team's AI gateway; the pages are still reported
/// (`pages_ocr`).
pub(crate) fn ocr_credits(_pages: u64) -> i64 {
    0
}

/// Map credits: flat 2 per call.
pub(crate) const MAP_CREDITS: i64 = 2;

/// Search credits: flat 2 per call.
pub(crate) const SEARCH_CREDITS: i64 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrape_credits_minimum_one() {
        assert_eq!(scrape_credits(&[], false, false), 1);
    }

    #[test]
    fn scrape_credits_base_formats_free() {
        let base = [
            ScrapeFormat::Html,
            ScrapeFormat::RawHtml,
            ScrapeFormat::Content,
        ];
        assert_eq!(feature_format_count(&base), 0);
        assert_eq!(scrape_credits(&base, false, false), 1);
    }

    #[test]
    fn scrape_credits_feature_formats() {
        assert_eq!(scrape_credits(&[ScrapeFormat::Markdown], false, false), 1);
        let three = [
            ScrapeFormat::Markdown,
            ScrapeFormat::Links,
            ScrapeFormat::Metadata,
        ];
        assert_eq!(feature_format_count(&three), 3);
        assert_eq!(scrape_credits(&three, false, false), 3);
        let six = [
            ScrapeFormat::Markdown,
            ScrapeFormat::Links,
            ScrapeFormat::Metadata,
            ScrapeFormat::Screenshot,
            ScrapeFormat::Schema,
            ScrapeFormat::Blocks,
        ];
        assert_eq!(scrape_credits(&six, false, false), 6);
    }

    #[test]
    fn scrape_credits_ignore_ai() {
        // The team's AI gateway meters AI: a summary or an extraction adds
        // nothing to the page.
        assert_eq!(scrape_credits(&[], true, false), 1);
        assert_eq!(scrape_credits(&[], false, true), 1);
        assert_eq!(scrape_credits(&[], true, true), 1);
        let two = [ScrapeFormat::Markdown, ScrapeFormat::Links];
        assert_eq!(scrape_credits(&two, true, true), 2);
    }

    #[test]
    fn flat_request_prices() {
        assert_eq!(MAP_CREDITS, 2);
        assert_eq!(SEARCH_CREDITS, 2);
    }

    #[test]
    fn crawl_credits_base_rates() {
        let f = FeaturesConfig::default();
        assert_eq!(non_ai_feature_credits(&f), 0);
        assert_eq!(crawl_credits(1, 0, 0, &f), 1, "http page");
        assert_eq!(crawl_credits(0, 1, 0, &f), 2, "browser page");
        assert_eq!(crawl_credits(10, 0, 0, &f), 10);
        assert_eq!(crawl_credits(0, 10, 0, &f), 20);
        assert!(crawl_credits(8, 2, 0, &f) > crawl_credits(10, 0, 0, &f));
    }

    #[test]
    fn crawl_credits_with_non_ai_features() {
        let f = FeaturesConfig::from_cli_args(true, true, true, true, false, false, None);
        assert_eq!(non_ai_feature_credits(&f), 4);
        // 1 base + 4 features = 5 per http page, 2 + 4 = 6 per browser page.
        assert_eq!(crawl_credits(1, 0, 0, &f), 5);
        assert_eq!(crawl_credits(10, 0, 0, &f), 50);
        assert_eq!(crawl_credits(0, 10, 0, &f), 60);
    }

    #[test]
    fn crawl_credits_with_every_feature() {
        let f = FeaturesConfig::from_cli_args(
            true,
            true,
            true,
            true,
            true,
            true,
            Some("extract".to_string()),
        );
        // 2 base + 4 features; the AI features add nothing.
        assert_eq!(crawl_credits(0, 1, 1, &f), 6);
    }

    /// The team's AI gateway meters crawl AI and OCR: AI-enriched and OCR'd
    /// pages cost what any other page costs.
    #[test]
    fn crawl_credits_ignore_ai_and_ocr_pages() {
        let f = FeaturesConfig::from_cli_args(
            true,
            false,
            false,
            false,
            true,
            true,
            Some("extract".to_string()),
        );
        assert_eq!(crawl_credits(10, 0, 10, &f), crawl_credits(10, 0, 0, &f));
        assert_eq!(crawl_credits(0, 4, 4, &f), crawl_credits(0, 4, 0, &f));
        // 1 base + 1 feature (metadata) per http page.
        assert_eq!(crawl_credits(10, 0, 10, &f), 20);
        assert_eq!(ocr_credits(10), 0);
    }
}
