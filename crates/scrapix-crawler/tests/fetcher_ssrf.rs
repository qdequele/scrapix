use std::sync::Arc;
use std::time::{Duration, Instant};

use scrapix_core::CrawlUrl;
use scrapix_crawler::{HttpFetcherBuilder, RobotsCache, RobotsConfig};
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

// `is_public_ip`'s classification (including the extended non-public ranges:
// benchmarking, reserved, IETF protocol assignments, site-local, NAT64,
// 6to4, IPv4-compatible) is covered by `safe_dns::tests::classifies_ips` —
// kept there only, not duplicated here.

/// A refused fetch must fail fast and never retry: retrying a refusal (raw
/// IP, non-public redirect target, or a hostname resolving only to
/// non-public addresses) just re-runs the same doomed resolution/redirect
/// and burns through `max_retries` × backoff (about 3.5s per URL with the
/// default retry config) for a request that can never succeed. Both tests
/// below assert on wall-clock time as the primary signal that no retry loop
/// ran.
const NO_RETRY_BUDGET: Duration = Duration::from_millis(500);

#[tokio::test]
async fn refuses_hostname_resolving_to_loopback() {
    let server = MockServer::start().await;
    // The refusal happens during DNS resolution, before reqwest ever opens a
    // TCP connection — `SafeResolver` returns an error straight from the
    // `Resolve` future, so hyper-util's connector never reaches the "connect
    // to an address" step at all. That means the mock server receives zero
    // requests for this URL, on every attempt; `.expect(1)` doesn't apply
    // here the way it does for the redirect test below (where the *first*
    // hop does complete). The proof that this doesn't retry is `elapsed`
    // staying under `NO_RETRY_BUDGET` instead.
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let f = HttpFetcherBuilder::new().build(robots()).unwrap(); // allow_private_ips = false
    let u = server.uri().replace("127.0.0.1", "localhost");

    let start = Instant::now();
    let err = f.fetch(&CrawlUrl::seed(u)).await.unwrap_err();
    let elapsed = start.elapsed();

    assert!(err.to_string().contains("non-public"), "{err}");
    assert!(
        elapsed < NO_RETRY_BUDGET,
        "refusal must not go through the retry loop, took {elapsed:?}"
    );
}

#[tokio::test]
async fn refuses_redirect_to_raw_private_ip() {
    let server = MockServer::start().await;
    Mock::given(path("/r"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "http://169.254.169.254/latest/meta-data"),
        )
        // The first hop succeeds and is fetched exactly once — the refused
        // redirect target must not cause `fetch_inner` to retry the whole
        // request (which would re-request "/r" on every attempt).
        .expect(1)
        .mount(&server)
        .await;
    // allow the first hop (test server is local) but still refuse raw-IP redirect targets
    let f = HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .build(robots())
        .unwrap();
    let u = format!("{}/r", server.uri().replace("127.0.0.1", "localhost"));

    let start = Instant::now();
    let err = f.fetch(&CrawlUrl::seed(u)).await.unwrap_err();
    let elapsed = start.elapsed();

    assert!(err.to_string().to_lowercase().contains("redirect"), "{err}");
    assert!(
        elapsed < NO_RETRY_BUDGET,
        "refusal must not go through the retry loop, took {elapsed:?}"
    );
}

// NOT TESTED (documented, not skipped silently — see fix-round-1 report):
// a domain redirect (e.g. 302 to `http://some-other-hostname/...`) with
// `allow_private_ips = false` should be refused if that hostname resolves
// privately, exactly like the raw-IP redirect case above but going through
// `SafeResolver`'s hostname-resolution check instead of the redirect
// policy's raw-IP check. This can't be exercised with wiremock alone in an
// offline test: the first hop must succeed (so the fetcher needs a
// *publicly* resolving hostname to reach it) while the redirect target must
// resolve *privately* — but wiremock only ever listens on a loopback
// address, and there is no real DNS available in this sandboxed test
// environment to make one test hostname resolve publicly while another
// resolves to the same loopback server. Doing this would require either
// live internet DNS or editing `/etc/hosts`, neither of which is available
// here, so this scenario is left untested.

/// With `with_dns_cache()`, a single fetch must resolve through
/// `CachingDnsResolver` exactly once (one cache miss, zero hits) — proving
/// reqwest's `SafeResolver` and the crawler's own DNS cache share the same
/// lookup rather than each resolving independently (the "DNS cache pre-
/// resolves on its own while reqwest resolves a second time" bug this task
/// fixes).
#[tokio::test]
async fn dns_cache_is_used_by_reqwest_without_double_lookup() {
    let server = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let f = HttpFetcherBuilder::new()
        .allow_private_ips(true)
        .with_dns_cache()
        .build(robots())
        .unwrap();
    let u = server.uri().replace("127.0.0.1", "localhost");

    f.fetch(&CrawlUrl::seed(u)).await.unwrap();

    let stats = f.dns_cache_stats().unwrap();
    assert_eq!(stats.misses, 1, "expected exactly one DNS lookup");
    assert_eq!(stats.hits, 0);
}
