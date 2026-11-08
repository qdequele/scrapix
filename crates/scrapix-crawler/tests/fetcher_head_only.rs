//! `FetchOptions::head_only`, `max_retries` and `timeout` (used by `/map`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use scrapix_core::CrawlUrl;
use scrapix_crawler::{
    FetchOptions, HttpFetcherBuilder, RobotsCache, RobotsConfig, HEAD_ONLY_MAX_BYTES,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fetcher() -> scrapix_crawler::HttpFetcher {
    let robots = Arc::new(
        RobotsCache::new(RobotsConfig {
            respect_robots: false,
            ..Default::default()
        })
        .unwrap(),
    );
    HttpFetcherBuilder::new()
        .allow_private_ips(true) // wiremock listens on 127.0.0.1
        .max_retries(2)
        .initial_backoff(Duration::from_millis(10))
        .build(robots)
        .unwrap()
}

// wiremock binds 127.0.0.1; the fetcher rejects raw IP hosts, so address it as localhost.
fn url(server: &MockServer, p: &str) -> String {
    format!("{}{}", server.uri().replace("127.0.0.1", "localhost"), p)
}

fn head_only() -> FetchOptions {
    FetchOptions {
        head_only: true,
        ..Default::default()
    }
}

async fn serve(server: &MockServer, p: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(p.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/html"))
        .mount(server)
        .await;
}

#[tokio::test]
async fn head_only_stops_at_the_end_of_the_head() {
    let server = MockServer::start().await;
    let head = "<html><HEAD><title>Hi</title></HEAD>";
    serve(
        &server,
        "/p",
        format!("{head}<body>{}</body></html>", "x".repeat(2_000_000)),
    )
    .await;

    let page = fetcher()
        .fetch_with_options(&CrawlUrl::seed(url(&server, "/p")), head_only())
        .await
        .unwrap();
    assert_eq!(page.status, 200);
    assert!(
        page.html.ends_with("</HEAD>"),
        "kept: {}",
        &page.html[..page.html.len().min(80)]
    );
    assert!(page.html.len() < 300_000, "read {} bytes", page.html.len());
}

#[tokio::test]
async fn head_only_never_rejects_a_large_body() {
    let server = MockServer::start().await;
    // Larger than the default 10 MB `max_body_size`, and no `</head>`.
    serve(&server, "/big", "y".repeat(11 * 1024 * 1024)).await;

    let page = fetcher()
        .fetch_with_options(&CrawlUrl::seed(url(&server, "/big")), head_only())
        .await
        .unwrap();
    assert_eq!(page.status, 200);
    assert!(page.html.len() <= HEAD_ONLY_MAX_BYTES);

    // Without head_only the same page is too large.
    let err = fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/big")))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
}

#[tokio::test]
async fn full_fetch_keeps_the_whole_body() {
    let server = MockServer::start().await;
    let body = format!("<head></head><body>{}</body>", "z".repeat(100_000));
    serve(&server, "/p", body.clone()).await;

    let page = fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/p")))
        .await
        .unwrap();
    assert_eq!(page.html, body);
}

#[tokio::test]
async fn max_retries_zero_makes_one_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/p"))
        .respond_with(ResponseTemplate::new(503).insert_header("content-type", "text/html"))
        .expect(1)
        .mount(&server)
        .await;
    let page = fetcher()
        .fetch_with_options(
            &CrawlUrl::seed(url(&server, "/p")),
            FetchOptions {
                max_retries: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.status, 503);
}

#[tokio::test]
async fn timeout_option_replaces_the_client_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("<p>late</p>".to_string(), "text/html")
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let started = Instant::now();
    let result = fetcher()
        .fetch_with_options(
            &CrawlUrl::seed(url(&server, "/slow")),
            FetchOptions {
                timeout: Some(Duration::from_millis(200)),
                max_retries: Some(0),
                ..Default::default()
            },
        )
        .await;
    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
}
