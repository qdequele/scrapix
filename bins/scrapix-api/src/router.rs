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
use crate::settings::Mode;
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

/// Apply `auth` to `routes`. `ws` routes additionally accept `?token=` in standalone.
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
        AuthMode::Saas(state) => routes.route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::validate_api_key_or_session,
        )),
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
pub(crate) fn build_router(state: Arc<AppState>, auth: &AuthMode, mode: Mode) -> Router {
    // Public routes (no auth required)
    let public = Router::new()
        .route("/health", get(health))
        .route("/health/services", get(health_services))
        .route("/metrics", get(metrics));

    let diagnostics = Router::new()
        .route("/stats", get(handle_stats))
        .route("/errors", get(handle_errors))
        .route("/domains", get(handle_domains));
    // Standalone: the operator's data. Hosted: unchanged (public) for now.
    let diagnostics = match mode {
        Mode::Standalone => guard(diagnostics, auth, false),
        Mode::Hosted => diagnostics,
    };

    // Guarded in every mode; standalone also accepts `?token=` (browsers
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
    // Stripe, analytics pipes, OAuth provider, /mcp) is served by the Rails
    // app (saas/, SCR-85); the engine keeps only the crawl data plane.
    let protected = guard(product.merge(management), auth, false);

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
    use crate::settings::Mode;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const KEY: &str = "0123456789abcdef";

    fn app(auth: AuthMode, mode: Mode) -> Router {
        let bus = scrapix_queue::ChannelBus::new();
        build_router(crate::results::test_support::test_state(&bus), &auth, mode)
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
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://x@127.0.0.1:1/x")
            .unwrap();
        AuthMode::Saas(std::sync::Arc::new(crate::auth::AuthState {
            pool,
            jwt_secret: "s".into(),
        }))
    }

    const THREE_MB: usize = 3 * 1024 * 1024;

    #[tokio::test]
    async fn body_limit_is_2mb_except_parse() {
        // The admin key is sent so only the body limit can reject.
        let parse = post_status(
            app(admin(), Mode::Standalone),
            "/parse",
            Some(KEY),
            THREE_MB,
        )
        .await;
        assert_ne!(parse, 413, "/parse has its own, larger cap");
        assert_ne!(parse, 401);
        for uri in ["/scrape", "/openapi.json"] {
            assert_eq!(
                post_status(app(admin(), Mode::Standalone), uri, Some(KEY), THREE_MB).await,
                413,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn standalone_docs_are_public() {
        for uri in ["/openapi.json", "/docs"] {
            let s = status(app(admin(), Mode::Standalone), uri, None).await;
            assert_ne!(s, 401, "{uri}");
            assert_ne!(s, 404, "{uri}");
        }
    }

    #[tokio::test]
    async fn standalone_upload_and_scrape_require_key() {
        for uri in ["/parse", "/scrape"] {
            assert_eq!(
                post_status(app(admin(), Mode::Standalone), uri, None, 2).await,
                401,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn standalone_unknown_path_is_404() {
        assert_eq!(
            status(app(admin(), Mode::Standalone), "/nope", None).await,
            404
        );
    }

    #[tokio::test]
    async fn hosted_ws_job_requires_auth() {
        assert_eq!(
            status(app(saas(), Mode::Hosted), "/ws/job/x", None).await,
            401
        );
    }

    #[tokio::test]
    async fn standalone_public_routes_stay_open() {
        assert_eq!(
            status(app(admin(), Mode::Standalone), "/health", None).await,
            200
        );
        assert_eq!(
            status(app(admin(), Mode::Standalone), "/metrics", None).await,
            200
        );
    }

    #[tokio::test]
    async fn standalone_protects_product_diagnostics_and_ws() {
        for uri in ["/jobs", "/stats", "/errors", "/domains", "/ws", "/ws/job/x"] {
            assert_eq!(
                status(app(admin(), Mode::Standalone), uri, None).await,
                401,
                "{uri}"
            );
        }
        assert_eq!(
            status(app(admin(), Mode::Standalone), "/jobs", Some(KEY)).await,
            200
        );
        assert_eq!(
            status(app(admin(), Mode::Standalone), "/stats", Some(KEY)).await,
            200
        );
    }

    #[tokio::test]
    async fn ws_accepts_query_token() {
        // Not a real upgrade request: auth passes, then the WS extractor rejects
        // the plain GET (4xx other than 401).
        let s = status(
            app(admin(), Mode::Standalone),
            &format!("/ws?token={KEY}"),
            None,
        )
        .await;
        assert_ne!(s, 401);
    }

    #[tokio::test]
    async fn hosted_ws_requires_auth_but_diagnostics_stay_public() {
        assert_eq!(status(app(saas(), Mode::Hosted), "/ws", None).await, 401);
        assert_eq!(status(app(saas(), Mode::Hosted), "/stats", None).await, 200);
    }

    #[tokio::test]
    async fn disabled_auth_leaves_everything_open() {
        assert_eq!(
            status(app(AuthMode::Disabled, Mode::Standalone), "/jobs", None).await,
            200
        );
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
