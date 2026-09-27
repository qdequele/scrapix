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
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

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

// ============================================================================
// Stealth, mobile emulation, cookies (SCR-76)
// ============================================================================

/// Echoes the request's `Cookie`, `User-Agent` and `Sec-CH-UA-Mobile`
/// headers into the page, plus a page script's view of the fingerprint.
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
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>.desktop{{display:block}} .mobile{{display:none}}
@media (max-width: 600px) {{ .desktop{{display:none}} .mobile{{display:block}} }}</style>
<script>
  // Runs before load, like a bot-detection script would.
  const gl = document.createElement('canvas').getContext('webgl');
  const dbg = gl && gl.getExtension('WEBGL_debug_renderer_info');
  window.__fp = {{
    webdriver: navigator.webdriver,
    webdriverType: typeof navigator.webdriver,
    plugins: navigator.plugins.length,
    languages: navigator.languages,
    chrome: typeof window.chrome,
    chromeRuntime: !!(window.chrome && window.chrome.runtime),
    userAgent: navigator.userAgent,
    platform: navigator.platform,
    brands: navigator.userAgentData ? navigator.userAgentData.brands.map(b => b.brand) : null,
    uaMobile: navigator.userAgentData ? navigator.userAgentData.mobile : null,
    webglVendor: dbg ? gl.getParameter(dbg.UNMASKED_VENDOR_WEBGL) : null,
    webglRenderer: dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : null,
    getParameterSource: gl ? Function.prototype.toString.call(gl.getParameter) : null,
    maxTouchPoints: navigator.maxTouchPoints,
    innerWidth: window.innerWidth,
    narrow: matchMedia('(max-width: 600px)').matches,
  }};
</script></head><body>
<div class="desktop">DESKTOP LAYOUT</div><div class="mobile">MOBILE LAYOUT</div>
<pre id="cookie">{cookie}</pre><pre id="ua">{ua}</pre><pre id="chmobile">{chm}</pre>
</body></html>"#,
            cookie = h("cookie"),
            ua = h("user-agent"),
            chm = h("sec-ch-ua-mobile"),
        );
        ResponseTemplate::new(200).set_body_raw(body, "text/html; charset=utf-8")
    }
}

async fn serve_echo(server: &MockServer) -> String {
    Mock::given(path("/echo"))
        .respond_with(EchoPage)
        .mount(server)
        .await;
    format!("{}/echo", server.uri().replace("127.0.0.1", "localhost"))
}

