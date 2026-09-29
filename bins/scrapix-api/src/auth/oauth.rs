//! OAuth 2.1 Bearer-token validation for the crawl engine.
//!
//! The full OAuth provider (RFC 8414 metadata, RFC 7591 dynamic client
//! registration, PKCE authorize/token/revoke) moved to the Rails app
//! (`saas/app/controllers/oauth_controller.rb`, SCR-85 phase 8). Both
//! backends share the `oauth_tokens` table, so the engine keeps only the
//! hashed-token lookup used by its auth middleware. The expired code/token
//! sweep is owned by Rails.

use axum::http::StatusCode;
use sha2::{Digest, Sha256};
use sqlx::Row;

use super::AuthenticatedAccount;

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Validate a Bearer token and return the authenticated account.
pub async fn validate_bearer_token(
    pool: &sqlx::PgPool,
    token: &str,
) -> Result<AuthenticatedAccount, StatusCode> {
    let token_hash = hash_token(token);

    let row = sqlx::query(
        "SELECT t.user_id, t.expires_at, t.revoked, a.id AS account_id, a.tier \
         FROM oauth_tokens t \
         JOIN account_members m ON m.user_id = t.user_id \
         JOIN accounts a ON a.id = m.account_id \
         WHERE t.token_hash = $1 AND t.token_type = 'access' \
         LIMIT 1",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::UNAUTHORIZED)?;

    let revoked: bool = row.get("revoked");
    if revoked {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let expires_at: chrono::DateTime<chrono::Utc> = row.get("expires_at");
    if chrono::Utc::now() > expires_at {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let account_id: uuid::Uuid = row.get("account_id");
    let tier: String = row.get("tier");

    Ok(AuthenticatedAccount {
        account_id: account_id.to_string(),
        tier,
        api_key_id: None,
    })
}
