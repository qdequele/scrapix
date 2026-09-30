use axum::{
    extract::{Request, State},
    http::{header::RETRY_AFTER, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use axum_extra::extract::CookieJar;
use serde::Serialize;
use std::sync::Arc;
use tracing::{debug, warn};

use super::{AuthState, AuthenticatedAccount};
use crate::lab_client::{CredentialKind, Identity, LabError};

/// Header naming the account a Rails service call acts as (hosted only,
/// together with `Authorization: Bearer <LAB_SERVICE_TOKEN>`).
pub(crate) const SERVICE_ACCOUNT_HEADER: &str = "X-Scrapix-Account-Id";

/// Longest credential ever sent to the Lab; anything longer is rejected
/// locally with the credential's usual 401.
const MAX_CREDENTIAL_LEN: usize = 4096;

#[derive(Debug, Serialize)]
pub(crate) struct AuthError {
    error: String,
    code: String,
    #[serde(skip)]
    status: StatusCode,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let mut resp = (self.status, Json(&self)).into_response();
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            resp.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("5"));
        }
        resp
    }
}

impl AuthError {
    pub(crate) fn new(error: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: code.into(),
            status: StatusCode::UNAUTHORIZED,
        }
    }

    /// The Lab could not answer: fail closed with a retryable 503.
    pub(crate) fn unavailable() -> Self {
        Self {
            error: "Authentication service unavailable".into(),
            code: "auth_service_unavailable".into(),
            status: StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// A Lab answer as the middleware sees it: an identity, a 401 with the
/// credential's usual `msg`/`code` when the Lab says inactive, or a 503 for
/// any Lab error (unreachable, token rejected, unexpected 4xx).
fn resolved(
    r: Result<Option<Identity>, LabError>,
    msg: &'static str,
    code: &'static str,
) -> Result<Identity, AuthError> {
    match r {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(AuthError::new(msg, code)),
        Err(e) => {
            warn!(error = %e, "Lab lookup failed during authentication");
            Err(AuthError::unavailable())
        }
    }
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

/// Middleware: accept API key (X-API-Key), Bearer token (Authorization: Bearer),
/// session cookie, or a Lab service call. Every credential is resolved by the
/// Lab; a Lab outage is a 503, never a pass.
pub(crate) async fn validate_api_key_or_session(
    State(auth_state): State<Arc<AuthState>>,
    jar: CookieJar,
    mut request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    let lab = &auth_state.lab;

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
                let id = resolved(
                    lab.account(&account.to_string()).await,
                    "Unknown or inactive account",
                    "invalid_service_call",
                )?;
                debug!(account_id = %id.account_id, tier = %id.tier, "Service call authenticated");
                request.extensions_mut().insert(AuthenticatedAccount {
                    account_id: id.account_id,
                    tier: id.tier,
                    api_key_id: None,
                    role: None,
                });
                return Ok(next.run(request).await);
            }
        }

        if bearer.len() > MAX_CREDENTIAL_LEN {
            return Err(AuthError::new(
                "Invalid or expired Bearer token",
                "invalid_bearer_token",
            ));
        }
        let id = resolved(
            lab.introspect(CredentialKind::Bearer, &bearer, None).await,
            "Invalid or expired Bearer token",
            "invalid_bearer_token",
        )?;
        debug!(account_id = %id.account_id, tier = %id.tier, "Bearer token validated");
        request.extensions_mut().insert(AuthenticatedAccount {
            account_id: id.account_id,
            tier: id.tier,
            api_key_id: None,
            role: id.role,
        });
        return Ok(next.run(request).await);
    }

    // Try API key
    if let Some(api_key) = request
        .headers()
        .get("X-API-Key")
        .and_then(|h| h.to_str().ok())
    {
        if (!api_key.starts_with("sk_live_") && !api_key.starts_with("sk_test_"))
            || api_key.len() > MAX_CREDENTIAL_LEN
        {
            return Err(AuthError::new("Invalid API key format", "invalid_api_key"));
        }

        debug!(prefix = %api_key.get(..12).unwrap_or("???"), "Validating API key");
        let id = resolved(
            lab.introspect(CredentialKind::ApiKey, api_key, None).await,
            "Invalid or inactive API key",
            "invalid_api_key",
        )?;
        debug!(account_id = %id.account_id, tier = %id.tier, "API key validated");
        request.extensions_mut().insert(AuthenticatedAccount {
            account_id: id.account_id,
            tier: id.tier,
            api_key_id: id.api_key_id,
            role: None,
        });
        return Ok(next.run(request).await);
    }

    // Fall back to session cookie
    let token = jar
        .get("scrapix_session")
        .map(|c| c.value().to_string())
        .ok_or_else(|| AuthError::new("Missing API key or session", "not_authenticated"))?;
    if token.len() > MAX_CREDENTIAL_LEN {
        return Err(AuthError::new(
            "Invalid or expired session",
            "invalid_session",
        ));
    }

    // Optional X-Account-Id header for account switching
    let selected = request
        .headers()
        .get("X-Account-Id")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
        .map(|u| u.to_string());

    let id = resolved(
        lab.introspect(CredentialKind::Session, &token, selected.as_deref())
            .await,
        "Invalid or expired session",
        "invalid_session",
    )?;
    debug!(account_id = %id.account_id, tier = %id.tier, "Session validated");
    request.extensions_mut().insert(AuthenticatedAccount {
        account_id: id.account_id,
        tier: id.tier,
        api_key_id: None,
        role: id.role,
    });

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab_client::{
        testing::{FakeLab, TOKEN},
        LabClient,
    };
    use axum::{
        body::Body, http::Request as HttpRequest, middleware, routing::get, Extension, Router,
    };
    use serde_json::json;
    use tower::ServiceExt;

    const SERVICE: &str = "0123456789abcdef0123456789abcdef";
    const ACCT: &str = "11111111-1111-1111-1111-111111111111";

    fn app(state: Arc<AuthState>) -> Router {
        Router::new()
            .route(
                "/p",
                get(|a: Option<Extension<AuthenticatedAccount>>| async move {
                    a.map(|Extension(a)| {
                        format!("{}|{}|{}", a.account_id, a.tier, a.role.unwrap_or_default())
                    })
                    .unwrap_or_default()
                }),
            )
            .route_layer(middleware::from_fn_with_state(
                state,
                validate_api_key_or_session,
            ))
    }

    async fn state() -> (FakeLab, Arc<AuthState>) {
        let lab = FakeLab::start().await;
        let client = Arc::new(LabClient::new(&lab.url, TOKEN));
        (lab, Arc::new(AuthState::new(client, Some(SERVICE.into()))))
    }

    async fn call(app: Router, req: HttpRequest<Body>) -> (u16, String) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, String::from_utf8_lossy(&body).to_string())
    }

    #[tokio::test]
    async fn api_key_resolves_through_the_lab() {
        let (lab, s) = state().await;
        lab.set_credential("api_key", "sk_live_abc", FakeLab::identity(ACCT, "pro", 10));
        let r = HttpRequest::get("/p")
            .header("X-API-Key", "sk_live_abc")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(app(s), r).await, (200, format!("{ACCT}|pro|")));
    }

    #[tokio::test]
    async fn api_key_format_is_checked_locally() {
        let (lab, s) = state().await;
        let r = HttpRequest::get("/p")
            .header("X-API-Key", "nope")
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app(s), r).await;
        assert_eq!(status, 401);
        assert!(body.contains("invalid_api_key"));
        assert_eq!(lab.calls(), 0);
    }

    #[tokio::test]
    async fn oversized_credential_is_rejected_without_a_lab_call() {
        let (lab, s) = state().await;
        let r = HttpRequest::get("/p")
            .header("X-API-Key", format!("sk_live_{}", "a".repeat(5000)))
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(app(s.clone()), r).await.0, 401);
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {}", "b".repeat(5000)))
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(app(s.clone()), r).await.0, 401);
        let r = HttpRequest::get("/p")
            .header("Cookie", format!("scrapix_session={}", "c".repeat(5000)))
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(app(s), r).await.0, 401);
        assert_eq!(lab.calls(), 0);
    }

    #[tokio::test]
    async fn inactive_credentials_keep_todays_error_codes() {
        let (_lab, s) = state().await;
        let cases = [
            (
                HttpRequest::get("/p").header("X-API-Key", "sk_live_unknown"),
                "invalid_api_key",
            ),
            (
                HttpRequest::get("/p").header("Authorization", "Bearer unknown"),
                "invalid_bearer_token",
            ),
            (
                HttpRequest::get("/p").header("Cookie", "scrapix_session=unknown"),
                "invalid_session",
            ),
        ];
        for (req, code) in cases {
            let (status, body) = call(app(s.clone()), req.body(Body::empty()).unwrap()).await;
            assert_eq!(status, 401, "{code}");
            assert!(body.contains(code), "{body}");
        }
        let (status, body) =
            call(app(s), HttpRequest::get("/p").body(Body::empty()).unwrap()).await;
        assert_eq!(status, 401);
        assert!(body.contains("not_authenticated"));
    }

    #[tokio::test]
    async fn session_passes_the_selected_account_and_carries_the_role() {
        let (lab, s) = state().await;
        let mut v = FakeLab::identity(ACCT, "free", 1);
        v["role"] = json!("viewer");
        lab.set_credential("session", "jwt1", v);
        let r = HttpRequest::get("/p")
            .header("Cookie", "scrapix_session=jwt1")
            .header("X-Account-Id", ACCT)
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(app(s), r).await, (200, format!("{ACCT}|free|viewer")));
    }

    #[tokio::test]
    async fn lab_down_is_503_with_retry_after() {
        let (lab, s) = state().await;
        lab.set_down(true);
        let resp = app(s)
            .oneshot(
                HttpRequest::get("/p")
                    .header("X-API-Key", "sk_live_x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    }

    #[tokio::test]
    async fn lab_rejections_are_503_never_a_pass() {
        // 401 = the Lab rejects our service token; 400 = any other 4xx.
        for forced in [401, 400] {
            let (lab, s) = state().await;
            lab.set_credential("api_key", "sk_live_abc", FakeLab::identity(ACCT, "pro", 10));
            lab.state
                .status_override
                .store(forced, std::sync::atomic::Ordering::SeqCst);
            let resp = app(s)
                .oneshot(
                    HttpRequest::get("/p")
                        .header("X-API-Key", "sk_live_abc")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 503, "Lab {forced}");
            assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
            let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("auth_service_unavailable"));
        }
    }

    #[tokio::test]
    async fn service_token_uses_the_account_lookup() {
        let (lab, s) = state().await;
        lab.set_account(
            ACCT,
            json!({"active": true, "account_id": ACCT, "tier": "pro", "credits": {"balance": 5}}),
        );
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {SERVICE}"))
            .header(SERVICE_ACCOUNT_HEADER, ACCT)
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(app(s.clone()), r).await, (200, format!("{ACCT}|pro|")));
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {SERVICE}"))
            .header(SERVICE_ACCOUNT_HEADER, uuid::Uuid::new_v4().to_string())
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app(s), r).await;
        assert_eq!(status, 401);
        assert!(body.contains("invalid_service_call"));
    }

    #[tokio::test]
    async fn service_token_without_or_with_bad_account_header_is_401() {
        let (_lab, s) = state().await;
        for header in [None, Some("not-a-uuid")] {
            let mut r = HttpRequest::get("/p").header("Authorization", format!("Bearer {SERVICE}"));
            if let Some(h) = header {
                r = r.header(SERVICE_ACCOUNT_HEADER, h);
            }
            let (status, body) = call(app(s.clone()), r.body(Body::empty()).unwrap()).await;
            assert_eq!(status, 401);
            assert!(body.contains("invalid_service_call"));
        }
    }
}
