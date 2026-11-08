//! `/map` over a local site: one fetch per page, head-only reads where no
//! links are followed, relative links, and the adaptive concurrency.

use super::*;
use scrapix_queue::ChannelBus;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::results::test_support::test_state;

fn root(server: &MockServer) -> String {
    server.uri().replace("127.0.0.1", "localhost")
}

/// `p` answers `html`, and must be fetched exactly `times` times.
async fn page(server: &MockServer, p: &str, html: String, times: u64) {
    Mock::given(method("GET"))
        .and(path(p.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html, "text/html"))
        .expect(times)
        .mount(server)
        .await;
}

fn html(title: &str, links: &[&str]) -> String {
    let links: String = links
        .iter()
        .map(|href| format!(r#"<a href="{href}">{href}</a>"#))
        .collect();
    format!("<html><head><title>{title}</title></head><body>{links}</body></html>")
}

async fn map(body: serde_json::Value) -> MapResponse {
    let bus = ChannelBus::new();
    let state = test_state(&bus);
    let request: MapRequest = serde_json::from_value(body).unwrap();
    map_url(State(state), None, Json(request)).await.unwrap().0
}

fn title_of<'a>(response: &'a MapResponse, url: &str) -> Option<&'a str> {
    response
        .links
        .iter()
        .find(|l| l.url == url)
        .unwrap_or_else(|| panic!("{url} not in the map"))
        .title
        .as_deref()
}

/// With `depth: 1`, sitemap pages are fetched once for both their title and
/// their links (not once for each), and the pages found one level down
/// once for their title.
#[tokio::test]
async fn each_page_is_fetched_once() {
    let server = MockServer::start().await;
    let base = root(&server);
    Mock::given(method("GET"))
        .and(path("/sitemap.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>{base}/</loc></url><url><loc>{base}/a</loc></url></urlset>"#
        )))
        .mount(&server)
        .await;
    page(&server, "/", html("Home", &["/a", "/c"]), 1).await;
    page(&server, "/a", html("A", &["/d", "/"]), 1).await;
    page(&server, "/c", html("C", &["/never"]), 1).await;
    page(&server, "/d", html("D", &[]), 1).await;
    page(&server, "/never", html("Never", &[]), 0).await;

    let response = map(serde_json::json!({"url": format!("{base}/"), "depth": 1})).await;

    let mut urls: Vec<&str> = response.links.iter().map(|l| l.url.as_str()).collect();
    urls.sort();
    let expected = [
        format!("{base}/"),
        format!("{base}/a"),
        format!("{base}/c"),
        format!("{base}/d"),
    ];
    assert_eq!(
        urls,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(title_of(&response, &format!("{base}/")), Some("Home"));
    assert_eq!(title_of(&response, &format!("{base}/c")), Some("C"));
    assert_eq!(title_of(&response, &format!("{base}/d")), Some("D"));
    // `expect(..)` counts are checked when the server drops.
}

/// Without titles or deeper levels, sitemap URLs are not fetched at all.
#[tokio::test]
async fn a_sitemap_only_map_fetches_no_page() {
    let server = MockServer::start().await;
    let base = root(&server);
    Mock::given(method("GET"))
        .and(path("/sitemap.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>{base}/x</loc></url></urlset>"#
        )))
        .mount(&server)
        .await;
    page(&server, "/x", html("X", &[]), 0).await;

    let response = map(serde_json::json!({
        "url": format!("{base}/"),
        "get_title": false,
        "get_description": false,
    }))
    .await;
    assert_eq!(response.total, 1);
    assert_eq!(response.links[0].url, format!("{base}/x"));
}

/// Relative links resolve against the page they are on, not the URL the
/// map started from.
#[tokio::test]
async fn relative_links_resolve_against_their_page() {
    let server = MockServer::start().await;
    let base = root(&server);
    page(&server, "/start", html("Start", &["/docs/"]), 1).await;
    page(&server, "/docs/", html("Docs", &["intro"]), 1).await;
    page(&server, "/docs/intro", html("Intro", &[]), 1).await;

    let response = map(serde_json::json!({
        "url": format!("{base}/start"),
        "depth": 2,
        "sitemap": false,
    }))
    .await;
    assert_eq!(
        title_of(&response, &format!("{base}/docs/intro")),
        Some("Intro")
    );
}

#[test]
fn concurrency_grows_on_success_and_halves_on_throttle() {
    let mut c = MapConcurrency::adaptive();
    assert_eq!(c.limit(), MAP_INITIAL_CONCURRENCY);
    for _ in 0..1_000 {
        c.on_success();
    }
    assert_eq!(c.limit(), MAP_MAX_CONCURRENCY);

    let t0 = std::time::Instant::now();
    c.on_throttle(t0);
    assert_eq!(c.limit(), MAP_MAX_CONCURRENCY / 2);
    // Failures already in flight within the cooldown count once.
    c.on_throttle(t0 + Duration::from_millis(10));
    assert_eq!(c.limit(), MAP_MAX_CONCURRENCY / 2);
    // Never below the floor.
    for i in 1..20 {
        c.on_throttle(t0 + MAP_BACKOFF_COOLDOWN * i);
    }
    assert_eq!(c.limit(), MAP_MIN_CONCURRENCY);
}

#[test]
fn fixed_concurrency_never_moves() {
    let mut c = MapConcurrency::fixed(7);
    c.on_success();
    c.on_throttle(std::time::Instant::now());
    assert_eq!(c.limit(), 7);
}

/// Times `/map` on live sites (network; not run by default):
///
/// ```text
/// MAP_BENCH='[{"url":"https://example.com","limit":1000}]' \
///   cargo test -p scrapix-api --release map_bench -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "network benchmark"]
async fn map_bench() {
    let runs: Vec<serde_json::Value> =
        serde_json::from_str(&std::env::var("MAP_BENCH").expect("MAP_BENCH")).unwrap();
    for body in runs {
        let started = std::time::Instant::now();
        let response = map(body.clone()).await;
        let titled = response.links.iter().filter(|l| l.title.is_some()).count();
        println!(
            "MAP_BENCH {body} -> {} urls, {titled} titled, {:.1}s",
            response.total,
            started.elapsed().as_secs_f64()
        );
    }
}
