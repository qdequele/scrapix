//! Read-only credit pre-check.
//!
//! Balance mutations are owned by the Rails control plane (the "lab"); the
//! engine only reads the balance and reports usage as lab events.

use crate::error::BillingError;

// ============================================================================
// Public API
// ============================================================================

/// Check that the account has at least `required_amount` credits.
/// Returns the current balance on success.
pub async fn check_credits(
    pool: &sqlx::PgPool,
    account_id: &str,
    required_amount: i64,
) -> Result<i64, BillingError> {
    let account_uuid = parse_uuid(account_id)?;

    let balance: i64 =
        sqlx::query_scalar("SELECT credits_balance FROM accounts WHERE id = $1 AND active = true")
            .bind(account_uuid)
            .fetch_optional(pool)
            .await?
            .ok_or(BillingError::AccountNotFound)?;

    if balance < required_amount {
        return Err(BillingError::InsufficientCredits {
            available: balance,
            required: required_amount,
        });
    }

    Ok(balance)
}

// ============================================================================
// Helpers
// ============================================================================

pub(crate) fn parse_uuid(id: &str) -> Result<uuid::Uuid, BillingError> {
    uuid::Uuid::parse_str(id).map_err(|_| BillingError::InvalidAccountId(id.to_string()))
}
