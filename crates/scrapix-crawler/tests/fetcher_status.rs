use std::sync::Arc;
use std::time::{Duration, Instant};

use scrapix_core::CrawlUrl;
use scrapix_crawler::{parse_retry_after, HttpFetcherBuilder, RobotsCache, RobotsConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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

// The mocked response carries a Content-Length (wiremock always sets one for
// a non-chunked body), so this only exercises the up-front Content-Length
// rejection, not the "enforced while streaming" path — see
// `enforces_cap_while_streaming_without_content_length` below for that.
#[tokio::test]
async fn rejects_oversized_body_via_content_length() {
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

/// R1: a status is never turned into an `Err`. An oversized *non-2xx* body
/// (common for SPA/CDN error pages) must not fail the fetch — it's truncated
/// to the (small, fixed) non-2xx cap and the final status is still returned.
#[tokio::test]
async fn truncates_oversized_non_2xx_body_instead_of_failing() {
    let server = MockServer::start().await;
    let big_body = "e".repeat(100 * 1024); // ~100 KB, well over the 64 KiB non-2xx cap
    Mock::given(method("GET"))
        .and(path("/not-found"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(big_body, "text/html"))
        .mount(&server)
        .await;
    let page = fetcher()
        .fetch(&CrawlUrl::seed(url(&server, "/not-found")))
        .await
        .unwrap();
    assert_eq!(page.status, 404);
    assert!(!page.is_success());
    assert!(
        page.html.len() <= 64 * 1024,
        "expected truncated body <= 64 KiB, got {} bytes",
        page.html.len()
    );
}

/// R8: the body-size cap must be enforced while streaming, not just via an
/// up-front Content-Length check — wiremock always sets Content-Length for a
/// non-chunked body, so it can't exercise this path. This spins up a raw
/// `TcpListener` and hand-writes an HTTP/1.1 response with
/// `Transfer-Encoding: chunked` (no Content-Length) whose body exceeds
/// `max_body_size`, and expects the streaming cap to still reject it.
#[tokio::test]
async fn enforces_cap_while_streaming_without_content_length() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            // We don't care about the request; just drain what's readily
            // available before writing the response.
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;

            let body = "y".repeat(4096); // over max_body_size(1024) below
            let mut response = String::new();
            response.push_str("HTTP/1.1 200 OK\r\n");
            response.push_str("Content-Type: text/html\r\n");
            response.push_str("Transfer-Encoding: chunked\r\n");
            response.push_str("Connection: close\r\n");
            response.push_str("\r\n");
            response.push_str(&format!("{:x}\r\n", body.len()));
            response.push_str(&body);
            response.push_str("\r\n0\r\n\r\n");

            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });

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
        .fetch(&CrawlUrl::seed(format!("http://localhost:{port}/big")))
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

#[tokio::test]
async fn per_request_user_agent_and_headers_are_sent() {
    use wiremock::matchers::header;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/h"))
        .and(header("user-agent", "JobUA/1"))
        .and(header("x-test", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("ok", "text/html"))
        .expect(1)
        .mount(&server)
        .await;
    let opts = scrapix_crawler::FetchOptions {
        user_agent: Some("JobUA/1".into()),
        extra_headers: vec![("x-test".into(), "1".into())],
        ..Default::default()
    };
    let page = fetcher()
        .fetch_with_options(&CrawlUrl::seed(url(&server, "/h")), opts)
        .await
        .unwrap();
    assert_eq!(page.status, 200);
}

#[tokio::test]
async fn per_request_proxy_routes_through_proxy() {
    // The wiremock server acts as a plain HTTP proxy: the origin host does
    // not exist, so the only way to get a 200 is through the proxy.
    let proxy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/via-proxy"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("proxied", "text/html"))
        .expect(1)
        .mount(&proxy)
        .await;
    let opts = scrapix_crawler::FetchOptions {
        proxy: Some(proxy.uri().replace("127.0.0.1", "localhost")),
        ..Default::default()
    };
    let page = fetcher()
        .fetch_with_options(&CrawlUrl::seed("http://origin.invalid/via-proxy"), opts)
        .await
        .unwrap();
    assert_eq!(page.status, 200);
    assert_eq!(page.html, "proxied");
}

#[tokio::test]
async fn respect_robots_false_skips_robots_check() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/private"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("ok", "text/html"))
        .mount(&server)
        .await;
    let robots = Arc::new(
        RobotsCache::new(RobotsConfig {
            respect_robots: true,
            allow_private_ips: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let fetcher = HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .max_retries(0)
        .build(robots)
        .unwrap();
    let target = CrawlUrl::seed(url(&server, "/private"));

    let denied = fetcher.fetch(&target).await;
    assert!(
        matches!(
            denied,
            Err(scrapix_core::ScrapixError::RobotsDisallowed { .. })
        ),
        "{denied:?}"
    );

    let opts = scrapix_crawler::FetchOptions {
        respect_robots: Some(false),
        ..Default::default()
    };
    let page = fetcher.fetch_with_options(&target, opts).await.unwrap();
    assert_eq!(page.status, 200);
}

#[tokio::test]
async fn raw_ip_private_proxy_is_refused() {
    let robots = Arc::new(
        RobotsCache::new(RobotsConfig {
            respect_robots: false,
            ..Default::default()
        })
        .unwrap(),
    );
    // Production SSRF policy: private addresses not allowed.
    let fetcher = HttpFetcherBuilder::new()
        .max_retries(0)
        .build(robots)
        .unwrap();
    for proxy in [
        "http://169.254.169.254:80",
        "http://127.0.0.1:3128",
        "http://localhost:3128",
    ] {
        let opts = scrapix_crawler::FetchOptions {
            proxy: Some(proxy.into()),
            ..Default::default()
        };
        let r = fetcher
            .fetch_with_options(&CrawlUrl::seed("http://origin.invalid/x"), opts)
            .await;
        assert!(
            matches!(r, Err(scrapix_core::ScrapixError::Refused(_))),
            "{proxy}: {r:?}"
        );
    }
}
