use std::net::IpAddr;
use std::sync::Arc;

use scrapix_core::CrawlUrl;
use scrapix_crawler::{is_public_ip, HttpFetcherBuilder, RobotsCache, RobotsConfig};
use wiremock::matchers::path;
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

#[test]
fn classifies_ips() {
    for ip in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "::1",
        "fe80::1",
        "fc00::1",
        "::ffff:127.0.0.1",
    ] {
        assert!(
            !is_public_ip(ip.parse::<IpAddr>().unwrap()),
            "{ip} must be non-public"
        );
    }
    for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
        assert!(
            is_public_ip(ip.parse::<IpAddr>().unwrap()),
            "{ip} must be public"
        );
    }
}

#[tokio::test]
async fn refuses_hostname_resolving_to_loopback() {
    let server = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let f = HttpFetcherBuilder::new().build(robots()).unwrap(); // allow_private_ips = false
    let u = server.uri().replace("127.0.0.1", "localhost");
    let err = f.fetch(&CrawlUrl::seed(u)).await.unwrap_err();
    assert!(err.to_string().contains("non-public"), "{err}");
}

#[tokio::test]
async fn refuses_redirect_to_raw_private_ip() {
    let server = MockServer::start().await;
    Mock::given(path("/r"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "http://169.254.169.254/latest/meta-data"),
        )
        .mount(&server)
        .await;
    // allow the first hop (test server is local) but still refuse raw-IP redirect targets
    let f = HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .build(robots())
        .unwrap();
    let u = format!("{}/r", server.uri().replace("127.0.0.1", "localhost"));
    let err = f.fetch(&CrawlUrl::seed(u)).await.unwrap_err();
    assert!(err.to_string().to_lowercase().contains("redirect"), "{err}");
}
