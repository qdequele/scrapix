//! Authentication middleware for the crawl engine.
//!
//! The SaaS control plane (signup/login/sessions, account/team, OAuth
//! provider, social login) lives in the Rails app (`saas/`, SCR-85). The
//! engine only *validates* credentials issued there — API keys, OAuth Bearer
//! tokens, and session JWTs — against the shared Postgres, and runs the
//! hourly OAuth token cleanup. Core primitives (JWT, types) are in
//! `scrapix-auth`.

pub(crate) mod admin_key;
pub(crate) mod middleware;
pub(crate) mod oauth;

// Re-export core auth primitives from the scrapix-auth crate.
pub use scrapix_auth::{AuthenticatedAccount, AuthenticatedUser, Claims};

pub use admin_key::AdminKey;

pub(crate) use middleware::{validate_api_key_or_session, ws_query_token_as_api_key};

use sqlx::{postgres::PgPoolOptions, PgPool};

/// How protected routes authenticate, fixed at startup (see `settings`).
#[derive(Clone)]
pub enum AuthMode {
    /// Standalone: one operator key.
    AdminKey(AdminKey),
    /// Hosted: API keys, OAuth Bearer tokens and session JWTs against the
    /// Rails-owned Postgres.
    Saas(std::sync::Arc<AuthState>),
    /// Standalone with `SCRAPIX_AUTH=disabled` (local dev only).
    Disabled,
}

/// Shared auth state: database pool + JWT secret
#[derive(Clone)]
pub struct AuthState {
    pub pool: PgPool,
    pub jwt_secret: String,
    /// Hosted only: the Lab's `LAB_SERVICE_TOKEN`. A Bearer equal to it plus
    /// an `X-Scrapix-Account-Id` header acts as that account (Rails → engine
    /// server-to-server calls).
    pub service_token: Option<AdminKey>,
}

impl AuthState {
    pub async fn new(
        database_url: &str,
        jwt_secret: String,
        service_token: Option<String>,
    ) -> Result<Self, sqlx::Error> {
        // Heroku Postgres requires SSL but doesn't include sslmode in DATABASE_URL,
        // while local dev Postgres has no TLS at all. sslmode=prefer negotiates TLS
        // when the server supports it and falls back to plaintext otherwise.
        let url = if !database_url.contains("sslmode=") {
            let sep = if database_url.contains('?') { "&" } else { "?" };
            format!("{database_url}{sep}sslmode=prefer")
        } else {
            database_url.to_string()
        };
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(&url)
            .await?;
        Ok(Self {
            pool,
            jwt_secret,
            // An empty token would match an empty Bearer: never accept one.
            service_token: service_token.filter(|t| !t.is_empty()).map(AdminKey::new),
        })
    }
}
