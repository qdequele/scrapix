use std::sync::Arc;
use std::time::{Duration, Instant};

use scrapix_core::CrawlUrl;
use scrapix_crawler::{parse_retry_after, HttpFetcherBuilder, RobotsCache, RobotsConfig};
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

#[tokio::test]
async fn retries_503_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/p"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/p"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<p>ok</p>".to_string(), "text/html"))
        .mount(&server)
        .await;
    let page = fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/p")))
        .await
        .unwrap();
    assert_eq!(page.status, 200);
    assert!(page.is_success());
}

#[tokio::test]
async fn returns_final_status_after_exhausting_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/p"))
        .respond_with(ResponseTemplate::new(503).insert_header("content-type", "text/html"))
        .expect(3)
        .mount(&server)
        .await;
    let page = fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/p")))
        .await
        .unwrap();
    assert_eq!(page.status, 503);
    assert!(!page.is_success());
}

#[tokio::test]
async fn does_not_retry_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/p"))
        .respond_with(ResponseTemplate::new(404).insert_header("content-type", "text/html"))
        .expect(1)
        .mount(&server)
        .await;
    let page = fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/p")))
        .await
        .unwrap();
    assert_eq!(page.status, 404);
}

#[tokio::test]
async fn honors_retry_after_seconds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/p"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/p"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("ok".to_string(), "text/html"))
        .mount(&server)
        .await;
    let started = Instant::now();
    let page = fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/p")))
        .await
        .unwrap();
    assert_eq!(page.status, 200);
    assert!(started.elapsed() >= Duration::from_millis(950));
}

#[tokio::test]
async fn rejects_oversized_body_by_content_length_and_by_stream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("x".repeat(4096), "text/html"))
        .mount(&server)
        .await;
    let robots = Arc::new(
        RobotsCache::new(RobotsConfig {
            respect_robots: false,
            ..Default::default()
        })
        .unwrap(),
    );
    let f = HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .max_body_size(1024)
        .build(robots)
        .unwrap();
    let err = f
        .fetch(&CrawlUrl::seed(url(&server, "/big")))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too large"), "{err}");
}

#[test]
fn parse_retry_after_forms() {
    let now = chrono::DateTime::parse_from_rfc2822("Wed, 21 Oct 2015 07:28:00 GMT")
        .unwrap()
        .to_utc();
    assert_eq!(
        parse_retry_after("120", now),
        Some(Duration::from_secs(120))
    );
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2015 07:28:30 GMT", now),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2015 07:27:00 GMT", now),
        Some(Duration::ZERO)
    );
    assert_eq!(parse_retry_after("soon", now), None);
}
