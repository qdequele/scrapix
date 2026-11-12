//! A fetcher built with its robots.txt cache (`build_with_robots`) fetches
//! robots.txt through its own DNS cache and connection pool: one lookup and
//! one connection per new origin instead of two. The robots.txt request
//! itself stays what a dedicated robots client sends, and the client is not
//! shared when the fetcher's would send something else.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use scrapix_core::CrawlUrl;
use scrapix_crawler::{HttpFetcher, HttpFetcherBuilder, RobotsCache, RobotsConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A keep-alive HTTP/1.1 server that counts the connections it accepts and
/// records each request head.
struct Origin {
    port: u16,
    connections: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Origin {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (accepted, seen) = (connections.clone(), requests.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        while let Some(end) = find(&buffer, b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                            buffer.drain(..end + 4);
                            let body = if head.starts_with("get /robots.txt ") {
                                "User-agent: *\nDisallow: /private\n"
                            } else {
                                "<html><body><a href=\"/next\">next</a></body></html>"
                            };
                            seen.lock().push(head);
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            if socket.write_all(response.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    }
                });
            }
        });
        Self {
            port,
            connections,
            requests,
        }
    }

    fn url(&self, path: &str) -> CrawlUrl {
        CrawlUrl::seed(format!("http://localhost:{}{}", self.port, path))
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// The recorded request heads (lowercased) for `path`.
    fn requests_for(&self, path: &str) -> Vec<String> {
        self.requests
            .lock()
            .iter()
            .filter(|head| head.starts_with(&format!("get {path} ")))
            .cloned()
            .collect()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn robots_config() -> RobotsConfig {
    RobotsConfig {
        user_agent: "RobotsBot/1.0".to_string(),
        allow_private_ips: true,
        ..Default::default()
    }
}

fn builder() -> HttpFetcherBuilder {
    HttpFetcherBuilder::new()
        .user_agent("PageBot/1.0")
        .allow_private_ips(true)
        .max_retries(0)
}

async fn fetch_two_pages(fetcher: &HttpFetcher, origin: &Origin) {
    let page = fetcher.fetch(&origin.url("/page")).await.unwrap();
    assert!(page.html.contains("/next"));
    let page = fetcher.fetch(&origin.url("/other")).await.unwrap();
    assert_eq!(page.status, 200);
    let err = fetcher.fetch(&origin.url("/private/x")).await.unwrap_err();
    assert!(err.to_string().contains("Robots.txt disallowed"), "{err}");
}

#[tokio::test]
async fn robots_and_pages_share_one_connection_and_one_lookup() {
    let origin = Origin::start().await;
    let fetcher = builder()
        .with_dns_cache()
        .build_with_robots(robots_config())
        .unwrap();

    fetch_two_pages(&fetcher, &origin).await;

    assert_eq!(
        origin.connections(),
        1,
        "robots.txt and pages, one connection"
    );
    let robots = origin.requests_for("/robots.txt");
    assert_eq!(robots.len(), 1, "robots.txt is fetched once");
    // The robots.txt request keeps its own identity on the shared client.
    assert!(
        robots[0].contains("user-agent: robotsbot/1.0"),
        "{}",
        robots[0]
    );
    assert!(robots[0].contains("accept: */*"), "{}", robots[0]);
    let pages = origin.requests_for("/page");
    assert!(pages[0].contains("user-agent: pagebot/1.0"), "{}", pages[0]);
    assert!(pages[0].contains("accept: text/html"), "{}", pages[0]);

    let dns = fetcher.dns_cache_stats().unwrap();
    assert_eq!(dns.misses, 1, "one lookup for the origin: {dns:?}");

    // The cache the fetcher checks is the one it hands out.
    let cache = fetcher.robots_cache();
    assert_eq!(
        cache.cached_crawl_delay(&origin.url("/x").url),
        Some(None),
        "cached by the fetch"
    );
    assert!(!cache.is_allowed(&origin.url("/private").url).await.unwrap());
    assert_eq!(origin.requests_for("/robots.txt").len(), 1);
}

#[tokio::test]
async fn a_separately_built_robots_cache_keeps_its_own_connection() {
    let origin = Origin::start().await;
    let fetcher = builder()
        .build(Arc::new(RobotsCache::new(robots_config()).unwrap()))
        .unwrap();

    fetch_two_pages(&fetcher, &origin).await;

    assert_eq!(origin.connections(), 2, "one for robots.txt, one for pages");
    assert_eq!(origin.requests_for("/robots.txt").len(), 1);
}

#[tokio::test]
async fn the_client_is_not_shared_when_it_would_change_the_robots_request() {
    // A fetcher-level header must not reach robots.txt: own client, but
    // still the fetcher's DNS cache.
    let origin = Origin::start().await;
    let fetcher = builder()
        .header("X-Tenant", "secret")
        .with_dns_cache()
        .build_with_robots(robots_config())
        .unwrap();
    fetch_two_pages(&fetcher, &origin).await;
    assert_eq!(origin.connections(), 2);
    let robots = origin.requests_for("/robots.txt");
    assert_eq!(robots.len(), 1);
    assert!(!robots[0].contains("x-tenant"), "{}", robots[0]);
    assert!(origin.requests_for("/page")[0].contains("x-tenant: secret"));
    let dns = fetcher.dns_cache_stats().unwrap();
    assert_eq!(dns.misses, 1, "{dns:?}");
    assert!(
        dns.hits >= 1,
        "the second client resolves from the cache: {dns:?}"
    );

    // A stricter robots SSRF policy than the fetcher's is kept: robots.txt
    // on a private address is refused (so no rules), never fetched through
    // the fetcher's more permissive client.
    let origin = Origin::start().await;
    let fetcher = builder()
        .build_with_robots(RobotsConfig {
            allow_private_ips: false,
            ..robots_config()
        })
        .unwrap();
    let page = fetcher.fetch(&origin.url("/private/x")).await.unwrap();
    assert_eq!(page.status, 200);
    assert!(origin.requests_for("/robots.txt").is_empty());

    // Redirects not followed by the fetcher: robots.txt still follows its
    // own, through its own client.
    let origin = Origin::start().await;
    let fetcher = builder()
        .follow_redirects(false)
        .build_with_robots(robots_config())
        .unwrap();
    fetch_two_pages(&fetcher, &origin).await;
    assert_eq!(origin.connections(), 2);
}
