//! Cookie support on the HTTP fetcher: supplied cookies are sent, a
//! one-off fetcher's store keeps cookies set along a redirect (login flows),
//! and fetchers without a store never carry cookies between requests.

use std::sync::Arc;

use scrapix_core::CrawlUrl;
use scrapix_crawler::{HttpFetcherBuilder, RobotsCache, RobotsConfig};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn robots() -> Arc<RobotsCache> {
    Arc::new(
        RobotsCache::new(RobotsConfig {
            respect_robots: false,
            ..Default::default()
        })
        .unwrap(),
    )
}

fn base(server: &MockServer) -> String {
    server.uri().replace("127.0.0.1", "localhost")
}

#[tokio::test]
async fn supplied_cookies_are_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/echo"))
        .and(header("cookie", "sid=abc123"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("has cookie", "text/html"))
        .mount(&server)
        .await;
    Mock::given(path("/echo"))
        .respond_with(ResponseTemplate::new(401).set_body_raw("denied", "text/html"))
        .mount(&server)
        .await;
    let url = format!("{}/echo", base(&server));

    let fetcher = HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .max_retries(0)
        .cookie("sid=abc123; Path=/", url::Url::parse(&url).unwrap())
        .build(robots())
        .unwrap();
    let page = fetcher.fetch(&CrawlUrl::seed(&url)).await.unwrap();
    assert_eq!(page.status, 200);
    assert_eq!(page.html, "has cookie");
}

#[tokio::test]
async fn cookie_store_keeps_session_across_redirects() {
    let server = MockServer::start().await;
    Mock::given(path("/login"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("set-cookie", "session=s3cr3t; Path=/; HttpOnly")
                .insert_header("location", "/account"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/account"))
        .and(header("cookie", "session=s3cr3t"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("welcome back", "text/html"))
        .mount(&server)
        .await;
    Mock::given(path("/account"))
        .respond_with(ResponseTemplate::new(401).set_body_raw("who are you", "text/html"))
        .mount(&server)
        .await;
    let login = format!("{}/login", base(&server));

    // A one-off fetcher with a store follows the login flow.
    let one_off = HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .max_retries(0)
        .cookie_store(true)
        .build(robots())
        .unwrap();
    let page = one_off.fetch(&CrawlUrl::seed(&login)).await.unwrap();
    assert_eq!((page.status, page.html.as_str()), (200, "welcome back"));

    // A fetcher without a store (the shared one) carries nothing, not even
    // within the redirect — and nothing between requests.
    let shared = HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .max_retries(0)
        .build(robots())
        .unwrap();
    let page = shared.fetch(&CrawlUrl::seed(&login)).await.unwrap();
    assert_eq!(page.status, 401);
    let account = format!("{}/account", base(&server));
    let page = shared.fetch(&CrawlUrl::seed(&account)).await.unwrap();
    assert_eq!(page.status, 401, "no cookie leaks across requests");
}
