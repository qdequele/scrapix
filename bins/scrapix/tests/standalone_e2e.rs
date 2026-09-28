//! Standalone e2e: `scrapix all` with SQLite + a real Meilisearch.
//!
//!   docker run -d --rm --name meili-e2e -p 7799:7700 \
//!     -e MEILI_MASTER_KEY=e2e-master-key getmeili/meilisearch:v1.31.0
//!   MEILISEARCH_URL=http://127.0.0.1:7799 MEILISEARCH_API_KEY=e2e-master-key \
//!     cargo test -p scrapix --test standalone_e2e -- --nocapture
//!   docker stop meili-e2e
//!
//! Without `MEILISEARCH_URL` set, the test prints "skipped" and passes.
//!
//! Crawls a local fixture site with `scrapix all`, searches the indexed
//! pages via `/search`, then restarts the engine on the same SQLite file
//! and checks the job's history survived the restart.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const KEY: &str = "e2e-admin-key-0123456789";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_engine(port: u16, db: &str, meili: &str, meili_key: Option<&str>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scrapix"));
    cmd.args(["all", "--port", &port.to_string()])
        .env("SCRAPIX_MODE", "standalone")
        .env("SCRAPIX_ADMIN_KEY", KEY)
        .env("DATABASE_URL", db)
        .env("MEILISEARCH_URL", meili)
        // The fixture site is served on 127.0.0.1, so SSRF protection must
        // be off for the crawler to be allowed to fetch it.
        .env("ALLOW_PRIVATE_IPS", "true")
        .env("DOMAIN_DELAY_MS", "0")
        .env("JOB_COMPLETION_GRACE_MS", "500")
        .env_remove("KAFKA_BROKERS")
        .env_remove("REDIS_URL")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(k) = meili_key {
        cmd.env("MEILISEARCH_API_KEY", k);
    }
    cmd.spawn().expect("spawn scrapix all")
}

/// SIGTERM → graceful shutdown (final flush), then wait. `scrapix all`
/// drains and flushes on SIGTERM but may keep waiting for Ctrl+C
/// afterwards, so fall back to SIGKILL once the deadline passes — the
/// flush already happened by then.
fn stop_child(mut child: Child) {
    let _ = Command::new("kill").arg(child.id().to_string()).status();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Owns a spawned `scrapix all` process and guarantees it is stopped even
/// if a `panic!`/`assert!` unwinds through the test before the explicit
/// restart point — `std::process::Child` does not kill on `Drop`, so an
/// unguarded `Child` would leak the engine (holding the SQLite file and
/// the Meilisearch connection open) on any failing assertion.
///
/// The restart point still calls `stop()` explicitly (consuming the
/// guard) rather than relying on shadowing + `Drop`: shadowing a `let`
/// binding does not drop the old value until the end of the enclosing
/// scope, so an unguarded shadow would leave the old and new engine
/// running concurrently across the restart, breaking the
/// flush-then-restart ordering the test depends on.
struct EngineGuard(Option<Child>);

impl EngineGuard {
    fn spawn(port: u16, db: &str, meili: &str, meili_key: Option<&str>) -> Self {
        Self(Some(spawn_engine(port, db, meili, meili_key)))
    }

    /// Explicit, deterministic stop — used at the restart point so the
    /// prior engine has fully exited (and flushed) before the next one
    /// starts.
    fn stop(mut self) {
        if let Some(child) = self.0.take() {
            stop_child(child);
        }
    }
}

impl Drop for EngineGuard {
    fn drop(&mut self) {
        // Safety net for panicking paths: `stop()` above already took the
        // child in the normal case, so this is a no-op then.
        if let Some(child) = self.0.take() {
            stop_child(child);
        }
    }
}

async fn wait_healthy(c: &reqwest::Client, base: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if c.get(format!("{base}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("engine not healthy at {base}");
}

/// Serves the fixture on `127.0.0.1` and returns a `localhost`-based URL:
/// the engine's crawler refuses raw IP-literal seed URLs outright
/// (`reject_ip_host`, `crates/scrapix-crawler/src/safe_client.rs`) even
/// with `ALLOW_PRIVATE_IPS=true` — that flag only relaxes the check on a
/// *resolved hostname*'s IP (`SafeResolver`), so the seed must be a
/// hostname, not `127.0.0.1` itself.
///
/// `localhost` can resolve to `::1` before `127.0.0.1` (observed on this
/// host's resolver order), so the fixture is served on both loopback
/// addresses at the same port to avoid depending on which one a given
/// environment's resolver returns first.
async fn fixture_site() -> String {
    use axum::{response::Html, routing::get, Router};
    fn app() -> axum::Router {
        // Content worker's readability extractor drops anything under
        // `min_content_length` (100 chars, `crates/scrapix-parser/src/html.rs`)
        // before it ever reaches Meilisearch — a one-line page is silently
        // never indexed (crawled, but `pages_indexed`/`documents_sent` stay
        // 0), so each page carries several real sentences of body text.
        Router::new()
            .route(
                "/",
                get(|| async {
                    Html(
                        r#"<html><head><title>Home</title></head><body>
                            <h1>Standalone zebra</h1>
                            <p>Welcome to the standalone zebra fixture site, built for the
                            end-to-end test of the Scrapix standalone engine. This page exists
                            only so the crawler has real paragraph text to extract and index.</p>
                            <p>A zebra is a striped, horse-like animal native to Africa, and this
                            fixture repeats the word zebra so the search step below has something
                            distinctive to query for.</p>
                            <a href="/about">About</a>
                        </body></html>"#,
                    )
                }),
            )
            .route(
                "/about",
                get(|| async {
                    Html(
                        r#"<html><head><title>About</title></head><body>
                            <h1>About this zebra fixture</h1>
                            <p>The quick standalone zebra page describes the second page of the
                            crawl fixture, reachable from the home page's single outbound link.</p>
                            <p>It exists purely to give the standalone end-to-end test a small,
                            self-contained site to crawl, extract, and index without depending on
                            the public internet.</p>
                        </body></html>"#,
                    )
                }),
            )
    }

    let listener_v4 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener_v4.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener_v4, app()).await.unwrap() });

    if let Ok(listener_v6) = tokio::net::TcpListener::bind(format!("[::1]:{port}")).await {
        tokio::spawn(async move { axum::serve(listener_v6, app()).await.unwrap() });
    }

    format!("http://localhost:{port}")
}

