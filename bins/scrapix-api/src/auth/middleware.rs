use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use axum_extra::extract::CookieJar;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::sync::Arc;
use tracing::{debug, warn};

use super::{AuthState, AuthenticatedAccount, AuthenticatedUser};
use scrapix_auth::jwt;

/// Header naming the account a Rails service call acts as (hosted only,
/// together with `Authorization: Bearer <LAB_SERVICE_TOKEN>`).
pub(crate) const SERVICE_ACCOUNT_HEADER: &str = "X-Scrapix-Account-Id";

#[derive(Debug, Serialize)]
pub(crate) struct AuthError {
    error: String,
    code: String,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, Json(self)).into_response()
    }
}

impl AuthError {
    pub(crate) fn new(error: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: code.into(),
        }
    }
}

/// Hash an API key using SHA-256
fn hash_api_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Hosted WebSocket routes only, layered in front of
/// [`validate_api_key_or_session`]: browsers can't set headers on an
/// upgrade, so when the request carries neither `Authorization` nor
/// `X-API-Key`, the percent-decoded `?token=` value is presented as an
/// `X-API-Key` (same rule as standalone's `validate_admin_key_ws`: a header,
/// when present, always wins). Logs redact `token` query values.
pub(crate) async fn ws_query_token_as_api_key(mut request: Request, next: Next) -> Response {
    let headers = request.headers();
    if !headers.contains_key("Authorization") && !headers.contains_key("X-API-Key") {
        if let Some(value) = super::admin_key::query_token(request.uri().query())
            .and_then(|t| axum::http::HeaderValue::from_str(&t).ok())
        {
            request.headers_mut().insert("X-API-Key", value);
        }
    }
    next.run(request).await
}

