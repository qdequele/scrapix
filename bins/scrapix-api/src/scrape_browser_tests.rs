//! End-to-end `/scrape` pipeline tests (`perform_scrape`) with a real
//! browser, against local fixture pages.
//!
//! Needs Chrome/Chromium: set `SCRAPIX_TEST_CHROME` to the executable path
//! (or `1` to auto-detect). Without it each test prints a skip note and
//! passes, like the crawler's `cdp_browser` tests:
//!
//! ```bash
//! SCRAPIX_TEST_CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
//!   cargo test -p scrapix-api scrape_browser_tests
//! ```
//!
//! Fixtures are served by wiremock on 127.0.0.1 and addressed as
//! `localhost`; the test state's fetcher and renderer allow private IPs.

use super::*;
use scrapix_queue::ChannelBus;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// `(renderer, fetcher)` for tests: both allow private IPs so `localhost`
/// fixtures are reachable.
fn test_fetcher() -> Arc<HttpFetcher> {
    let robots = Arc::new(
        RobotsCache::new(RobotsConfig {
            respect_robots: false,
            ..Default::default()
        })
        .unwrap(),
    );
    Arc::new(
        HttpFetcherBuilder::new()
            .allow_private_ips(true)
            .max_retries(0)
            .build(robots)
            .unwrap(),
    )
}

/// A DB-less state with (when `SCRAPIX_TEST_CHROME` is set) a browser.
/// Returns `None` to skip when no browser is configured.
async fn browser_state() -> Option<Arc<AppState>> {
    let chrome = std::env::var("SCRAPIX_TEST_CHROME")
        .ok()
        .filter(|s| !s.is_empty());
    let Some(chrome) = chrome else {
        eprintln!("SCRAPIX_TEST_CHROME not set; skipping browser /scrape test");
        return None;
    };
    let mut builder = CdpRendererBuilder::new()
        .headless(true)
        .allow_private_ips(true)
        .viewport(1280, 800)
        .timeout(Duration::from_secs(20));
    if chrome != "1" {
        builder = builder.executable_path(chrome);
    }
    let renderer = Arc::new(builder.build().await.expect("launch test browser"));
    Some(Arc::new(state_with(Some(renderer))))
}

fn state_with(renderer: Option<Arc<CdpRenderer>>) -> AppState {
    let bus = ChannelBus::new();
    AppState::new(
        AnyProducer::channel(bus.producer()),
        AppConfig {
            max_jobs: 10,
            job_stall_timeout: Duration::from_secs(1800),
            completion_grace: Duration::from_secs(3),
            resume_heal_after: Duration::from_secs(60),
            max_pending_acks: 1000,
        },
        None,
        None,
        None,
        None,
        test_fetcher(),
        renderer,
        None,
        None,
        None,
        None,
        None,
        webhooks::WebhookDispatcher::new(
            scrapix_crawler::safe_client_builder(None, true)
                .build()
                .unwrap(),
            webhooks::DEFAULT_MAX_CONCURRENT_DELIVERIES,
        ),
    )
}

/// Serve `html` at `route`; returns the page URL on `localhost`.
async fn serve(server: &MockServer, route: &str, html: &str) -> String {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(html.to_string(), "text/html; charset=utf-8"),
        )
        .mount(server)
        .await;
    format!("{}{route}", server.uri().replace("127.0.0.1", "localhost"))
}

fn request(json: serde_json::Value) -> ScrapeRequest {
    serde_json::from_value(json).expect("valid ScrapeRequest")
}

const PAGE: &str = r#"<!doctype html><html><head><title>Fixture</title></head>
<body style="margin:0"><h1>Hello fixture</h1><div style="height:2400px"></div></body></html>"#;

/// SCR-69: `formats: ["screenshot"]` without `render_js` switches to the
/// browser and returns a decodable PNG; `full_page: false` captures only
/// the viewport.
#[tokio::test]
async fn screenshot_format_returns_png_and_forces_browser() {
    let Some(state) = browser_state().await else {
        return;
    };
    let server = MockServer::start().await;
    let url = serve(&server, "/page", PAGE).await;

    let resp = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": url,
            "formats": ["screenshot", "markdown"],
        })),
    )
    .await
    .expect("scrape succeeds");
    assert!(resp.success);
    assert!(resp.markdown.unwrap().contains("Hello fixture"));
    let png = BASE64
        .decode(resp.screenshot.expect("screenshot returned"))
        .expect("base64");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    assert!(height > 2000, "full page by default, got height {height}");

    let resp = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": url,
            "formats": ["screenshot"],
            "screenshot": {"full_page": false},
        })),
    )
    .await
    .unwrap();
    let png = BASE64.decode(resp.screenshot.unwrap()).unwrap();
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
    assert_eq!(height, 800, "viewport only");
    // Only the requested formats come back.
    assert!(resp.markdown.is_none());
}

