//! Ack timing of the buffered Meilisearch writer (R2): a document's `Ack`
//! fires only after Meilisearch accepted (HTTP 202 + taskUid) the batch
//! containing it; a failed batch keeps its (document, ack) pairs for the
//! next flush.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use scrapix_core::{Ack, Document};
use scrapix_storage::MeilisearchStorageBuilder;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn task_accepted() -> ResponseTemplate {
    ResponseTemplate::new(202).set_body_json(serde_json::json!({
        "taskUid": 1, "indexUid": "i", "status": "enqueued", "type": "documentAdditionOrUpdate",
        "enqueuedAt": "2026-01-01T00:00:00Z"}))
}

fn counting_ack(counter: &Arc<AtomicUsize>) -> Ack {
    let counter = counter.clone();
    Ack::from_fn(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })
}

fn doc(n: usize) -> Document {
    Document::new(format!("https://a.test/{n}"), "a.test")
}

#[tokio::test]
async fn acks_only_after_meilisearch_accepts_the_batch() {
    let ms = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes/.*/documents$"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&ms)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes/.*/documents$"))
        .respond_with(task_accepted())
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "i")
        .batch_size(10)
        .connect()
        .unwrap();
    let acked = Arc::new(AtomicUsize::new(0));
    for n in 0..3 {
        storage
            .add_document_to_index(doc(n), "i", counting_ack(&acked))
            .await
            .unwrap();
    }
    assert_eq!(acked.load(Ordering::SeqCst), 0, "buffered, not yet sent");

    assert!(storage.flush().await.is_err());
    assert_eq!(
        acked.load(Ordering::SeqCst),
        0,
        "rejected batch is not acked"
    );
    assert_eq!(storage.pending_count(), 3, "rejected batch is kept");

    assert_eq!(storage.flush().await.unwrap(), 3);
    assert_eq!(acked.load(Ordering::SeqCst), 3);
    assert_eq!(storage.pending_count(), 0);
}

#[tokio::test]
async fn full_batch_is_sent_and_acked_by_add() {
    let ms = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes/.*/documents$"))
        .respond_with(task_accepted())
        .expect(1)
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "i")
        .batch_size(2)
        .connect()
        .unwrap();
    let acked = Arc::new(AtomicUsize::new(0));
    storage
        .add_document_to_index(doc(0), "idx", counting_ack(&acked))
        .await
        .unwrap();
    assert_eq!(acked.load(Ordering::SeqCst), 0);
    storage
        .add_document_to_index(doc(1), "idx", counting_ack(&acked))
        .await
        .unwrap();
    assert_eq!(acked.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn keep_settings_skips_settings_on_existing_index() {
    let ms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/indexes/existing$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uid": "existing", "primaryKey": "uid",
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z"})))
        .mount(&ms)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/existing/settings$"))
        .respond_with(task_accepted())
        .expect(0)
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "existing")
        .connect()
        .unwrap();
    let spec = scrapix_core::JobSpec {
        keep_settings: true,
        ..Default::default()
    };
    storage
        .configure_index("existing", &Default::default(), Some(&spec))
        .await;
}

#[tokio::test]
async fn job_settings_are_merged_over_feature_defaults() {
    let ms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/indexes/fresh$"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "message": "Index `fresh` not found.", "code": "index_not_found",
            "type": "invalid_request", "link": "https://docs.meilisearch.com/errors#index_not_found"})))
        .mount(&ms)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes$"))
        .respond_with(task_accepted())
        .expect(1)
        .mount(&ms)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/fresh/settings$"))
        .respond_with(task_accepted())
        .expect(1)
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "fresh")
        .primary_key("id")
        .connect()
        .unwrap();
    let spec = scrapix_core::JobSpec {
        keep_settings: true, // index does not exist yet → settings still applied
        index_settings: Some(scrapix_core::MeilisearchSettings {
            searchable_attributes: Some(vec!["title".into()]),
            filterable_attributes: Some(vec!["lang".into()]),
            stop_words: Some(vec!["the".into()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    storage
        .configure_index("fresh", &Default::default(), Some(&spec))
        .await;

    let requests = ms.received_requests().await.unwrap();
    let create = requests
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path() == "/indexes")
        .expect("index created");
    let body: serde_json::Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(body["primaryKey"], "id");
    let settings = requests
        .iter()
        .find(|r| r.method.as_str() == "PATCH")
        .expect("settings applied");
    let body: serde_json::Value = serde_json::from_slice(&settings.body).unwrap();
    assert_eq!(body["searchableAttributes"], serde_json::json!(["title"]));
    // Job's filterable list replaces the defaults, but `_crawl_job_id` stays
    // filterable: the Replace index strategy's stale-document cleanup needs it.
    let filterable: Vec<String> =
        serde_json::from_value(body["filterableAttributes"].clone()).unwrap();
    assert!(filterable.contains(&"lang".to_string()));
    assert!(filterable.contains(&"_crawl_job_id".to_string()));
    assert!(!filterable.contains(&"domain".to_string()));
    assert_eq!(body["stopWords"], serde_json::json!(["the"]));
}
