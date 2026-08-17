//! Writer side of the shared `scheduled_emails` queue.
//!
//! The engine only *inserts* rows for the events it owns (job completed or
//! failed, auto-topup receipts/failures, low-balance warnings). Rendering
//! and delivery live in the Rails app (SCR-87 I4): a recurring SolidQueue
//! job drains the queue and sends via ActionMailer.

use sqlx::PgPool;
use tracing::{info, warn};

/// Schedule an email to be sent at `send_at`.
pub async fn schedule_email(
    pool: &PgPool,
    email_type: &str,
    recipient: &str,
    payload: serde_json::Value,
    send_at: chrono::DateTime<chrono::Utc>,
) {
    let result = sqlx::query(
        "INSERT INTO scheduled_emails (email_type, recipient, payload, send_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(email_type)
    .bind(recipient)
    .bind(&payload)
    .bind(send_at)
    .execute(pool)
    .await;

    match result {
        Ok(_) => info!(email_type, recipient, %send_at, "Email scheduled"),
        Err(e) => warn!(error = %e, email_type, recipient, "Failed to schedule email"),
    }
}

/// Schedule an email for immediate delivery (send_at = now).
pub async fn schedule_email_now(
    pool: &PgPool,
    email_type: &str,
    recipient: &str,
    payload: serde_json::Value,
) {
    schedule_email(pool, email_type, recipient, payload, chrono::Utc::now()).await;
}

/// Fetch the owner's email for an account (billing notifications).
pub async fn get_account_email(pool: &PgPool, account_id: uuid::Uuid) -> Option<String> {
    sqlx::query_scalar(
        "SELECT u.email FROM users u \
         JOIN account_members m ON m.user_id = u.id \
         WHERE m.account_id = $1 AND m.role = 'owner' \
         LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Fetch the owner's email for an account, only if they opted into job
/// notification emails.
pub async fn get_account_email_for_job_notification(
    pool: &PgPool,
    account_id: uuid::Uuid,
) -> Option<String> {
    sqlx::query_scalar(
        "SELECT u.email FROM users u \
         JOIN account_members m ON m.user_id = u.id \
         WHERE m.account_id = $1 AND m.role = 'owner' \
         AND u.notify_job_emails = true \
         LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}
