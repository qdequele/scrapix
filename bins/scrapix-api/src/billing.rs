//! Credit consumption and billing logic.
//!
//! Thin wrapper over `scrapix_billing` that converts between the library's
//! error types and the API server's `ApiError`, and maps `ScrapeFormat` to
//! the feature count expected by the billing crate.

use crate::{ApiError, ScrapeFormat};
use scrapix_core::{CrawlerType, FeaturesConfig};

// Re-export constants from the billing crate.
pub use scrapix_billing::{MAP_CREDITS, SEARCH_CREDITS};

// ============================================================================
// BillingError → ApiError conversion
// ============================================================================

impl From<scrapix_billing::BillingError> for ApiError {
    fn from(e: scrapix_billing::BillingError) -> Self {
        let code = e.code();
        ApiError::new(e.to_string(), code)
    }
}

// ============================================================================
// Public API (delegates to scrapix_billing)
// ============================================================================

pub(crate) async fn check_credits(
    pool: &sqlx::PgPool,
    account_id: &str,
    required_amount: i64,
) -> Result<i64, ApiError> {
    Ok(scrapix_billing::check_credits(pool, account_id, required_amount).await?)
}

// ============================================================================
// Credit calculation
// ============================================================================

/// Compute credits for a /scrape request.
///
/// Counts feature formats from the `ScrapeFormat` slice, then delegates to
/// `scrapix_billing::scrape_credits`.
pub(crate) fn scrape_credits(
    formats: &[ScrapeFormat],
    has_ai_summary: bool,
    has_ai_extraction: bool,
) -> i64 {
    let feature_count = formats
        .iter()
        .filter(|f| {
            matches!(
                f,
                ScrapeFormat::Markdown
                    | ScrapeFormat::Links
                    | ScrapeFormat::Metadata
                    | ScrapeFormat::Screenshot
                    | ScrapeFormat::Schema
                    | ScrapeFormat::Blocks
            )
        })
        .count() as i64;

    scrapix_billing::scrape_credits(feature_count, has_ai_summary, has_ai_extraction)
}

/// Credits of one AI call made by `POST /extract` (the price of an AI
/// extraction on `/scrape`).
pub(crate) fn extract_ai_call_credits() -> i64 {
    scrapix_billing::scrape_credits(0, false, true)
}

/// Re-export crawl credit calculation directly.
pub fn crawl_credits_per_page(crawler_type: &CrawlerType, features: &FeaturesConfig) -> i64 {
    scrapix_billing::crawl_credits_per_page(crawler_type, features)
}

/// Re-export delivery-based crawl credit calculation directly (D4/R4).
pub fn crawl_credits(
    pages_http: u64,
    pages_browser: u64,
    pages_ai: u64,
    features: &FeaturesConfig,
) -> i64 {
    scrapix_billing::crawl_credits(pages_http, pages_browser, pages_ai, features)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scrape_credits_minimum_one() {
        let credits = scrape_credits(&[], false, false);
        assert_eq!(credits, 1);
    }

    #[test]
    fn test_scrape_credits_base_formats_free() {
        let credits = scrape_credits(
            &[
                ScrapeFormat::Html,
                ScrapeFormat::RawHtml,
                ScrapeFormat::Content,
            ],
            false,
            false,
        );
        assert_eq!(credits, 1);
    }

    #[test]
    fn test_scrape_credits_feature_formats() {
        let credits = scrape_credits(&[ScrapeFormat::Markdown], false, false);
        assert_eq!(credits, 1);

        let credits = scrape_credits(
            &[
                ScrapeFormat::Markdown,
                ScrapeFormat::Links,
                ScrapeFormat::Metadata,
            ],
            false,
            false,
        );
        assert_eq!(credits, 3);

        let credits = scrape_credits(
            &[
                ScrapeFormat::Markdown,
                ScrapeFormat::Links,
                ScrapeFormat::Metadata,
                ScrapeFormat::Screenshot,
                ScrapeFormat::Schema,
                ScrapeFormat::Blocks,
            ],
            false,
            false,
        );
        assert_eq!(credits, 6);
    }

    #[test]
    fn test_scrape_credits_ai_summary() {
        let credits = scrape_credits(&[], true, false);
        assert_eq!(credits, 5);
    }

    #[test]
    fn test_scrape_credits_ai_extraction() {
        let credits = scrape_credits(&[], false, true);
        assert_eq!(credits, 5);
    }

    #[test]
    fn test_scrape_credits_ai_both() {
        let credits = scrape_credits(&[], true, true);
        assert_eq!(credits, 10);
    }

    #[test]
    fn test_scrape_credits_combined() {
        let credits = scrape_credits(&[ScrapeFormat::Markdown, ScrapeFormat::Schema], true, true);
        assert_eq!(credits, 12);
    }

    #[test]
    fn test_scrape_credits_mixed_base_and_feature() {
        let credits = scrape_credits(&[ScrapeFormat::Html, ScrapeFormat::Markdown], false, false);
        assert_eq!(credits, 1);
    }

    #[test]
    fn test_crawl_credits_http_no_features() {
        let features = FeaturesConfig::default();
        let credits = crawl_credits_per_page(&CrawlerType::Http, &features);
        assert_eq!(credits, 1);
    }

    #[test]
    fn test_crawl_credits_browser_base() {
        let features = FeaturesConfig::default();
        let credits = crawl_credits_per_page(&CrawlerType::Browser, &features);
        assert_eq!(credits, 2);
    }

    #[test]
    fn test_crawl_credits_with_features() {
        let features = FeaturesConfig::from_cli_args(true, true, true, true, false, false, None);
        let credits = crawl_credits_per_page(&CrawlerType::Http, &features);
        assert_eq!(credits, 5);
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
        let credits = crawl_credits_per_page(&CrawlerType::Http, &features);
        assert_eq!(credits, 11);
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
        let credits = crawl_credits_per_page(&CrawlerType::Browser, &features);
        assert_eq!(credits, 16);
    }

    #[test]
    fn test_extract_ai_call_credits() {
        assert_eq!(extract_ai_call_credits(), 5);
    }

    #[test]
    fn test_map_credits_constant() {
        assert_eq!(MAP_CREDITS, 2);
    }
}
