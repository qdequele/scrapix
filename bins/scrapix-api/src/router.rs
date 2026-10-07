//! HTTP routes and which auth layer guards them.

use std::sync::Arc;

use axum::{
    middleware,
    routing::{delete, get, post},
    Router,
};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::auth::{self, AuthMode};
use crate::*; // handlers: health, metrics, handle_stats, ..., batch, extract, results, documents

/// Path plus query with any `token` value replaced, for logs.
///
/// Keys are compared percent-decoded, like `admin_key::query_token` reads
/// them, so `?%74oken=<key>` is redacted too.
pub(crate) fn redact_query(uri: &axum::http::Uri) -> String {
    let Some(q) = uri.query() else {
        return uri.path().to_string();
    };
    let redacted: Vec<String> = q
        .split('&')
        .map(|pair| {
            let is_token = url::form_urlencoded::parse(pair.as_bytes())
                .next()
                .is_some_and(|(k, _)| k == "token");
            if is_token {
                "token=[REDACTED]".to_string()
            } else {
                pair.to_string()
            }
        })
        .collect();
    format!("{}?{}", uri.path(), redacted.join("&"))
}

/// `TraceLayer::new_for_http()` whose span logs the redacted URI.
fn trace_layer() -> TraceLayer<
    tower_http::classify::SharedClassifier<tower_http::classify::ServerErrorsAsFailures>,
    impl Fn(&axum::http::Request<axum::body::Body>) -> tracing::Span + Clone,
> {
    TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<axum::body::Body>| {
        tracing::info_span!("request", method = %req.method(), uri = %redact_query(req.uri()))
    })
}

/// Apply `auth` to `routes`. `ws` routes additionally accept `?token=` (in
/// both modes) when no `Authorization`/`X-API-Key` header is sent.
///
/// `route_layer` keeps the guard off the 404 fallback, so unknown paths
/// return 404 instead of a misleading 401.
fn guard(routes: Router<Arc<AppState>>, auth: &AuthMode, ws: bool) -> Router<Arc<AppState>> {
    match auth {
        AuthMode::Disabled => routes,
        AuthMode::AdminKey(key) if ws => routes.route_layer(middleware::from_fn_with_state(
            key.clone(),
            auth::admin_key::validate_admin_key_ws,
        )),
        AuthMode::AdminKey(key) => routes.route_layer(middleware::from_fn_with_state(
            key.clone(),
            auth::admin_key::validate_admin_key,
        )),
        AuthMode::Saas(state) => {
            let routes = routes.route_layer(middleware::from_fn_with_state(
                state.clone(),
                auth::validate_api_key_or_session,
            ));
            if ws {
                // Added last, so it runs first: `?token=` → `X-API-Key`.
                routes.route_layer(middleware::from_fn(auth::ws_query_token_as_api_key))
            } else {
                routes
            }
        }
    }
}

/// OpenAPI spec + Scalar docs UI (public, no trace layer — as before the
/// router extraction).
fn docs_routes() -> Router {
    use utoipa::OpenApi;
    let spec = openapi::ScrapixApi::openapi();
    let spec_json = spec.to_json().expect("OpenAPI JSON serialization");
    let routes = Router::new()
        .route(
            "/openapi.json",
            get(|| async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    spec_json,
                )
            }),
        )
        .route(
            "/docs",
            get(|| async {
                axum::response::Html(
                    r#"<!doctype html>
<html>
<head><title>Scrapix API Reference</title><meta charset="utf-8"/></head>
<body>
<script id="api-reference" data-url="/openapi.json"></script>
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#,
                )
            }),
        );
    info!("OpenAPI spec at /openapi.json, docs UI at /docs");
    routes
}