/// Without a browser, the screenshot format fails clearly instead of being
/// silently ignored (no Chrome needed for this one).
#[tokio::test]
async fn screenshot_without_browser_is_render_js_unavailable() {
    let state = Arc::new(state_with(None));
    let err = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": "https://example.com/",
            "formats": ["screenshot"],
        })),
    )
    .await
    .expect_err("refused without a browser");
    assert_eq!(err.code, "render_js_unavailable");
    assert!(err.error.contains("screenshot"), "{}", err.error);
}

const ACTIONS_PAGE: &str = r#"<!doctype html><html><head><title>Actions</title></head>
<body><button id="more" onclick="document.body.insertAdjacentHTML('beforeend','<p id=added>Added by click</p>')">More</button></body></html>"#;

/// SCR-75: actions force the browser, run before extraction, and their
/// script values come back in `actions.javascript_returns`.
#[tokio::test]
async fn actions_run_before_extraction() {
    let Some(state) = browser_state().await else {
        return;
    };
    let server = MockServer::start().await;
    let url = serve(&server, "/actions", ACTIONS_PAGE).await;

    let resp = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": url,
            "formats": ["markdown"],
            "actions": [
                {"type": "click", "selector": "#more"},
                {"type": "wait", "selector": "#added"},
                {"type": "execute_javascript", "script": "document.querySelectorAll('#added').length"},
                {"type": "execute_javascript", "script": "return {title: document.title}"}
            ]
        })),
    )
    .await
    .expect("scrape succeeds");
    assert!(resp.markdown.unwrap().contains("Added by click"));
    let actions = resp.actions.expect("actions result");
    assert_eq!(
        actions.javascript_returns,
        vec![
            serde_json::json!(1),
            serde_json::json!({"title": "Actions"})
        ]
    );
    let json = serde_json::to_value(ScrapeActionsResult {
        javascript_returns: vec![serde_json::json!(1)],
    })
    .unwrap();
    assert_eq!(json, serde_json::json!({"javascript_returns": [1]}));
}

/// A failing action is a structured `action_error` (422) naming the action,
/// not a generic fetch error or a whole-request timeout.
#[tokio::test]
async fn failing_action_is_action_error() {
    let Some(state) = browser_state().await else {
        return;
    };
    let server = MockServer::start().await;
    let url = serve(&server, "/actions", ACTIONS_PAGE).await;

    let err = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": url,
            "actions": [
                {"type": "wait", "ms": 10},
                {"type": "execute_javascript", "script": "throw new Error('kaboom')"}
            ]
        })),
    )
    .await
    .expect_err("action fails");
    assert_eq!(err.code, "action_error");
    assert!(
        err.error.starts_with("actions[1] (execute_javascript)"),
        "{}",
        err.error
    );
    let details = err.details.clone().expect("details");
    assert_eq!(details["action_index"], 1);
    assert_eq!(details["action_type"], "execute_javascript");
    assert!(details["message"].as_str().unwrap().contains("kaboom"));
    assert_eq!(
        err.into_response().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

/// Invalid or too many actions are rejected before anything is fetched
/// (no browser needed), and actions without a browser are refused.
#[tokio::test]
async fn actions_are_validated() {
    let state = Arc::new(state_with(None));
    let too_many: Vec<_> = (0..51)
        .map(|_| serde_json::json!({"type": "wait", "ms": 1}))
        .collect();
    let err = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({"url": "https://example.com/", "actions": too_many})),
    )
    .await
    .expect_err("too many actions");
    assert_eq!(err.code, "validation_error");
    assert!(err.error.contains("too many actions"), "{}", err.error);

    let err = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": "https://example.com/",
            "actions": [{"type": "wait"}]
        })),
    )
    .await
    .expect_err("wait needs ms or selector");
    assert_eq!(err.code, "validation_error");
    assert!(err.error.starts_with("actions[0] (wait)"), "{}", err.error);

    let err = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": "https://example.com/",
            "actions": [{"type": "scroll"}]
        })),
    )
    .await
    .expect_err("no browser");
    assert_eq!(err.code, "render_js_unavailable");

    // Unknown action types are rejected at deserialization.
    assert!(serde_json::from_value::<ScrapeRequest>(serde_json::json!({
        "url": "https://example.com/",
        "actions": [{"type": "hover", "selector": "a"}]
    }))
    .is_err());
}

/// Echoes the request's `Cookie` and `User-Agent` headers into the page;
/// the layout is responsive (phone below 600px).
struct EchoPage;

impl Respond for EchoPage {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let h = |name: &str| {
            req.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let body = format!(
            r#"<!doctype html><html><head><title>Echo</title>
<meta name="viewport" content="width=device-width, initial-scale=1"></head>
<body><p>cookie=[{}]</p><p>ua=[{}]</p></body></html>"#,
            h("cookie"),
            h("user-agent")
        );
        ResponseTemplate::new(200).set_body_raw(body, "text/html")
    }
}

