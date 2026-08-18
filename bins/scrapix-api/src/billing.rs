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
// Trait implementations for scrapix-billing
// ============================================================================

/// Implements [`scrapix_billing::PaymentProvider`] by delegating to the Stripe
/// `charge_auto_topup` function in `crate::stripe`.
pub(crate) struct StripePaymentProvider<'a> {
    pub client: &'a stripe::Client,
}

#[async_trait::async_trait]
impl scrapix_billing::PaymentProvider for StripePaymentProvider<'_> {
    async fn charge_auto_topup(
        &self,
        pool: &sqlx::PgPool,
        account_id: uuid::Uuid,
        credits: i64,
    ) -> Result<String, String> {
        crate::stripe::charge_auto_topup(self.client, pool, account_id, credits).await
    }
}

/// Implements [`scrapix_billing::BillingNotifier`] by queueing rows in the
/// shared `scheduled_emails` table (delivered by the Rails app).
pub(crate) struct QueueBillingNotifier;

impl QueueBillingNotifier {
    fn queue(
        pool: &sqlx::PgPool,
        account_id: uuid::Uuid,
        email_type: &'static str,
        payload_for: impl FnOnce() -> serde_json::Value + Send + 'static,
    ) {
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Some(email) = crate::email_scheduler::get_account_email(&pool, account_id).await
            {
                crate::email_scheduler::schedule_email_now(
                    &pool,
                    email_type,
                    &email,
                    payload_for(),
                )
                .await;
            }
        });
    }
}

impl scrapix_billing::BillingNotifier for QueueBillingNotifier {
    fn notify_auto_topup_success(
        &self,
        pool: &sqlx::PgPool,
        account_id: uuid::Uuid,
        credits: i64,
        amount_cents: i64,
        new_balance: i64,
    ) {
        Self::queue(pool, account_id, "auto_topup_receipt", move || {
            serde_json::json!({
                "credits": credits,
                "amount_cents": amount_cents,
                "new_balance": new_balance,
            })
        });
    }

    fn notify_auto_topup_failure(&self, pool: &sqlx::PgPool, account_id: uuid::Uuid, reason: &str) {
        let reason = reason.to_string();
        Self::queue(
            pool,
            account_id,
            "auto_topup_failed",
            move || serde_json::json!({ "reason": reason }),
        );
    }

    fn notify_low_balance(&self, pool: &sqlx::PgPool, account_id: uuid::Uuid, balance: i64) {
        Self::queue(
            pool,
            account_id,
            "low_balance",
            move || serde_json::json!({ "balance": balance }),
        );
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

/// Post-hoc billing for completed crawl work — always records the usage,
/// letting the balance go negative rather than leaving pages unbilled.
pub(crate) async fn deduct_crawl_usage(
    pool: &sqlx::PgPool,
    account_id: &str,
    amount: i64,
    description: &str,
    stripe_client: Option<&stripe::Client>,
) -> Result<i64, ApiError> {
    let notifier = QueueBillingNotifier;
    let notifier_ref: Option<&dyn scrapix_billing::BillingNotifier> = Some(&notifier);
    let provider = stripe_client.map(|client| StripePaymentProvider { client });
    let provider_ref: Option<&dyn scrapix_billing::PaymentProvider> = provider
        .as_ref()
        .map(|p| p as &dyn scrapix_billing::PaymentProvider);

    Ok(scrapix_billing::auto_topup::deduct_usage(
        pool,
        account_id,
        amount,
        "crawl",
        description,
        provider_ref,
        notifier_ref,
    )
    .await?)
}

pub(crate) async fn check_credits_and_deduct(
    pool: &sqlx::PgPool,
    account_id: &str,
    amount: i64,
    operation: &str,
    description: &str,
    stripe_client: Option<&stripe::Client>,
) -> Result<i64, ApiError> {
    let notifier = QueueBillingNotifier;
    let notifier_ref: Option<&dyn scrapix_billing::BillingNotifier> = Some(&notifier);

    let provider = stripe_client.map(|client| StripePaymentProvider { client });
    let provider_ref: Option<&dyn scrapix_billing::PaymentProvider> = provider
        .as_ref()
        .map(|p| p as &dyn scrapix_billing::PaymentProvider);

    Ok(scrapix_billing::auto_topup::check_credits_and_deduct(
        pool,
        account_id,
        amount,
        operation,
        description,
        provider_ref,
        notifier_ref,
    )
    .await?)
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

/// Re-export crawl credit calculation directly.
pub fn crawl_credits_per_page(crawler_type: &CrawlerType, features: &FeaturesConfig) -> i64 {
    scrapix_billing::crawl_credits_per_page(crawler_type, features)
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
    fn test_map_credits_constant() {
        assert_eq!(MAP_CREDITS, 2);
    }
}
