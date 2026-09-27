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
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
