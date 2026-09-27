//! Runs the shared `FrontierStore` conformance suite against
//! `RedisFrontierStore`.
//!
//! Needs a live Redis (or DragonflyDB): set `SCRAPIX_TEST_REDIS_URL`, e.g.
//! `SCRAPIX_TEST_REDIS_URL=redis://localhost:6379` (`just infra` provides
//! DragonflyDB there). Without it the test prints a skip note and passes.
//! Every case gets its own random key prefix, so runs never collide and
//! never touch non-test keys; the keys each case created are deleted once
//! the suite finishes.

#![cfg(all(feature = "conformance", feature = "redis-store"))]

use std::sync::{Arc, Mutex};

use scrapix_frontier::store::{conformance, FrontierStore, RedisFrontierStore};

#[tokio::test]
async fn redis_store_conforms() {
    let Ok(url) = std::env::var("SCRAPIX_TEST_REDIS_URL") else {
        eprintln!("SCRAPIX_TEST_REDIS_URL not set; skipping Redis conformance");
        return;
    };
    let prefixes = Arc::new(Mutex::new(Vec::new()));
    conformance::run_all_async(|| {
        let url = url.clone();
        let prefix = format!("test-{}", uuid::Uuid::new_v4());
        prefixes.lock().unwrap().push(prefix.clone());
        async move {
            Arc::new(RedisFrontierStore::new(&url, &prefix).await.unwrap())
                as Arc<dyn FrontierStore>
        }
    })
    .await;

    // Clean up every key the cases created (released jobs' `meta` would
    // otherwise linger for their retention window, unreleased ones forever).
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let prefixes = prefixes.lock().unwrap().clone();
    for prefix in prefixes {
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(format!("{prefix}:*"))
            .query_async(&mut conn)
            .await
            .unwrap();
        if !keys.is_empty() {
            let _: () = redis::cmd("DEL")
                .arg(&keys)
                .query_async(&mut conn)
                .await
                .unwrap();
        }
    }
}
