//! HTTP routes and which auth layer guards them.

use std::sync::Arc;

use axum::{
    middleware,
    routing::{delete, get, post},
    Router,
};
use tower_http::compression::{
    predicate::{NotForContentType, Predicate, SizeAbove},
    CompressionLayer,
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

/// Smallest response body worth compressing.
const COMPRESSION_MIN_BYTES: u16 = 1024;

/// Whether a response may be compressed: a body of at least
/// [`COMPRESSION_MIN_BYTES`] that is not a stream of events (SSE must
/// reach the client as each event is written), a WebSocket upgrade, an
/// image or gRPC.
fn compressible() -> impl Predicate {
    SizeAbove::new(COMPRESSION_MIN_BYTES)
        .and(NotForContentType::SSE)
        .and(NotForContentType::IMAGES)
        .and(NotForContentType::GRPC)
        .and(
            |status: axum::http::StatusCode,
             _: axum::http::Version,
             _: &axum::http::HeaderMap,
             _: &axum::http::Extensions| {
                status != axum::http::StatusCode::SWITCHING_PROTOCOLS
            },
        )
}

/// gzip for the responses [`compressible`] allows, when the client's
/// `Accept-Encoding` asks for it. Scrape and results payloads are text
/// (markdown, HTML, JSON) and shrink several times over.
fn compression_layer() -> CompressionLayer<impl Predicate> {
    CompressionLayer::new().compress_when(compressible())
}

/// Apply `auth` to `routes`. `ws` routes additionally accept `?token=` (in
/// both modes) when no `Authorization`/`X-API-Key` header is sent.
///
/// `route_layer` keeps the guard off the 404 fallback, so unknown paths
/// return 404 instead of a misleading 401.
///
/// Hosted, the guard also enforces the team role: a viewer is refused every
/// method that is not a read (`auth::middleware::admit`), so a route added
/// to a guarded router needs nothing more.
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

/// Every engine route with its auth guard, the request trace layer, the
/// body-size limits and response compression. CORS is added by the caller.
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
        .route("/crawl/bulk", post(create_crawl_bulk))
        .route("/crawl/suggest", post(suggest::create_suggest))
        .route("/crawl/suggest/{id}", get(suggest::get_suggest));

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
        // Outermost, so it covers every route merged above.
        .layer(compression_layer())
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
            crate::lab_client::testing::INSTANCE_ID,
            crate::lab_client::testing::SECRET,
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

    /// `Content-Encoding` and body of a GET sent with `accept_encoding`.
    async fn encoded(uri: &str, accept_encoding: Option<&str>) -> (Option<String>, Vec<u8>) {
        let mut req = Request::get(uri);
        if let Some(value) = accept_encoding {
            req = req.header("Accept-Encoding", value);
        }
        let res = app(admin())
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{uri}");
        let encoding = res
            .headers()
            .get("content-encoding")
            .map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(res.into_body(), 1 << 24)
            .await
            .unwrap();
        (encoding, body.to_vec())
    }

    #[tokio::test]
    async fn large_responses_are_gzipped_when_the_client_accepts_it() {
        // The OpenAPI spec: a large JSON body.
        let (encoding, plain) = encoded("/openapi.json", None).await;
        assert_eq!(encoding, None);
        assert!(plain.len() > usize::from(COMPRESSION_MIN_BYTES));
        serde_json::from_slice::<serde_json::Value>(&plain).unwrap();

        let (encoding, gzipped) = encoded("/openapi.json", Some("gzip")).await;
        assert_eq!(encoding.as_deref(), Some("gzip"));
        assert_eq!(&gzipped[..2], &[0x1f, 0x8b], "gzip magic bytes");
        assert!(gzipped.len() < plain.len() / 2);

        // An encoding the server does not produce: plain.
        let (encoding, body) = encoded("/openapi.json", Some("zstd")).await;
        assert_eq!(encoding, None);
        assert_eq!(body, plain);
    }

    #[tokio::test]
    async fn small_responses_are_not_compressed() {
        let (encoding, body) = encoded("/health", Some("gzip")).await;
        assert_eq!(encoding, None);
        assert!(body.len() < usize::from(COMPRESSION_MIN_BYTES));
        serde_json::from_slice::<serde_json::Value>(&body).unwrap();
    }

    #[tokio::test]
    async fn metrics_stay_readable_with_and_without_gzip() {
        let (encoding, plain) = encoded("/metrics", None).await;
        assert_eq!(encoding, None);
        std::str::from_utf8(&plain).unwrap();
        let (encoding, body) = encoded("/metrics", Some("gzip")).await;
        match encoding.as_deref() {
            Some("gzip") => assert_eq!(&body[..2], &[0x1f, 0x8b]),
            None => assert!(body.len() < usize::from(COMPRESSION_MIN_BYTES)),
            other => panic!("unexpected Content-Encoding {other:?}"),
        }
    }

    #[test]
    fn event_streams_and_upgrades_are_never_compressed() {
        use axum::http::{Response, StatusCode};
        let big = || Body::from(vec![b'x'; 4096]);
        let predicate = compressible();
        let json = Response::builder()
            .header("content-type", "application/json")
            .body(big())
            .unwrap();
        assert!(predicate.should_compress(&json));
        let sse = Response::builder()
            .header("content-type", "text/event-stream")
            .body(big())
            .unwrap();
        assert!(!predicate.should_compress(&sse));
        let upgrade = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .body(big())
            .unwrap();
        assert!(!predicate.should_compress(&upgrade));
        let small = Response::builder()
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert!(!predicate.should_compress(&small));
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

    /// The team of every credential in the role tests.
    const TEAM: &str = "11111111-1111-1111-1111-111111111111";
    const SERVICE: &str = "0123456789abcdef0123456789abcdef";
    /// A running job of [`TEAM`], present in every role-test app.
    const JOB: &str = "job-of-the-team";

    /// Every write or billed route: (method, uri, content type, a body the
    /// route accepts).
    fn write_routes() -> Vec<(&'static str, String, &'static str, String)> {
        let crawl = r#"{"start_urls":["https://example.invalid/"],"index_uid":"t"}"#;
        let json = "application/json";
        vec![
            ("POST", "/scrape".into(), json, r#"{"url":"https://example.invalid/"}"#.into()),
            ("POST", "/batch/scrape".into(), json, r#"{"urls":["https://example.invalid/"]}"#.into()),
            ("POST", "/extract".into(), json, r#"{"urls":["https://example.invalid/"],"prompt":"x"}"#.into()),
            ("POST", "/map".into(), json, r#"{"url":"https://example.invalid/"}"#.into()),
            ("POST", "/search".into(), json, r#"{"url":"https://example.invalid/","q":"x"}"#.into()),
            ("POST", "/crawl".into(), json, crawl.into()),
            ("POST", "/crawl/sync".into(), json, crawl.into()),
            ("POST", "/crawl/bulk".into(), json, format!("[{crawl}]")),
            ("POST", "/crawl/suggest".into(), json, r#"{"url":"https://example.invalid/"}"#.into()),
            (
                "POST",
                "/parse".into(),
                "multipart/form-data; boundary=b",
                "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b--\r\n".into(),
            ),
            ("DELETE", format!("/job/{JOB}"), json, String::new()),
            ("DELETE", format!("/job/{JOB}?purge=true"), json, String::new()),
            ("POST", format!("/job/{JOB}/pause"), json, String::new()),
            ("POST", format!("/job/{JOB}/resume"), json, String::new()),
        ]
    }

    /// Every read route a team member reaches (GET only).
    fn read_routes() -> Vec<String> {
        vec![
            "/jobs".into(),
            format!("/job/{JOB}/status"),
            format!("/job/{JOB}/results"),
            format!("/job/{JOB}/events/history"),
            "/extract/nope".into(),
            "/crawl/suggest/nope".into(),
            "/stats".into(),
            "/errors".into(),
            "/domains".into(),
            "/analytics/v0/pipes".into(),
            "/health".into(),
            "/openapi.json".into(),
        ]
    }

    /// A hosted app over a fake Lab that knows one credential per role, with
    /// a running job of [`TEAM`].
    async fn hosted_with_roles() -> (crate::lab_client::testing::FakeLab, Router) {
        use crate::lab_client::testing::{FakeLab, INSTANCE_ID, SECRET};
        let lab = FakeLab::start().await;
        let with_role = |role: serde_json::Value| {
            let mut v = FakeLab::identity(TEAM, "pro", 100);
            v["role"] = role;
            v
        };
        for role in ["viewer", "member", "admin", "owner", "auditor"] {
            lab.set_credential("session", &format!("s_{role}"), with_role(role.into()));
            lab.set_credential("bearer", &format!("o_{role}"), with_role(role.into()));
        }
        lab.set_credential("session", "s_null", with_role(serde_json::Value::Null));
        lab.set_credential(
            "api_key",
            "sk_live_team",
            with_role(serde_json::Value::Null),
        );
        lab.set_credential("bearer", "sk_live_team", with_role(serde_json::Value::Null));
        lab.set_account(
            TEAM,
            serde_json::json!({"active": true, "account_id": TEAM, "tier": "pro",
                               "credits": {"balance": 100}}),
        );
        let client = std::sync::Arc::new(crate::lab_client::LabClient::new(
            &lab.url,
            INSTANCE_ID,
            SECRET,
        ));
        let auth = AuthMode::Saas(std::sync::Arc::new(crate::auth::AuthState::new(
            client,
            Some(SERVICE.into()),
        )));
        let bus = scrapix_queue::ChannelBus::new();
        let state = crate::results::test_support::test_state(&bus);
        let mut job = scrapix_core::JobState::with_account(JOB, "idx", TEAM);
        job.start();
        state.insert_job(job);
        (lab, build_router(state, &auth))
    }

    /// The headers that present `who`'s credential.
    fn credential(who: &str) -> Vec<(&'static str, String)> {
        match who {
            "api_key" => vec![("X-API-Key", "sk_live_team".into())],
            "api_key_bearer" => vec![("Authorization", "Bearer sk_live_team".into())],
            "service" => vec![
                ("Authorization", format!("Bearer {SERVICE}")),
                (auth::middleware::SERVICE_ACCOUNT_HEADER, TEAM.into()),
            ],
            s if s.starts_with("s_") => vec![
                ("Cookie", format!("scrapix_session={s}")),
                ("X-Account-Id", TEAM.into()),
            ],
            o => vec![("Authorization", format!("Bearer {o}"))],
        }
    }

    /// Status and `code` of `method uri` sent by `who`.
    async fn send_as(
        app: Router,
        who: &str,
        method: &str,
        uri: &str,
        content_type: &str,
        body: String,
    ) -> (u16, String) {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in credential(who) {
            req = req.header(name, value);
        }
        if !body.is_empty() {
            req = req.header("Content-Type", content_type);
        }
        let res = app
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 24)
            .await
            .unwrap();
        let code = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v["code"].as_str().map(str::to_string))
            .unwrap_or_default();
        (status, code)
    }

    /// A team viewer (session or OAuth connection) only reads: every write
    /// or billed route is a 403 `forbidden`, before any of its work.
    #[tokio::test]
    async fn hosted_viewer_is_refused_every_write_and_billed_route() {
        let mut let_through = Vec::new();
        for who in ["s_viewer", "o_viewer"] {
            for (method, uri, content_type, body) in write_routes() {
                let (_lab, app) = hosted_with_roles().await;
                let got = send_as(app, who, method, &uri, content_type, body).await;
                if got != (403, "forbidden".to_string()) {
                    let_through.push(format!("{who} {method} {uri} -> {got:?}"));
                }
            }
        }
        assert!(
            let_through.is_empty(),
            "a viewer got past:\n{}",
            let_through.join("\n")
        );
    }

    /// A viewer still reads everything a member reads.
    #[tokio::test]
    async fn hosted_viewer_still_reads() {
        for uri in read_routes() {
            for who in ["s_viewer", "o_viewer"] {
                let (_lab, app) = hosted_with_roles().await;
                let viewer = send_as(app, who, "GET", &uri, "", String::new()).await;
                let (_lab, app) = hosted_with_roles().await;
                let member = send_as(app, "s_member", "GET", &uri, "", String::new()).await;
                assert_ne!(viewer.0, 403, "{who} GET {uri}");
                assert_eq!(viewer, member, "{who} GET {uri}");
            }
        }
        let (_lab, app) = hosted_with_roles().await;
        assert_eq!(
            send_as(app, "s_viewer", "GET", "/jobs", "", String::new())
                .await
                .0,
            200
        );
    }

    /// Every other caller keeps today's behaviour: no role (API key, service
    /// call, a Lab that sends none), member, admin, owner, and a role the
    /// engine does not know (the Lab may add roles) are never refused for
    /// their role.
    #[tokio::test]
    async fn hosted_non_viewers_are_never_refused_for_their_role() {
        let callers = [
            "api_key",
            "api_key_bearer",
            "service",
            "s_null",
            "s_member",
            "s_admin",
            "s_owner",
            "s_auditor",
            "o_member",
            "o_admin",
            "o_owner",
            "o_auditor",
        ];
        for who in callers {
            // Bodies the routes refuse (no content type), so no work starts.
            for (method, uri, _, _) in write_routes() {
                let (_lab, app) = hosted_with_roles().await;
                let got = send_as(app, who, method, &uri, "", String::new()).await;
                assert_ne!(got.1, "forbidden", "{who} {method} {uri}: {got:?}");
                assert_ne!(got.0, 401, "{who} {method} {uri}: {got:?}");
            }
            let (_lab, app) = hosted_with_roles().await;
            assert_eq!(
                send_as(
                    app,
                    who,
                    "POST",
                    &format!("/job/{JOB}/pause"),
                    "",
                    String::new()
                )
                .await,
                (200, String::new()),
                "{who} pauses the team's job"
            );
            let (_lab, app) = hosted_with_roles().await;
            assert_eq!(
                send_as(app, who, "GET", "/jobs", "", String::new()).await.0,
                200,
                "{who}"
            );
        }
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
