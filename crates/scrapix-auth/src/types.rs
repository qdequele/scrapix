//! Shared authentication types.

/// The account a request acts as, whatever the credential (API key, OAuth
/// Bearer, session or service call).
#[derive(Debug, Clone)]
pub struct AuthenticatedAccount {
    pub account_id: String,
    pub tier: String,
    pub api_key_id: Option<String>,
    /// Member role for session/OAuth principals (`owner`/`admin`/`member`/`viewer`); None for API keys and service calls.
    pub role: Option<String>,
}

/// User information extracted from a validated JWT session.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub user_id: uuid::Uuid,
    pub email: String,
    /// If set, the user wants to operate on this specific account (from X-Account-Id header).
    pub selected_account_id: Option<uuid::Uuid>,
}