/// Text of `<pre id="{id}">` in rendered HTML.
fn pre(html: &str, id: &str) -> String {
    let open = format!("<pre id=\"{id}\">");
    let start = html.find(&open).unwrap_or_else(|| panic!("no #{id}")) + open.len();
    let end = start + html[start..].find("</pre>").unwrap();
    html[start..end]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Render `url` with `opts` and return the page script's fingerprint.
async fn fingerprint(
    r: &CdpRenderer,
    url: &str,
    mut opts: PageOptions,
) -> (serde_json::Value, String) {
    opts.actions.push(Action::ExecuteJavascript {
        script: "window.__fp".into(),
    });
    let result = r.render_page(url, &opts).await.unwrap();
    (result.javascript_returns[0].clone(), result.html)
}

/// The page's own (pre-load) script sees a regular desktop Chrome:
/// `navigator.webdriver` is undefined, no "HeadlessChrome" anywhere, and
/// plausible plugins, languages, `window.chrome` and WebGL strings.
#[tokio::test]
async fn stealth_fingerprint_looks_like_desktop_chrome() {
    let Some(r) = renderer().await else { return };
    let server = MockServer::start().await;
    let url = serve_echo(&server).await;

    let (fp, html) = fingerprint(&r, &url, PageOptions::default()).await;
    eprintln!("desktop fingerprint: {fp:#}");
    assert_eq!(fp["webdriverType"], "undefined", "{fp}");
    assert!(fp["webdriver"].is_null());
    assert!(fp["plugins"].as_u64().unwrap() > 0, "{fp}");
    assert_eq!(fp["languages"], serde_json::json!(["en-US", "en"]));
    assert_eq!(fp["chrome"], "object");
    assert_eq!(fp["chromeRuntime"], true);
    let ua = fp["userAgent"].as_str().unwrap();
    assert!(!ua.contains("Headless"), "{ua}");
    assert!(ua.contains("Chrome/"), "{ua}");
    let brands = fp["brands"].to_string();
    assert!(!brands.contains("Headless"), "{brands}");
    assert!(brands.contains("Google Chrome"), "{brands}");
    assert_eq!(fp["maxTouchPoints"], 0, "desktop has no touch");
    assert_eq!(fp["uaMobile"], false);
    // WebGL is available (software rasterizer) and reports a real GPU.
    let vendor = fp["webglVendor"].as_str().expect("WebGL available");
    assert!(!vendor.contains("SwiftShader"), "{vendor}");
    let renderer = fp["webglRenderer"].as_str().unwrap();
    assert!(!renderer.contains("SwiftShader"), "{renderer}");
    assert!(renderer.starts_with("ANGLE ("), "{renderer}");
    assert!(
        fp["getParameterSource"]
            .as_str()
            .unwrap()
            .contains("[native code]"),
        "patched getParameter still looks native"
    );
    // The request header matches navigator.userAgent.
    let header_ua = pre(&html, "ua");
    assert!(!header_ua.contains("Headless"), "{header_ua}");
    assert_eq!(header_ua, ua);
    assert!(html.contains("DESKTOP LAYOUT"));
    r.close().await;
}

/// `mobile: true` renders the phone layout of a responsive page, with a
/// mobile user agent, client hints and touch.
#[tokio::test]
async fn mobile_emulation_renders_mobile_layout() {
    let Some(r) = renderer().await else { return };
    let server = MockServer::start().await;
    let url = serve_echo(&server).await;

    let mut opts = PageOptions {
        mobile: true,
        screenshot: Some(ScreenshotOptions { full_page: false }),
        ..Default::default()
    };
    opts.isolate = true;
    let (fp, _) = fingerprint(&r, &url, opts.clone()).await;
    eprintln!("mobile fingerprint: {fp:#}");
    assert_eq!(fp["narrow"], true, "mobile media query matches: {fp}");
    assert_eq!(fp["innerWidth"], 412);
    assert!(fp["maxTouchPoints"].as_u64().unwrap() > 0);
    assert_eq!(fp["uaMobile"], true);
    assert!(fp["userAgent"].as_str().unwrap().contains("Android"));

    let result = r.render_page(&url, &opts).await.unwrap();
    let text = pre(&result.html, "ua");
    assert!(text.contains("Mobile Safari"), "UA header: {text}");
    assert_eq!(pre(&result.html, "chmobile"), "?1", "Sec-CH-UA-Mobile");
    let (w, _) = png_size(result.screenshot.as_deref().unwrap());
    assert!(w < 1280, "phone-width screenshot (device pixels), got {w}");

    // The same page without `mobile` is the desktop layout.
    let (fp, _) = fingerprint(&r, &url, PageOptions::default()).await;
    assert_eq!(fp["narrow"], false);
    assert_eq!(fp["innerWidth"], 1280);
    r.close().await;
}

/// Supplied cookies reach the server on the browser path; isolated renders
/// never see each other's cookies (supplied or set by the page).
#[tokio::test]
async fn cookies_are_sent_and_isolated() {
    let Some(r) = renderer().await else { return };
    let server = MockServer::start().await;
    let url = serve_echo(&server).await;

    let cookie = |name: &str, value: &str| scrapix_core::browser::RequestCookie {
        name: name.into(),
        value: value.into(),
        domain: None,
        path: None,
        secure: None,
        http_only: Some(true),
    };
    let first = PageOptions {
        cookies: vec![cookie("sid", "abc123"), cookie("theme", "dark")],
        isolate: true,
        actions: vec![Action::ExecuteJavascript {
            script: "document.cookie = 'set_by_page=1; path=/'; document.cookie".into(),
        }],
        ..Default::default()
    };
    let result = r.render_page(&url, &first).await.unwrap();
    let sent = pre(&result.html, "cookie");
    assert!(sent.contains("sid=abc123"), "cookie header: {sent}");
    assert!(sent.contains("theme=dark"), "cookie header: {sent}");
    // HttpOnly cookies are not visible to the page's script.
    assert_eq!(result.javascript_returns[0], "set_by_page=1");

    // A later isolated render starts from an empty jar.
    let second = PageOptions {
        isolate: true,
        ..Default::default()
    };
    let result = r.render_page(&url, &second).await.unwrap();
    assert_eq!(pre(&result.html, "cookie"), "", "no cookie leaks");
    r.close().await;
}
