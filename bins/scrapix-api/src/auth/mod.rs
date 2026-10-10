//! Authentication middleware for the crawl engine.
//!
//! The SaaS control plane (signup/login/sessions, account/team, OAuth
//! provider, social login) lives in the Lab (Rails app, meilisearch/lab). The engine
//! only *validates* credentials issued there — API keys, OAuth Bearer tokens,
//! session JWTs and the Lab's service token — and resolves every one of them
//! through the Lab's internal API over HTTP (`LabClient`); it never reads the
//! Lab's database. Core identity types are in `scrapix-auth`.

pub(crate) mod admin_key;
pub(crate) mod middleware;

// Re-export the core identity types from the scrapix-auth crate.
pub use scrapix_auth::{AuthenticatedAccount, Limits};

pub use admin_key::AdminKey;

pub(crate) use middleware::{validate_api_key_or_session, ws_query_token_as_api_key};

/// How protected routes authenticate, fixed at startup (see `settings`).
#[derive(Clone)]
pub enum AuthMode {
    /// Standalone: one operator key.
    AdminKey(AdminKey),
    /// Hosted: credentials resolved by the Lab.
    Saas(std::sync::Arc<AuthState>),
    /// Standalone with `SCRAPIX_AUTH=disabled` (local dev only).
    Disabled,
}

/// Hosted auth: every credential is resolved by the Lab (`LabClient`);
/// the engine never reads the Lab's database.
#[derive(Clone)]
pub struct AuthState {
    pub(crate) lab: std::sync::Arc<crate::lab_client::LabClient>,
    /// The Lab's `LAB_SERVICE_TOKEN`. A Bearer equal to it plus an
    /// `X-Scrapix-Account-Id` header acts as that account.
    pub service_token: Option<AdminKey>,
}

impl AuthState {
    pub(crate) fn new(
        lab: std::sync::Arc<crate::lab_client::LabClient>,
        service_token: Option<String>,
    ) -> Self {
        // An empty token would match an empty Bearer: never accept one.
        Self {
            lab,
            service_token: service_token.filter(|t| !t.is_empty()).map(AdminKey::new),
        }
    }
}
