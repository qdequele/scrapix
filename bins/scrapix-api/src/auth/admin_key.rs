//! Standalone-mode authentication: one operator key from `SCRAPIX_ADMIN_KEY`.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

use super::middleware::AuthError;

#[derive(Clone)]
pub struct AdminKey(Arc<str>);

impl AdminKey {
    pub fn new(key: String) -> Self {
        Self(Arc::from(key))
    }

    /// Constant-time for equal lengths; a length mismatch returns early,
    /// which only reveals the key's length.
    pub fn matches(&self, candidate: &str) -> bool {
        let a = self.0.as_bytes();
        let b = candidate.as_bytes();
        a.len() == b.len() && bool::from(a.ct_eq(b))
    }
}

fn header_key(request: &Request) -> Option<&str> {
    let headers = request.headers();
    if let Some(b) = headers
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        return Some(b.trim());
    }
    headers
        .get("X-API-Key")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
}

/// The `token` query parameter, percent-decoded.
pub(crate) fn query_token(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
}

fn unauthorized() -> AuthError {
    AuthError::new("Missing or invalid admin key", "unauthorized")
}

pub(crate) async fn validate_admin_key(
    State(key): State<AdminKey>,
    request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    match header_key(&request) {
        Some(k) if key.matches(k) => Ok(next.run(request).await),
        _ => Err(unauthorized()),
    }
}

/// WebSocket variant: browsers can't set headers on an upgrade, so the key
/// may also come as `?token=`.
pub(crate) async fn validate_admin_key_ws(
    State(key): State<AdminKey>,
    request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    let ok = match header_key(&request) {
        Some(k) => key.matches(k),
        None => query_token(request.uri().query()).is_some_and(|t| key.matches(&t)),
    };
    if ok {
        Ok(next.run(request).await)
    } else {
        Err(unauthorized())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest, middleware, routing::get, Router};
    use tower::ServiceExt;

    const KEY: &str = "0123456789abcdef";

    fn app(ws: bool) -> Router {
        let key = AdminKey::new(KEY.into());
        // The two `from_fn_with_state` calls produce distinct concrete
        // `FromFnLayer` types (one per handler fn item), so the layer can't
        // be built once and branched on; build the whole router per branch.
        if ws {
            Router::new()
                .route("/p", get(|| async { "ok" }))
                .route_layer(middleware::from_fn_with_state(key, validate_admin_key_ws))
        } else {
            Router::new()
                .route("/p", get(|| async { "ok" }))
                .route_layer(middleware::from_fn_with_state(key, validate_admin_key))
        }
    }

    async fn status(app: Router, req: HttpRequest<Body>) -> u16 {
        app.oneshot(req).await.unwrap().status().as_u16()
    }

    #[tokio::test]
    async fn missing_key_is_401() {
        let r = HttpRequest::get("/p").body(Body::empty()).unwrap();
        assert_eq!(status(app(false), r).await, 401);
    }

    #[tokio::test]
    async fn wrong_key_is_401() {
        let r = HttpRequest::get("/p")
            .header("X-API-Key", "nope-nope-nope-nope")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(app(false), r).await, 401);
    }

    #[tokio::test]
    async fn bearer_and_x_api_key_both_work() {
        let r = HttpRequest::get("/p")
            .header("Authorization", format!("Bearer {KEY}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(app(false), r).await, 200);
        let r = HttpRequest::get("/p")
            .header("X-API-Key", KEY)
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(app(false), r).await, 200);
    }

    #[tokio::test]
    async fn query_token_only_on_ws_routes() {
        let r = HttpRequest::get(format!("/p?token={KEY}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(app(false), r).await, 401);
        let r = HttpRequest::get(format!("/p?x=1&token={KEY}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(app(true), r).await, 200);
    }

    #[tokio::test]
    async fn ws_missing_key_is_401() {
        let r = HttpRequest::get("/p").body(Body::empty()).unwrap();
        assert_eq!(status(app(true), r).await, 401);
    }

    #[tokio::test]
    async fn ws_wrong_query_token_is_401() {
        let r = HttpRequest::get("/p?token=wrong-wrong-wrong-")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(app(true), r).await, 401);
    }

    #[tokio::test]
    async fn ws_wrong_header_does_not_fall_back_to_correct_query_token() {
        let r = HttpRequest::get(format!("/p?token={KEY}"))
            .header("X-API-Key", "nope-nope-nope-nope")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(app(true), r).await, 401);
    }

    #[test]
    fn matches_rejects_prefixes_and_different_lengths() {
        let k = AdminKey::new(KEY.into());
        assert!(k.matches(KEY));
        assert!(!k.matches(&KEY[..15]));
        assert!(!k.matches(&format!("{KEY}x")));
        assert!(!k.matches(""));
    }

    #[test]
    fn query_token_parses_percent_encoding() {
        assert_eq!(
            query_token(Some("a=1&token=ab%2Bc")).as_deref(),
            Some("ab+c")
        );
        assert_eq!(query_token(Some("a=1")), None);
        assert_eq!(query_token(None), None);
    }
}