async fn serve_echo(server: &MockServer) -> String {
    Mock::given(path("/echo"))
        .respond_with(EchoPage)
        .mount(server)
        .await;
    format!("{}/echo", server.uri().replace("127.0.0.1", "localhost"))
}

/// SCR-76: supplied cookies reach the server on the HTTP path, and never
/// carry over to a later request (the shared fetcher has no cookie store).
#[tokio::test]
async fn cookies_on_http_path_do_not_leak() {
    let state = Arc::new(state_with(None));
    let server = MockServer::start().await;
    let url = serve_echo(&server).await;

    let resp = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": url,
            "formats": ["rawhtml"],
            "cookies": [{"name": "sid", "value": "abc123"}, {"name": "theme", "value": "dark", "path": "/"}]
        })),
    )
    .await
    .unwrap();
    let html = resp.raw_html.unwrap();
    assert!(html.contains("sid=abc123"), "{html}");
    assert!(html.contains("theme=dark"), "{html}");

    let resp = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({"url": url, "formats": ["rawhtml"]})),
    )
    .await
    .unwrap();
    assert!(resp.raw_html.unwrap().contains("cookie=[]"), "no leak");
}

/// SCR-76: the one-off fetcher keeps cookies set along a redirect (login
/// flow) within the request.
#[tokio::test]
async fn http_path_keeps_session_cookies_across_redirects() {
    let state = Arc::new(state_with(None));
    let server = MockServer::start().await;
    Mock::given(path("/login"))
        .and(header("cookie", "sid=abc123"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("set-cookie", "session=s3cr3t; Path=/")
                .insert_header("location", "/account"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/account"))
        .and(|req: &Request| {
            let c = req
                .headers
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            c.contains("sid=abc123") && c.contains("session=s3cr3t")
        })
        .respond_with(ResponseTemplate::new(200).set_body_raw("<p>welcome back</p>", "text/html"))
        .mount(&server)
        .await;
    Mock::given(path("/account"))
        .respond_with(ResponseTemplate::new(401).set_body_raw("<p>denied</p>", "text/html"))
        .mount(&server)
        .await;
    let url = format!("{}/login", server.uri().replace("127.0.0.1", "localhost"));

    let resp = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": url,
            "formats": ["rawhtml"],
            "cookies": [{"name": "sid", "value": "abc123"}]
        })),
    )
    .await
    .unwrap();
    assert_eq!(resp.status_code, 200);
    assert!(resp.raw_html.unwrap().contains("welcome back"));
}

/// Cookies for another site are refused up front.
#[tokio::test]
async fn cookies_are_validated_against_the_target() {
    let state = Arc::new(state_with(None));
    let err = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": "https://www.example.com/",
            "cookies": [{"name": "sid", "value": "x", "domain": "evil.com"}]
        })),
    )
    .await
    .expect_err("foreign domain");
    assert_eq!(err.code, "validation_error");
    assert!(err.error.contains("does not match"), "{}", err.error);

    // `mobile` needs the browser.
    let err = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({"url": "https://www.example.com/", "mobile": true})),
    )
    .await
    .expect_err("no browser");
    assert_eq!(err.code, "render_js_unavailable");
}

/// SCR-76 on the browser path: `mobile: true` renders the phone layout with
/// a mobile UA, cookies arrive, `navigator.webdriver` is undefined, and the
/// next request (fresh browser context) does not see the cookies.
#[tokio::test]
async fn browser_path_mobile_cookies_and_stealth() {
    let Some(state) = browser_state().await else {
        return;
    };
    let server = MockServer::start().await;
    let url = serve_echo(&server).await;

    let resp = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({
            "url": url,
            "formats": ["rawhtml"],
            "mobile": true,
            "cookies": [{"name": "sid", "value": "abc123", "http_only": true}],
            "actions": [{"type": "execute_javascript", "script":
                "({ webdriver: typeof navigator.webdriver, narrow: matchMedia('(max-width: 600px)').matches, width: innerWidth })"}]
        })),
    )
    .await
    .unwrap();
    let html = resp.raw_html.unwrap();
    assert!(html.contains("cookie=[sid=abc123]"), "{html}");
    assert!(
        html.contains("Android") && !html.contains("Headless"),
        "{html}"
    );
    let fp = &resp.actions.unwrap().javascript_returns[0];
    assert_eq!(
        fp,
        &serde_json::json!({"webdriver": "undefined", "narrow": true, "width": 412})
    );

    let resp = perform_scrape(
        &state,
        &None,
        &request(serde_json::json!({"url": url, "formats": ["rawhtml"], "render_js": true})),
    )
    .await
    .unwrap();
    let html = resp.raw_html.unwrap();
    assert!(html.contains("cookie=[]"), "isolated: {html}");
    assert!(
        !html.contains("Headless") && !html.contains("Android"),
        "{html}"
    );
}
