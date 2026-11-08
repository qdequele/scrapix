//! Sitemap-index children are fetched concurrently, merged in index order,
//! and the walk stops once `max_urls` is reached.

use std::time::{Duration, Instant};

use scrapix_crawler::{SitemapConfig, SitemapParser};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn base(server: &MockServer) -> String {
    server.uri().replace("127.0.0.1", "localhost")
}

fn parser(max_urls: usize) -> SitemapParser {
    SitemapParser::new(SitemapConfig {
        allow_private_ips: true, // wiremock listens on 127.0.0.1
        max_urls,
        ..Default::default()
    })
}

/// An index of `children` sitemaps, each holding 2 URLs and answering
/// after `delay`.
async fn site(children: usize, delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    let root = base(&server);
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!("User-agent: *\nSitemap: {root}/sitemap.xml\n")),
        )
        .mount(&server)
        .await;
    let entries: String = (0..children)
        .map(|i| format!("<sitemap><loc>{root}/s{i}.xml</loc></sitemap>"))
        .collect();
    Mock::given(method("GET"))
        .and(path("/sitemap.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"<?xml version="1.0"?><sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{entries}</sitemapindex>"#
        )))
        .mount(&server)
        .await;
    for i in 0..children {
        Mock::given(method("GET"))
            .and(path(format!("/s{i}.xml")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!(
                        r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"><url><loc>{root}/p{i}a</loc></url><url><loc>{root}/p{i}b</loc></url></urlset>"#
                    ))
                    .set_delay(delay),
            )
            .mount(&server)
            .await;
    }
    server
}

#[tokio::test]
async fn index_children_are_fetched_concurrently_in_order() {
    let server = site(8, Duration::from_millis(400)).await;
    let root = base(&server);

    let started = Instant::now();
    let urls = parser(100_000).discover_all_urls(&root).await.unwrap();
    let took = started.elapsed();

    let locs: Vec<String> = urls.into_iter().map(|u| u.loc).collect();
    let expected: Vec<String> = (0..8)
        .flat_map(|i| [format!("{root}/p{i}a"), format!("{root}/p{i}b")])
        .collect();
    assert_eq!(locs, expected);
    // Sequential: 8 x 400 ms = 3.2 s.
    assert!(took < Duration::from_millis(1600), "took {took:?}");
}

#[tokio::test]
async fn stops_at_max_urls() {
    let server = site(20, Duration::ZERO).await;
    let root = base(&server);

    let urls = parser(5).discover_all_urls(&root).await.unwrap();
    assert_eq!(urls.len(), 5);
    assert_eq!(urls[0].loc, format!("{root}/p0a"));
}