#[tokio::test(flavor = "multi_thread")]
async fn crawl_search_restart_history() {
    let Ok(meili) = std::env::var("MEILISEARCH_URL") else {
        eprintln!("skipped: MEILISEARCH_URL not set");
        return;
    };
    let meili_key = std::env::var("MEILISEARCH_API_KEY").ok();
    let dir = tempfile::tempdir().unwrap();
    let db = format!("sqlite://{}", dir.path().join("scrapix.db").display());
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let site = fixture_site().await;
    let c = reqwest::Client::new();

    let engine = EngineGuard::spawn(port, &db, &meili, meili_key.as_deref());
    wait_healthy(&c, &base).await;

    // Unauthenticated calls are rejected.
    assert_eq!(
        c.get(format!("{base}/jobs")).send().await.unwrap().status(),
        401
    );

    // `index_uid` is intentionally left unset: the engine auto-derives it
    // from `start_urls[0]` (`scrapix_core::url_to_index_uid`), the same
    // function `/search` uses to derive the index from `request.url` below
    // — leaving both sides to compute it independently from the identical
    // URL string guarantees they land on the same index.
    let created: Value = c
        .post(format!("{base}/crawl"))
        .header("X-API-Key", KEY)
        .json(&json!({
            "start_urls": [format!("{site}/")],
            "max_pages": 5,
            "max_depth": 2
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let job_id = created["job_id"].as_str().expect("job_id").to_string();

    // Wait for completion.
    let deadline = Instant::now() + Duration::from_secs(120);
    let final_status = loop {
        let s: Value = c
            .get(format!("{base}/job/{job_id}/status"))
            .header("X-API-Key", KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let status = s["status"].as_str().unwrap_or_default().to_string();
        if ["completed", "failed", "cancelled"].contains(&status.as_str()) {
            break s;
        }
        assert!(Instant::now() < deadline, "job did not finish: {s}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(final_status["status"], "completed", "{final_status}");
    let crawled = final_status["pages_crawled"].as_u64().unwrap();
    assert!(crawled >= 2, "{final_status}");

    // /search derives the index uid from `url` the same way `/crawl` did
    // above (from an unset `index_uid`) — pass the same site URL.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r: Value = c
            .post(format!("{base}/search"))
            .header("X-API-Key", KEY)
            .json(&json!({ "url": format!("{site}/"), "q": "zebra" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if r["hits"].as_array().is_some_and(|h| !h.is_empty()) {
            break;
        }
        assert!(Instant::now() < deadline, "no search hits: {r}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Restart on the same SQLite file: history survives. `stop()` blocks
    // until the old engine has exited before the new one starts, keeping
    // the flush-then-restart ordering deterministic (see `EngineGuard`).
    engine.stop();
    let engine = EngineGuard::spawn(port, &db, &meili, meili_key.as_deref());
    wait_healthy(&c, &base).await;
    let jobs: Value = c
        .get(format!("{base}/jobs"))
        .header("X-API-Key", KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // `GET /jobs` returns a bare array; `.get("jobs")` on a JSON array
    // always misses, so this falls through to `jobs` itself either way.
    let jobs = jobs
        .get("jobs")
        .unwrap_or(&jobs)
        .as_array()
        .cloned()
        .unwrap_or_default();
    let found = jobs
        .iter()
        .find(|j| j["job_id"] == job_id.as_str())
        .expect("job survives restart");
    assert_eq!(found["status"], "completed");
    assert_eq!(found["pages_crawled"].as_u64().unwrap(), crawled);
    engine.stop();
}
