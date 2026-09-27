//! Real-browser tests for the CDP renderer, against local fixture pages.
//!
//! Needs Chrome/Chromium: set `SCRAPIX_TEST_CHROME` to the executable path
//! (or to `1` to auto-detect), and build with the `browser-cdp` feature:
//!
//! ```bash
//! SCRAPIX_TEST_CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
//!   cargo test -p scrapix-crawler --features browser-cdp --test cdp_browser
//! ```
//!
//! Without it every test prints a skip note and passes (the same gating as
//! the `SCRAPIX_TEST_REDIS_URL` tests), so CI without a browser stays green.
//! Fixtures are served by wiremock on 127.0.0.1 and addressed as
//! `localhost` (raw-IP hosts are always refused), with the renderer's
//! `allow_private_ips` opt-out.

#![cfg(feature = "browser-cdp")]

use std::time::{Duration, Instant};

use scrapix_core::browser::Action;
use scrapix_core::ScrapixError;
use scrapix_crawler::{
    CdpRenderer, CdpRendererBuilder, PageOptions, ScreenshotOptions, ACTION_TIMEOUT,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A renderer on the test browser, or `None` (skip) when
/// `SCRAPIX_TEST_CHROME` is not set.
async fn renderer() -> Option<CdpRenderer> {
    let chrome = std::env::var("SCRAPIX_TEST_CHROME")
        .ok()
        .filter(|s| !s.is_empty());
    let Some(chrome) = chrome else {
        eprintln!("SCRAPIX_TEST_CHROME not set; skipping browser test");
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
    Some(builder.build().await.expect("launch test browser"))
}

/// Serve `html` at `route` and return the page URL (on `localhost`).
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

/// Width and height from a PNG's IHDR chunk (panics if not a PNG).
fn png_size(png: &[u8]) -> (u32, u32) {
    assert!(png.len() > 24, "PNG too short");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
    (w, h)
}

const TALL_PAGE: &str = r#"<!doctype html><html><head><title>Tall</title>
<style>body{margin:0} .block{height:1000px;background:linear-gradient(red,blue)}</style>
</head><body><div class="block"></div><div class="block"></div><div class="block"></div>
<p id="end">end</p></body></html>"#;

#[tokio::test]
async fn screenshot_full_page_and_viewport() {
    let Some(r) = renderer().await else { return };
    let server = MockServer::start().await;
    let url = serve(&server, "/tall", TALL_PAGE).await;

    let full = r
        .render_page(
            &url,
            &PageOptions {
                screenshot: Some(ScreenshotOptions { full_page: true }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(full.html.contains("id=\"end\""));
    let (w, h) = png_size(full.screenshot.as_deref().expect("screenshot"));
    assert_eq!(w, 1280, "full-page width is the viewport width");
    assert!(h >= 3000, "full page covers the whole document, got {h}");

    let viewport = r
        .render_page(
            &url,
            &PageOptions {
                screenshot: Some(ScreenshotOptions { full_page: false }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let (w, h) = png_size(viewport.screenshot.as_deref().expect("screenshot"));
    assert_eq!((w, h), (1280, 800), "viewport-only capture");

    // No screenshot unless asked.
    let plain = r.render(&url).await.unwrap();
    assert!(plain.screenshot.is_none());
    r.close().await;
}

// ============================================================================
// Page actions (SCR-75)
// ============================================================================

const ACTIONS_PAGE: &str = r#"<!doctype html><html><head><title>Actions</title></head><body>
<button id="more" onclick="document.getElementById('out').insertAdjacentHTML('beforeend','<p class=loaded>clicked</p>')">More</button>
<div id="out"></div>
<input id="q" onkeydown="if (event.key === 'Enter') document.getElementById('entered').textContent = 'entered:' + this.value">
<p id="entered"></p>
<p id="scrolled">0</p>
<div style="height:5000px"></div>
<script>
setTimeout(() => document.body.insertAdjacentHTML('beforeend', '<div id="late">late</div>'), 700);
window.addEventListener('scroll', () => { document.getElementById('scrolled').textContent = 'y=' + Math.round(window.scrollY); });
</script>
</body></html>"#;

fn actions(json: serde_json::Value) -> Vec<Action> {
    let actions: Vec<Action> = serde_json::from_value(json).unwrap();
    scrapix_core::browser::validate_actions(&actions).unwrap();
    actions
}

fn with_actions(actions: Vec<Action>) -> PageOptions {
    PageOptions {
        actions,
        guard_requests: true,
        ..Default::default()
    }
}

/// Every action type against a fixture page, in one sequence.
#[tokio::test]
async fn every_action_type_runs_in_order() {
    let Some(r) = renderer().await else { return };
    let server = MockServer::start().await;
    let url = serve(&server, "/actions", ACTIONS_PAGE).await;

    let opts = with_actions(actions(serde_json::json!([
        {"type": "wait", "selector": "#late"},
        {"type": "click", "selector": "#more"},
        {"type": "write", "selector": "#q", "text": "héllo wörld"},
        {"type": "press", "key": "Enter"},
        {"type": "scroll", "direction": "down", "amount": 2},
        {"type": "wait", "ms": 100},
        {"type": "execute_javascript", "script": "document.querySelectorAll('.loaded').length"},
        {"type": "execute_javascript", "script": "return { title: document.title, q: document.getElementById('q').value }"},
        {"type": "execute_javascript", "script": "await new Promise(r => setTimeout(r, 20)); window.scrollY > 0"},
        {"type": "execute_javascript", "script": "undefined"},
        {"type": "scroll", "direction": "up", "amount": 10},
        {"type": "execute_javascript", "script": "window.scrollY"}
    ])));
    let result = r.render_page(&url, &opts).await.unwrap();

    assert_eq!(
        result.javascript_returns,
        vec![
            serde_json::json!(1),
            serde_json::json!({"title": "Actions", "q": "héllo wörld"}),
            serde_json::json!(true),
            serde_json::Value::Null,
            serde_json::json!(0),
        ]
    );
    // Content is captured after the actions.
    assert!(result.html.contains("class=\"loaded\""), "click effect");
    assert!(result.html.contains("entered:héllo wörld"), "write + press");
    assert!(result.html.contains("id=\"late\""), "wait for selector");
    assert!(result.html.contains(">y="), "scroll event fired");
    r.close().await;
}

fn action_error(e: ScrapixError) -> (usize, String, String) {
    match e {
        ScrapixError::Action {
            index,
            action,
            message,
        } => (index, action, message),
        other => panic!("expected an action error, got {other:?}"),
    }
}

/// A failing step names its index and type; a missing selector fails at
/// the per-action timeout rather than hanging the request.
#[tokio::test]
async fn failing_actions_report_index_and_type() {
    let Some(r) = renderer().await else { return };
    let server = MockServer::start().await;
    let url = serve(&server, "/actions", ACTIONS_PAGE).await;

    // Script exception.
    let err = r
        .render_page(
            &url,
            &with_actions(actions(serde_json::json!([
                {"type": "wait", "ms": 10},
                {"type": "execute_javascript", "script": "throw new Error('boom')"}
            ]))),
        )
        .await
        .unwrap_err();
    let (index, action, message) = action_error(err);
    assert_eq!((index, action.as_str()), (1, "execute_javascript"));
    assert!(message.contains("boom"), "{message}");

    // Invalid selector fails fast.
    let started = Instant::now();
    let err = r
        .render_page(
            &url,
            &with_actions(actions(
                serde_json::json!([{"type": "click", "selector": "[[nope"}]),
            )),
        )
        .await
        .unwrap_err();
    let (index, action, message) = action_error(err);
    assert_eq!((index, action.as_str()), (0, "click"));
    assert!(message.contains("invalid CSS selector"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(5));

    // Missing element: bounded by the per-action timeout.
    let started = Instant::now();
    let err = r
        .render_page(
            &url,
            &with_actions(actions(serde_json::json!([
                {"type": "scroll"},
                {"type": "write", "selector": "#missing", "text": "x"}
            ]))),
        )
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    let (index, action, message) = action_error(err);
    assert_eq!((index, action.as_str()), (1, "write"));
    assert!(
        message.contains("no element matches `#missing`"),
        "{message}"
    );
    assert!(
        elapsed < ACTION_TIMEOUT + Duration::from_secs(8),
        "bounded by the action timeout, took {elapsed:?}"
    );

    // Overall budget: the second wait no longer fits.
    let mut opts = with_actions(actions(serde_json::json!([
        {"type": "wait", "ms": 700},
        {"type": "wait", "ms": 700}
    ])));
    opts.actions_budget = Some(Duration::from_secs(1));
    let (index, action, message) = action_error(r.render_page(&url, &opts).await.unwrap_err());
    assert_eq!((index, action.as_str()), (1, "wait"));
    assert!(message.contains("budget"), "{message}");

    // Unknown key.
    let (index, action, message) = action_error(
        r.render_page(
            &url,
            &with_actions(actions(
                serde_json::json!([{"type": "press", "key": "NoSuchKey"}]),
            )),
        )
        .await
        .unwrap_err(),
    );
    assert_eq!((index, action.as_str()), (0, "press"));
    assert!(message.contains("unknown key"), "{message}");
    r.close().await;
}

/// Requests the page makes after load — a script's `fetch`, a click on a
/// link — to a non-public address are blocked by the request guard: the
/// internal endpoint never sees a request.
#[tokio::test]
async fn request_guard_blocks_internal_targets_from_actions() {
    let Some(r) = renderer().await else { return };
    let server = MockServer::start().await;
    // The "internal" endpoint, addressed by raw loopback IP (never public).
    Mock::given(path("/secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("access-control-allow-origin", "*")
                .set_body_string("top-secret"),
        )
        .expect(0)
        .mount(&server)
        .await;
    let internal = format!("{}/secret", server.uri());
    let page =
        format!(r#"<!doctype html><html><body><a id="go" href="{internal}">go</a></body></html>"#);
    let url = serve(&server, "/page", &page).await;

    let script = format!(
        "await fetch({internal:?}).then(r => r.text()).catch(e => 'blocked: ' + e.message)"
    );
    let result = r
        .render_page(
            &url,
            &with_actions(vec![
                Action::ExecuteJavascript { script },
                Action::Click {
                    selector: "#go".into(),
                },
            ]),
        )
        .await
        .unwrap();
    let fetched = result.javascript_returns[0].as_str().unwrap().to_string();
    assert!(fetched.starts_with("blocked"), "{fetched}");
    assert!(
        !result.html.contains("top-secret"),
        "internal page not returned"
    );
    // Dropping the server verifies `.expect(0)`.
    drop(server);
    r.close().await;
}

/// `execute_script` goes through the same checked path.
#[tokio::test]
async fn execute_script_returns_value() {
    let Some(r) = renderer().await else { return };
    let server = MockServer::start().await;
    let url = serve(&server, "/actions", ACTIONS_PAGE).await;
    let v = r.execute_script(&url, "document.title").await.unwrap();
    assert_eq!(v, serde_json::json!("Actions"));
    r.close().await;
}
