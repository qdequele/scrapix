//! Shared authentication types.

/// Plan limits the Lab serves with every identity (platform contract v2 §5).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Limits {
    pub concurrent_jobs: i64,
    pub rate_limit_rpm: i64,
    pub max_depth: u32,
    pub js_rendering: bool,
}

/// The account a request acts as, whatever the credential (API key, OAuth
/// Bearer, session or service call).
#[derive(Debug, Clone)]
pub struct AuthenticatedAccount {
    pub account_id: String,
    pub tier: String,
    pub api_key_id: Option<String>,
    /// Member role for session/OAuth principals (`owner`/`admin`/`member`/`viewer`); None for API keys and service calls.
    pub role: Option<String>,
    /// `None` when the Lab did not send limits (contract v1).
    pub limits: Option<Limits>,
}

/// User information extracted from a validated JWT session.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub user_id: uuid::Uuid,
    pub email: String,
    /// If set, the user wants to operate on this specific account (from X-Account-Id header).
    pub selected_account_id: Option<uuid::Uuid>,
}
