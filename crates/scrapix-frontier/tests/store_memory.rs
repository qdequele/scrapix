//! Runs the shared `FrontierStore` conformance suite against
//! `MemoryFrontierStore`.

#![cfg(feature = "conformance")]

#[tokio::test]
async fn memory_store_conforms() {
    scrapix_frontier::store::conformance::run_all(|| {
        std::sync::Arc::new(scrapix_frontier::store::MemoryFrontierStore::default())
    })
    .await;
}
