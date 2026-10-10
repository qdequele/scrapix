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

/// Credits for one scraped page: +1 per feature format, +5 for a delivered
/// AI summary, +5 for a delivered AI extraction, at least 1.
fn scrape_credits_for_count(
    feature_formats: i64,
    has_ai_summary: bool,
    has_ai_extraction: bool,
) -> i64 {
    let ai_cost = if has_ai_summary { 5 } else { 0 } + if has_ai_extraction { 5 } else { 0 };
    (feature_formats + ai_cost).max(1)
}

/// Credits for a `/scrape` (and a document scrape or `/parse` upload).
pub(crate) fn scrape_credits(
    formats: &[ScrapeFormat],
    has_ai_summary: bool,
    has_ai_extraction: bool,
) -> i64 {
    scrape_credits_for_count(
        feature_format_count(formats) as i64,
        has_ai_summary,
        has_ai_extraction,
    )
}

/// Credits of one AI call made by `POST /extract` (the price of an AI
/// extraction on `/scrape`).
pub(crate) fn extract_ai_call_credits() -> i64 {
    scrape_credits_for_count(0, false, true)
}

/// Non-AI per-page feature surcharge of a crawl: +1 per enabled feature
/// (metadata, markdown, block_split, schema, custom_selectors).
fn non_ai_feature_credits(features: &FeaturesConfig) -> i64 {
    page_feature_count(features) as i64
}

/// AI per-page surcharge: +5 for AI extraction, +5 for AI summary, applied
/// only per page that was actually AI-enriched.
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

/// Credits for a terminal `/crawl` job from what was delivered: 1 per HTTP
/// page, 2 per browser page, +1 per enabled non-AI feature per page, and
/// the AI surcharge only for the `pages_ai` pages that were AI-enriched.
/// `pages_http` and `pages_browser` are disjoint; `pages_ai` is a subset.
pub(crate) fn crawl_credits(
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

/// OCR surcharge per page recognized by OCR (cache hits are free).
pub(crate) const OCR_PAGE_CREDITS: i64 = 5;

/// Credits for `pages` OCR'd pages.
pub(crate) fn ocr_credits(pages: u64) -> i64 {
    (pages as i64).saturating_mul(OCR_PAGE_CREDITS)
}

/// Map credits: flat 2 per call.
pub(crate) const MAP_CREDITS: i64 = 2;

/// Search credits: flat 2 per call.
pub(crate) const SEARCH_CREDITS: i64 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_pages_cost_more_than_normal_pages() {
        const { assert!(OCR_PAGE_CREDITS > 1) };
        assert_eq!(ocr_credits(0), 0);
        assert_eq!(ocr_credits(3), 15);
    }

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
    fn scrape_credits_ai() {
        assert_eq!(scrape_credits(&[], true, false), 5);
        assert_eq!(scrape_credits(&[], false, true), 5);
        assert_eq!(scrape_credits(&[], true, true), 10);
        // 2 feature formats + AI summary + AI extraction = 2 + 5 + 5 = 12
        let two = [ScrapeFormat::Markdown, ScrapeFormat::Links];
        assert_eq!(scrape_credits(&two, true, true), 12);
    }

    #[test]
    fn extract_ai_call_costs_an_ai_extraction() {
        assert_eq!(extract_ai_call_credits(), 5);
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
        // 2 base + 4 features + 5 ai_extraction + 5 ai_summary = 16
        assert_eq!(crawl_credits(0, 1, 1, &f), 16);
    }

    /// 10 pages with only 3 AI-enriched pays the AI surcharge on exactly
    /// those 3 pages, not all 10.
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
        assert_eq!(crawl_credits(1, 0, 1, &f), 11, "1 base + 5 + 5");
        let none_enriched = crawl_credits(10, 0, 0, &f);
        let three_enriched = crawl_credits(10, 0, 3, &f);
        assert_eq!(three_enriched - none_enriched, 10 * 3);
    }
}
