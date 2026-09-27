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

use std::time::Duration;

use scrapix_crawler::{CdpRenderer, CdpRendererBuilder, PageOptions, ScreenshotOptions};
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
