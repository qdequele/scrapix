//! The hosted engine's balance pre-check (platform contract v2 §8). Prices
//! live in the Lab (`saas/config/pricing.yml`): the engine reports units
//! and only refuses to start billable work when the balance is gone.

use crate::ApiError;

#[derive(Debug)]
pub(crate) enum BillingError {
    InsufficientCredits { available: i64 },
    AccountNotFound,
}

impl BillingError {
    fn code(&self) -> &'static str {
        match self {
            BillingError::InsufficientCredits { .. } => "insufficient_credits",
            BillingError::AccountNotFound => "not_found",
        }
    }
}

impl std::fmt::Display for BillingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BillingError::InsufficientCredits { available } => {
                write!(
                    f,
                    "Insufficient credits: {available} available, top up to continue"
                )
            }
            BillingError::AccountNotFound => f.write_str("Account not found or inactive"),
        }
    }
}

impl From<BillingError> for ApiError {
    fn from(e: BillingError) -> Self {
        ApiError::new(e.to_string(), e.code())
    }
}

/// Refuse when the Lab's balance for `account_id` is `<= 0`. A snapshot
/// that says so is refreshed once first, so a top-up made since counts.
/// While the Lab is unreachable the last snapshot is served for the stale
/// window (`Timing::stale_grace`, 300 s); past it the check is a 503 with
/// `Retry-After: 5` (spec §8.1), which is what `LabClient::refresh_credits`
/// already does today (`Err(LabError::Unavailable)` once no snapshot is
/// usable): this function only maps that error, it adds no new rule.
pub(crate) async fn check_credits(
    lab: &crate::lab_client::LabClient,
    account_id: &str,
) -> Result<i64, ApiError> {
    let answer = match lab.available_credits(account_id).await {
        Ok(Some(a)) if a.cached && a.credits <= 0 => lab.refresh_credits(account_id).await,
        other => other,
    };
    match answer.map(|a| a.map(|a| a.credits)) {
        Ok(Some(available)) if available > 0 => Ok(available),
        Ok(Some(available)) => Err(BillingError::InsufficientCredits { available }.into()),
        Ok(None) => Err(BillingError::AccountNotFound.into()),
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

#[cfg(test)]
mod lab_balance_tests {
    use super::check_credits;
    use crate::lab_client::{
        testing::{FakeLab, INSTANCE_ID, SECRET},
        LabClient,
    };
    use serde_json::json;

    const ACCT: &str = "11111111-1111-1111-1111-111111111111";

    fn account(balance: i64) -> serde_json::Value {
        json!({"active": true, "account_id": ACCT, "tier": "free", "credits": {"balance": balance}})
    }

    #[tokio::test]
    async fn a_zero_or_negative_balance_is_402() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        lab.set_account(ACCT, account(0));
        assert_eq!(
            check_credits(&c, ACCT).await.unwrap_err().code,
            "insufficient_credits"
        );
        lab.set_account(ACCT, account(-3));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // The refusal refreshes once, so the new negative balance is seen.
        assert_eq!(
            check_credits(&c, ACCT).await.unwrap_err().code,
            "insufficient_credits"
        );
    }

    #[tokio::test]
    async fn a_top_up_counts_before_a_402_with_one_refresh_per_check() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        lab.set_account(ACCT, account(1));
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 1);
        let calls = lab.calls();
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 1);
        assert_eq!(lab.calls(), calls, "positive snapshot: no Lab call");
        lab.set_account(ACCT, account(0));
        assert_eq!(
            check_credits(&c, ACCT).await.unwrap(),
            1,
            "snapshot still positive, served cached"
        );
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        assert_eq!(
            check_credits(&c, ACCT).await.unwrap_err().code,
            "insufficient_credits"
        );
        lab.set_account(ACCT, account(10));
        let calls = lab.calls();
        assert_eq!(
            check_credits(&c, ACCT).await.unwrap(),
            10,
            "refreshed once before refusing"
        );
        assert_eq!(lab.calls(), calls + 1);
    }

    #[tokio::test]
    async fn billing_unavailable_is_503_with_retry_after() {
        use axum::response::IntoResponse;
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        lab.set_down(true);
        let resp = check_credits(&c, ACCT).await.unwrap_err().into_response();
        assert_eq!(resp.status(), 503);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    }

    /// Spec 8.1: inside the stale window the last snapshot is served; past
    /// it, with the Lab still unreachable, the pre-check fails closed (503).
    /// This is today's `refresh_credits` behaviour, kept as is.
    #[tokio::test]
    async fn past_the_stale_window_with_the_lab_down_is_503() {
        use crate::lab_client::Timing;
        use std::time::Duration;
        let lab = FakeLab::start().await;
        let c = LabClient::with_timing(
            &lab.url,
            INSTANCE_ID,
            SECRET,
            Timing {
                default_ttl: Duration::from_millis(100),
                stale_grace: Duration::from_millis(100),
                stale_retry: Duration::from_millis(20),
                ..Timing::default()
            },
        );
        lab.set_account(ACCT, account(5));
        assert_eq!(check_credits(&c, ACCT).await.unwrap(), 5);
        lab.set_down(true);
        tokio::time::sleep(Duration::from_millis(120)).await; // past ttl, inside grace
        assert_eq!(
            check_credits(&c, ACCT).await.unwrap(),
            5,
            "stale snapshot served"
        );
        tokio::time::sleep(Duration::from_millis(150)).await; // past ttl + grace
        assert_eq!(
            check_credits(&c, ACCT).await.unwrap_err().code,
            "service_unavailable"
        );
    }

    #[tokio::test]
    async fn unknown_account_is_not_found_and_lab_down_is_503() {
        let lab = FakeLab::start().await;
        let c = LabClient::new(&lab.url, INSTANCE_ID, SECRET);
        assert_eq!(check_credits(&c, ACCT).await.unwrap_err().code, "not_found");
        lab.set_down(true);
        let other = "22222222-2222-2222-2222-222222222222";
        assert_eq!(
            check_credits(&c, other).await.unwrap_err().code,
            "service_unavailable"
        );
    }
}