/// Middleware: accept API key (X-API-Key), Bearer token (Authorization: Bearer), or session cookie.
/// This allows external API clients, CLI (OAuth), and the console to access protected routes.
pub(crate) async fn validate_api_key_or_session(
    State(auth_state): State<Arc<AuthState>>,
    jar: CookieJar,
    mut request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    // Try Bearer token (OAuth access token) first
    if let Some(bearer) = request
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        let bearer = bearer.to_string();

        // Service call from the Lab (Rails): the shared token plus an explicit
        // account. A matching token never falls through to OAuth, and a
        // non-matching one is handled exactly as before.
        if let Some(ref service) = auth_state.service_token {
            if service.matches(&bearer) {
                let account = request
                    .headers()
                    .get(SERVICE_ACCOUNT_HEADER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<uuid::Uuid>().ok())
                    .ok_or_else(|| {
                        AuthError::new(
                            "Service call without a valid X-Scrapix-Account-Id",
                            "invalid_service_call",
                        )
                    })?;
                let tier: Option<String> =
                    sqlx::query_scalar("SELECT tier FROM accounts WHERE id = $1 AND active = true")
                        .bind(account)
                        .fetch_optional(&auth_state.pool)
                        .await
                        .map_err(|e| {
                            warn!(error = %e, "Database error during service-call account lookup");
                            AuthError::new(
                                "Authentication service unavailable",
                                "auth_service_error",
                            )
                        })?;
                let tier = tier.ok_or_else(|| {
                    AuthError::new("Unknown or inactive account", "invalid_service_call")
                })?;
                debug!(account_id = %account, tier = %tier, "Service call authenticated");
                request.extensions_mut().insert(AuthenticatedAccount {
                    account_id: account.to_string(),
                    tier,
                    api_key_id: None,
                });
                return Ok(next.run(request).await);
            }
        }

        debug!("Validating Bearer token");

        match super::oauth::validate_bearer_token(&auth_state.pool, &bearer).await {
            Ok(account) => {
                debug!(account_id = %account.account_id, tier = %account.tier, "Bearer token validated");
                request.extensions_mut().insert(account);
                return Ok(next.run(request).await);
            }
            Err(_) => {
                return Err(AuthError {
                    error: "Invalid or expired Bearer token".to_string(),
                    code: "invalid_bearer_token".to_string(),
                });
            }
        }
    }

    // Try API key
    if let Some(api_key) = request
        .headers()
        .get("X-API-Key")
        .and_then(|h| h.to_str().ok())
    {
        if !api_key.starts_with("sk_live_") && !api_key.starts_with("sk_test_") {
            return Err(AuthError {
                error: "Invalid API key format".to_string(),
                code: "invalid_api_key".to_string(),
            });
        }

        let key_hash = hash_api_key(api_key);
        debug!(prefix = %api_key.get(..12).unwrap_or("???"), "Validating API key");

        let row =
            sqlx::query("SELECT account_id, tier, active, api_key_id FROM validate_api_key($1)")
                .bind(&key_hash)
                .fetch_optional(&auth_state.pool)
                .await
                .map_err(|e| {
                    warn!(error = %e, "Database error during API key validation");
                    AuthError {
                        error: "Authentication service unavailable".to_string(),
                        code: "auth_service_error".to_string(),
                    }
                })?
                .ok_or_else(|| AuthError {
                    error: "Invalid or inactive API key".to_string(),
                    code: "invalid_api_key".to_string(),
                })?;

        let account_id: uuid::Uuid = row.try_get("account_id").map_err(|_| AuthError {
            error: "Invalid API key".to_string(),
            code: "invalid_api_key".to_string(),
        })?;
        let tier: String = row.try_get("tier").map_err(|_| AuthError {
            error: "Invalid API key".to_string(),
            code: "invalid_api_key".to_string(),
        })?;
        let api_key_id: uuid::Uuid = row.try_get("api_key_id").map_err(|_| AuthError {
            error: "Invalid API key".to_string(),
            code: "invalid_api_key".to_string(),
        })?;
        let active: bool = row.try_get("active").unwrap_or(false);
        if !active {
            return Err(AuthError {
                error: "Account is inactive".to_string(),
                code: "account_inactive".to_string(),
            });
        }

        debug!(account_id = %account_id, tier = %tier, api_key_id = %api_key_id, "API key validated");
        request.extensions_mut().insert(AuthenticatedAccount {
            account_id: account_id.to_string(),
            tier,
            api_key_id: Some(api_key_id.to_string()),
        });

        return Ok(next.run(request).await);
    }

    // Fall back to session cookie
    let token = jar
        .get("scrapix_session")
        .map(|c| c.value().to_string())
        .ok_or_else(|| AuthError {
            error: "Missing API key or session".to_string(),
            code: "not_authenticated".to_string(),
        })?;

    let claims = jwt::decode_jwt(&token, &auth_state.jwt_secret).map_err(|_| AuthError {
        error: "Invalid or expired session".to_string(),
        code: "invalid_session".to_string(),
    })?;

    let user_id: uuid::Uuid = claims.sub.parse().map_err(|_| AuthError {
        error: "Invalid session".to_string(),
        code: "invalid_session".to_string(),
    })?;

    // Read optional X-Account-Id header for account switching
    let selected_account_id = request
        .headers()
        .get("X-Account-Id")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<uuid::Uuid>().ok());

    request.extensions_mut().insert(AuthenticatedUser {
        user_id,
        email: claims.email,
        selected_account_id,
    });

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body, http::Request as HttpRequest, middleware, routing::get, Extension, Router,
    };
    use tower::ServiceExt;

    #[test]
    fn test_hash_api_key() {
        let key = "sk_live_test123";
        let hash = hash_api_key(key);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash_api_key(key), hash);
        assert_ne!(hash_api_key("sk_live_other"), hash);
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn app(state: Arc<AuthState>) -> Router {
        Router::new()
            .route(
                "/p",
                get(|acct: Option<Extension<AuthenticatedAccount>>| async move {
                    acct.map(|Extension(a)| a.account_id).unwrap_or_default()
                }),
            )
            .route_layer(middleware::from_fn_with_state(
                state,
                validate_api_key_or_session,
            ))
    }

    fn lazy_state() -> Arc<AuthState> {
        // Unreachable: a DB-backed check fails fast instead of waiting out
        // the default 30 s acquire timeout.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy("postgres://x@127.0.0.1:1/x")
            .unwrap();
        Arc::new(AuthState {
            pool,
            jwt_secret: "s".into(),
            service_token: Some(crate::auth::AdminKey::new(TOKEN.into())),
        })
    }

    /// Status and the `code` field of the JSON error body.
    async fn status_and_code(app: Router, req: HttpRequest<Body>) -> (u16, String) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        (status, v["code"].as_str().unwrap_or_default().to_string())
    }

    #[tokio::test]
    async fn service_token_without_account_header_is_401() {
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status_and_code(app(lazy_state()), r).await,
            (401, "invalid_service_call".to_string())
        );
    }

    #[tokio::test]
    async fn service_token_with_malformed_account_is_401() {
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header(SERVICE_ACCOUNT_HEADER, "not-a-uuid")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status_and_code(app(lazy_state()), r).await,
            (401, "invalid_service_call".to_string())
        );
    }

    #[tokio::test]
    async fn service_token_resolves_the_account() {
        let Some(pool) = crate::job_store::postgres::test_rails_like_pool().await else {
            eprintln!("skipped: postgres backend unavailable");
            return;
        };
        let account = uuid::Uuid::new_v4();
        sqlx::query(include_str!("../../tests/fixtures/rails_like_account.sql"))
            .bind(account)
            .execute(&pool)
            .await
            .unwrap();
        let state = Arc::new(AuthState {
            pool,
            jwt_secret: "s".into(),
            service_token: Some(crate::auth::AdminKey::new(TOKEN.into())),
        });
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header(SERVICE_ACCOUNT_HEADER, account.to_string())
            .body(Body::empty())
            .unwrap();
        let resp = app(state.clone()).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), account.to_string());

        // An unknown account is rejected even with the right token.
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header(SERVICE_ACCOUNT_HEADER, uuid::Uuid::new_v4().to_string())
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status_and_code(app(state), r).await,
            (401, "invalid_service_call".to_string())
        );
    }

    #[tokio::test]
    async fn other_bearer_tokens_still_go_to_oauth() {
        // wrong token -> OAuth path -> lazy pool unreachable -> 401 invalid_bearer_token
        let r = HttpRequest::get("/p")
            .header("Authorization", "Bearer nope")
            .body(Body::empty())
            .unwrap();
        let resp = app(lazy_state()).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), 401);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(std::str::from_utf8(&body)
            .unwrap()
            .contains("invalid_bearer_token"));
    }

    #[tokio::test]
    async fn without_a_configured_token_the_account_header_is_ignored() {
        let mut state = lazy_state();
        Arc::get_mut(&mut state).unwrap().service_token = None;
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header(SERVICE_ACCOUNT_HEADER, uuid::Uuid::new_v4().to_string())
            .body(Body::empty())
            .unwrap();
        let resp = app(state).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), 401);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(std::str::from_utf8(&body)
            .unwrap()
            .contains("invalid_bearer_token"));
    }
}