/// Every engine route with its auth guard, the request trace layer and the
/// body-size limits. CORS is added by the caller.
pub(crate) fn build_router(state: Arc<AppState>, auth: &AuthMode) -> Router {
    // Public routes (no auth required)
    let public = Router::new()
        .route("/health", get(health))
        .route("/health/services", get(health_services))
        .route("/metrics", get(metrics));

    // Guarded in every mode; each caller sees its own account's data
    // (the standalone admin key sees everything).
    let diagnostics = guard(
        Router::new()
            .route("/stats", get(diagnostics::handle_stats))
            .route("/errors", get(diagnostics::handle_errors))
            .route("/domains", get(diagnostics::handle_domains)),
        auth,
        false,
    );

    // Guarded in every mode; both modes also accept `?token=` (browsers
    // can't set headers on an upgrade).
    let ws = guard(
        Router::new()
            .route("/ws", get(ws_handler))
            .route("/ws/job/{id}", get(ws_job_handler)),
        auth,
        true,
    );

    // Product routes — revenue-generating API endpoints
    let product = Router::new()
        .route("/scrape", post(scrape_url))
        .route("/batch/scrape", post(batch::batch_scrape))
        .route("/extract", post(extract::create_extract))
        .route("/extract/{id}", get(extract::get_extract))
        .route("/map", post(map_url))
        .route("/search", post(search_url))
        .route("/crawl", post(create_crawl))
        .route("/crawl/sync", post(create_crawl_sync))
        .route("/crawl/bulk", post(create_crawl_bulk));

    // Management routes — job monitoring and configuration
    let management = Router::new()
        .route("/jobs", get(list_jobs))
        .route("/job/{id}/status", get(job_status))
        .route("/job/{id}/events", get(job_events))
        .route("/job/{id}/events/history", get(get_job_events_history))
        .route("/job/{id}/results", get(results::job_results))
        .route("/job/{id}", delete(cancel_job))
        .route("/job/{id}/pause", post(pause_job))
        .route("/job/{id}/resume", post(resume_job));

    // The SaaS surface (auth, account/team, configs/engines CRUD, billing,
    // Stripe, OAuth provider, /mcp) is served by the Lab's Rails app
    // (meilisearch/lab, SCR-85); the engine keeps the crawl data plane and serves the analytics
    // pipes over its own ClickHouse, scoped per account.
    let protected = guard(
        product
            .merge(management)
            .nest("/analytics/v0", analytics_pipes::router()),
        auth,
        false,
    );

    // Per-account rate limiting on protected routes was removed — pricing is
    // usage-based (credits), not per-request, so there's nothing to gate on the
    // request rate. Brute-force protection on the auth endpoints lives with
    // the auth endpoints themselves, in the Rails app (Rack::Attack).
    let app = Router::new()
        .merge(public)
        .merge(diagnostics)
        .merge(ws)
        .merge(protected)
        .layer(trace_layer())
        .with_state(state.clone())
        .merge(docs_routes());

    // Request body size limit (2 MB default, prevents DoS via large payloads).
    // Wraps every route above, including /openapi.json and /docs.
    let app = app.layer(tower_http::limit::RequestBodyLimitLayer::new(
        2 * 1024 * 1024,
    ));

    // POST /parse (document upload) is merged after the 2 MB layer — layers
    // only wrap routes that already exist — with its own cap
    // (DOCUMENT_MAX_SIZE_MB + multipart overhead) and the same auth.
    let upload_limit = documents::max_document_bytes() as usize + 1024 * 1024;
    let parse = Router::new()
        .route("/parse", post(documents::parse_upload))
        .layer(axum::extract::DefaultBodyLimit::max(upload_limit))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(upload_limit));
    let parse = guard(parse, auth, false);
    app.merge(parse.layer(trace_layer()).with_state(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AdminKey, AuthMode};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const KEY: &str = "0123456789abcdef";

    fn app(auth: AuthMode) -> Router {
        let bus = scrapix_queue::ChannelBus::new();
        build_router(crate::results::test_support::test_state(&bus), &auth)
    }

    async fn status(app: Router, uri: &str, key: Option<&str>) -> u16 {
        let mut req = Request::get(uri);
        if let Some(k) = key {
            req = req.header("X-API-Key", k);
        }
        app.oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// Status and JSON body of an authenticated GET.
    async fn get_json(app: Router, uri: &str, key: &str) -> (u16, serde_json::Value) {
        let req = Request::get(uri).header("X-API-Key", key);
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// POST `len` bytes to `uri` (with an explicit `Content-Length`, as a
    /// real client sends).
    async fn post_status(app: Router, uri: &str, key: Option<&str>, len: usize) -> u16 {
        let mut req = Request::post(uri).header("Content-Length", len);
        if let Some(k) = key {
            req = req.header("X-API-Key", k);
        }
        app.oneshot(req.body(Body::from(vec![b'x'; len])).unwrap())
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    fn admin() -> AuthMode {
        AuthMode::AdminKey(AdminKey::new(KEY.into()))
    }

    fn saas() -> AuthMode {
        // Unreachable Lab (connection refused): any credential that reaches
        // the Lab fails fast with a 503 instead of passing.
        let lab = std::sync::Arc::new(crate::lab_client::LabClient::new(
            "http://127.0.0.1:1",
            "unused-service-token",
        ));
        AuthMode::Saas(std::sync::Arc::new(crate::auth::AuthState::new(lab, None)))
    }

    const THREE_MB: usize = 3 * 1024 * 1024;

    #[tokio::test]
    async fn body_limit_is_2mb_except_parse() {
        // The admin key is sent so only the body limit can reject.
        let parse = post_status(app(admin()), "/parse", Some(KEY), THREE_MB).await;
        assert_ne!(parse, 413, "/parse has its own, larger cap");
        assert_ne!(parse, 401);
        for uri in ["/scrape", "/openapi.json"] {
            assert_eq!(
                post_status(app(admin()), uri, Some(KEY), THREE_MB).await,
                413,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn standalone_docs_are_public() {
        for uri in ["/openapi.json", "/docs"] {
            let s = status(app(admin()), uri, None).await;
            assert_ne!(s, 401, "{uri}");
            assert_ne!(s, 404, "{uri}");
        }
    }

    #[tokio::test]
    async fn standalone_upload_and_scrape_require_key() {
        for uri in ["/parse", "/scrape"] {
            assert_eq!(post_status(app(admin()), uri, None, 2).await, 401, "{uri}");
        }
    }

    #[tokio::test]
    async fn standalone_unknown_path_is_404() {
        assert_eq!(status(app(admin()), "/nope", None).await, 404);
    }

    #[tokio::test]
    async fn hosted_ws_job_requires_auth() {
        assert_eq!(status(app(saas()), "/ws/job/x", None).await, 401);
    }

    #[tokio::test]
    async fn standalone_public_routes_stay_open() {
        assert_eq!(status(app(admin()), "/health", None).await, 200);
        assert_eq!(status(app(admin()), "/metrics", None).await, 200);
    }

    #[tokio::test]
    async fn standalone_protects_product_diagnostics_and_ws() {
        for uri in ["/jobs", "/stats", "/errors", "/domains", "/ws", "/ws/job/x"] {
            assert_eq!(status(app(admin()), uri, None).await, 401, "{uri}");
        }
        assert_eq!(status(app(admin()), "/jobs", Some(KEY)).await, 200);
        assert_eq!(status(app(admin()), "/stats", Some(KEY)).await, 200);
    }

    #[tokio::test]
    async fn analytics_pipes_are_protected_and_404_without_clickhouse() {
        for uri in ["/analytics/v0/pipes", "/analytics/v0/pipes/kpis.json"] {
            assert_eq!(status(app(admin()), uri, None).await, 401, "{uri}");
            let (code, body) = get_json(app(admin()), uri, KEY).await;
            assert_eq!(code, 404, "{uri}");
            assert_eq!(body["code"], "analytics_unavailable", "{uri}");
            assert_eq!(body["error"], "Analytics unavailable", "{uri}");
            assert_eq!(status(app(saas()), uri, None).await, 401, "{uri}");
        }
    }

    #[tokio::test]
    async fn ws_accepts_query_token() {
        // Not a real upgrade request: auth passes, then the WS extractor rejects
        // the plain GET (4xx other than 401).
        let s = status(app(admin()), &format!("/ws?token={KEY}"), None).await;
        assert_ne!(s, 401);
    }

    #[tokio::test]
    async fn hosted_ws_and_diagnostics_require_auth() {
        for uri in ["/ws", "/stats", "/errors", "/domains"] {
            assert_eq!(status(app(saas()), uri, None).await, 401, "{uri}");
        }
        for uri in ["/health", "/health/services", "/metrics"] {
            assert_eq!(status(app(saas()), uri, None).await, 200, "{uri}");
        }
    }

    /// Status and body `error` of a rejected request to `uri` with `headers`.
    async fn rejection(app: Router, uri: &str, headers: &[(&str, &str)]) -> (u16, String) {
        let mut req = Request::get(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (
            status,
            json["error"].as_str().unwrap_or_default().to_string(),
        )
    }

    /// The 401 body's `error` for a request to `uri` with `headers`.
    async fn auth_error(app: Router, uri: &str, headers: &[(&str, &str)]) -> String {
        let (status, error) = rejection(app, uri, headers).await;
        assert_eq!(status, 401, "{uri}");
        error
    }

    /// What an unreachable Lab answers for a credential that reached it.
    fn lab_unavailable() -> (u16, String) {
        (503, "Authentication service unavailable".to_string())
    }

    #[tokio::test]
    async fn hosted_ws_reads_query_token_as_api_key() {
        // A malformed key is rejected on format before any Lab lookup, so the
        // error proves `?token=` reached the SaaS middleware as an API key
        // (without it the request is "Missing API key or session").
        for uri in ["/ws?token=not-a-key", "/ws/job/x?token=not-a-key"] {
            assert_eq!(
                auth_error(app(saas()), uri, &[]).await,
                "Invalid API key format",
                "{uri}"
            );
        }
        assert_eq!(
            auth_error(app(saas()), "/ws", &[]).await,
            "Missing API key or session"
        );
        // Percent-decoded, like standalone's `?token=`: `%73k_live_...` is
        // `sk_live_...`, which passes the format check and reaches the
        // (unreachable) Lab.
        assert_eq!(
            rejection(app(saas()), "/ws?token=%73k_live_x", &[]).await,
            lab_unavailable()
        );
    }

    #[tokio::test]
    async fn hosted_ws_header_wins_over_query_token() {
        // X-API-Key present: its (malformed) value is the one validated,
        // not the well-formed `?token=`.
        assert_eq!(
            auth_error(
                app(saas()),
                "/ws?token=sk_live_x",
                &[("X-API-Key", "not-a-key")]
            )
            .await,
            "Invalid API key format"
        );
        // Authorization present: the Bearer token is sent to the
        // (unreachable) Lab; `?token=` (malformed, a local 401) is ignored.
        assert_eq!(
            rejection(
                app(saas()),
                "/ws?token=not-a-key",
                &[("Authorization", "Bearer abc")]
            )
            .await,
            lab_unavailable()
        );
    }

    #[tokio::test]
    async fn hosted_query_token_is_ws_only() {
        assert_eq!(
            auth_error(app(saas()), "/jobs?token=not-a-key", &[]).await,
            "Missing API key or session"
        );
    }

    #[tokio::test]
    async fn disabled_auth_leaves_everything_open() {
        assert_eq!(status(app(AuthMode::Disabled), "/jobs", None).await, 200);
    }

    #[test]
    fn redacts_token_query() {
        let uri: axum::http::Uri = format!("/ws?a=1&token={KEY}&b=2").parse().unwrap();
        let out = redact_query(&uri);
        assert!(!out.contains(KEY), "{out}");
        assert!(out.contains("token=[REDACTED]"), "{out}");
        assert!(out.contains("a=1") && out.contains("b=2"), "{out}");
        // Percent-encoded key: `query_token` decodes it, so it must be redacted.
        let encoded: axum::http::Uri = format!("/ws?%74oken={KEY}").parse().unwrap();
        let out = redact_query(&encoded);
        assert!(!out.contains(KEY), "{out}");
        let plain: axum::http::Uri = "/jobs".parse().unwrap();
        assert_eq!(redact_query(&plain), "/jobs");
    }
}
