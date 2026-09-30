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

/// Credit pre-check against the Lab's effective balance minus usage this
/// engine recorded since that balance was fetched (spec: Lab split §3.2).
pub(crate) async fn check_credits(
    lab: &crate::lab_client::LabClient,
    account_id: &str,
    required_amount: i64,
) -> Result<i64, ApiError> {
    let answer = match lab.available_credits(account_id).await {
        // About to refuse on a cached snapshot: refresh once, so a top-up
        // made since counts immediately.
        Ok(Some(a)) if a.cached && a.credits < required_amount => {
            lab.refresh_credits(account_id).await
        }
        other => other,
    };
    match answer.map(|a| a.map(|a| a.credits)) {
        Ok(Some(available)) if available >= required_amount => Ok(available),
        Ok(Some(available)) => Err(scrapix_billing::BillingError::InsufficientCredits {
            available,
            required: required_amount,
        }
        .into()),
        Ok(None) => Err(scrapix_billing::BillingError::AccountNotFound.into()),
        Err(e) => {
            crate::lab_client::log_lab_error(&e, "credit check");
            Err(ApiError::new(
                "Billing service unavailable, retry shortly",
                "service_unavailable",
            )
            .with_retry_after(5))
        }
    }
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

#[cfg(test)]
mod lab_balance_tests {
    use super::check_credits;
    use crate::lab_client::{
        testing::{FakeLab, TOKEN},
        LabClient,
    };
    use serde_json::json;

    const ACCT: &str = "11111111-1111-1111-1111-111111111111";

    #[tokio::test]
    async fn service_call_balance_uses_account_lookup() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, TOKEN);
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "free", "credits": {"balance": 0}}),
        );
        assert_eq!(
            check_credits(&c, ACCT, 1).await.unwrap_err().code,
            "insufficient_credits"
        );
    }

    #[tokio::test]
    async fn enough_credits_then_local_usage_runs_out() {
        use crate::lab_events::{LabEvent, LabOutbox, MemoryOutbox};
        let lab = FakeLab::start().await;
        let outbox = std::sync::Arc::new(MemoryOutbox::default());
        let c = LabClient::new(&lab.url, TOKEN).with_undelivered_usage(outbox.clone());
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "free", "credits": {"balance": 10}}),
        );
        assert_eq!(check_credits(&c, ACCT, 3).await.unwrap(), 10);
        // Recorded (not yet delivered to the Lab), then noted.
        outbox
            .enqueue(&[LabEvent::usage(
                ACCT,
                None,
                "scrape",
                8,
                json!({}),
                "s".into(),
                None,
            )])
            .await
            .unwrap();
        c.note_usage(ACCT, 8);
        // The refresh before the 402 still counts the undelivered 8.
        let err = check_credits(&c, ACCT, 3).await.unwrap_err();
        assert_eq!(err.code, "insufficient_credits");
    }

    #[tokio::test]
    async fn a_top_up_counts_before_a_402_with_one_refresh_per_check() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, TOKEN);
        let account = |balance: i64| json!({"active": true, "account_id": ACCT, "tier": "free", "credits": {"balance": balance}});
        lab.set_account(ACCT, account(1));
        assert_eq!(check_credits(&c, ACCT, 1).await.unwrap(), 1);
        let calls = lab.calls();
        assert_eq!(check_credits(&c, ACCT, 1).await.unwrap(), 1);
        assert_eq!(lab.calls(), calls, "enough on the snapshot: no Lab call");

        lab.set_account(ACCT, account(10)); // topped up, snapshot still says 1
        assert_eq!(check_credits(&c, ACCT, 5).await.unwrap(), 10);
        assert_eq!(lab.calls(), calls + 1, "one refresh");

        lab.set_account(ACCT, account(2));
        c.note_usage(ACCT, 8); // snapshot: 10 - 8 = 2
        let calls = lab.calls();
        assert_eq!(
            check_credits(&c, ACCT, 5).await.unwrap_err().code,
            "insufficient_credits"
        );
        assert_eq!(lab.calls(), calls + 1, "still short: exactly one refresh");
    }

    #[tokio::test]
    async fn billing_unavailable_is_503_with_retry_after() {
        use axum::response::IntoResponse;
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, TOKEN);
        lab.set_down(true);
        let resp = check_credits(&c, ACCT, 1)
            .await
            .unwrap_err()
            .into_response();
        assert_eq!(resp.status(), 503);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    }

    #[tokio::test]
    async fn unknown_account_is_not_found_and_lab_down_is_503() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, TOKEN);
        assert_eq!(
            check_credits(&c, ACCT, 1).await.unwrap_err().code,
            "not_found"
        );
        lab.set_down(true);
        let other = "22222222-2222-2222-2222-222222222222";
        assert_eq!(
            check_credits(&c, other, 1).await.unwrap_err().code,
            "service_unavailable"
        );
    }
}
