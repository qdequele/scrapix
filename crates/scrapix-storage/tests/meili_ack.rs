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

/// Mocks an existing index `existing` whose filterable attributes are
/// `filterable`; PATCH /settings must never be called.
async fn existing_index(filterable: serde_json::Value) -> MockServer {
    let ms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/indexes/existing$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uid": "existing", "primaryKey": "uid",
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z"})))
        .mount(&ms)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/indexes/existing/settings/filterable-attributes$",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(filterable))
        .mount(&ms)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/existing/settings$"))
        .respond_with(task_accepted())
        .expect(0)
        .mount(&ms)
        .await;
    ms
}

fn keep_settings() -> scrapix_core::JobSpec {
    scrapix_core::JobSpec {
        keep_settings: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn keep_settings_skips_settings_on_existing_index() {
    let ms = existing_index(serde_json::json!(["domain", "_crawl_job_id"])).await;
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/indexes/existing/settings/filterable-attributes$",
        ))
        .respond_with(task_accepted())
        .expect(0)
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "existing")
        .connect()
        .unwrap();
    assert!(
        storage
            .configure_index("existing", &Default::default(), Some(&keep_settings()))
            .await
    );
}

#[tokio::test]
async fn keep_settings_still_makes_crawl_job_id_filterable() {
    let ms = existing_index(serde_json::json!(["domain"])).await;
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/indexes/existing/settings/filterable-attributes$",
        ))
        .respond_with(task_accepted())
        .expect(1)
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "existing")
        .connect()
        .unwrap();
    assert!(
        storage
            .configure_index("existing", &Default::default(), Some(&keep_settings()))
            .await
    );

    let requests = ms.received_requests().await.unwrap();
    let put = requests
        .iter()
        .find(|r| r.method.as_str() == "PUT")
        .expect("filterable attributes updated");
    let body: serde_json::Value = serde_json::from_slice(&put.body).unwrap();
    assert_eq!(body, serde_json::json!(["domain", "_crawl_job_id"]));
}

#[tokio::test]
async fn permanently_refused_batch_is_rejected_not_retried() {
    let ms = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes/.*/documents$"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "message": "The provided API key is invalid.", "code": "invalid_api_key",
            "type": "auth", "link": "https://docs.meilisearch.com/errors#invalid_api_key"})))
        .expect(1)
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "i")
        .batch_size(10)
        .connect()
        .unwrap();
    let acked = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(AtomicUsize::new(0));
    for n in 0..2 {
        let r = rejected.clone();
        let ack = scrapix_storage::DocAck::new(counting_ack(&acked)).on_reject(move |_| {
            r.fetch_add(1, Ordering::SeqCst);
        });
        storage
            .add_document_to_index(doc(n), "i", ack)
            .await
            .unwrap();
    }

    assert!(storage.flush().await.is_err());
    assert_eq!(acked.load(Ordering::SeqCst), 0);
    assert_eq!(rejected.load(Ordering::SeqCst), 2);
    assert_eq!(storage.pending_count(), 0, "refused batch is not kept");
    // Nothing left to send: the next flush makes no request (expect(1)).
    assert_eq!(storage.flush().await.unwrap(), 0);
}

#[tokio::test]
async fn backpressure_wait_is_time_boxed() {
    let ms = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes/.*/documents$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "i")
        .batch_size(1)
        .backpressure_timeout(std::time::Duration::from_millis(300))
        .connect()
        .unwrap();
    let acked = Arc::new(AtomicUsize::new(0));
    // 4 * batch_size documents fill the buffer (every flush fails retryably).
    for n in 0..4 {
        storage
            .add_document_to_index(doc(n), "i", counting_ack(&acked))
            .await
            .unwrap();
    }
    assert_eq!(storage.pending_count(), 4);

    let reason = Arc::new(parking_lot::Mutex::new(None::<String>));
    let r = reason.clone();
    let ack = scrapix_storage::DocAck::new(counting_ack(&acked)).on_reject(move |why| {
        *r.lock() = Some(why);
    });
    let started = std::time::Instant::now();
    assert!(storage
        .add_document_to_index(doc(9), "i", ack)
        .await
        .is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        reason.lock().as_deref(),
        Some("meilisearch backpressure timeout")
    );
    assert_eq!(acked.load(Ordering::SeqCst), 0);
    assert_eq!(
        storage.pending_count(),
        4,
        "earlier documents stay buffered"
    );
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
    assert_eq!(body["pagination"]["maxTotalHits"], 10000);
}

/// Settings JSON with synonyms keys in the given order.
fn spec_with_synonyms(keys: &[&str]) -> scrapix_core::JobSpec {
    let synonyms: Vec<String> = keys
        .iter()
        .map(|k| format!("\"{k}\": [\"{k}-alt\", \"{k}-other\"]"))
        .collect();
    let json = format!(
        r#"{{"index_settings": {{"synonyms": {{{}}}, "stop_words": ["a"]}}}}"#,
        synonyms.join(",")
    );
    serde_json::from_str(&json).unwrap()
}

#[tokio::test]
async fn settings_fingerprint_is_independent_of_map_order() {
    let ms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/indexes/syn$"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "message": "not found", "code": "index_not_found",
            "type": "invalid_request", "link": "https://docs.meilisearch.com"})))
        .mount(&ms)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes$"))
        .respond_with(task_accepted())
        .mount(&ms)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/syn/settings$"))
        .respond_with(task_accepted())
        .expect(1)
        .mount(&ms)
        .await;

    let storage = MeilisearchStorageBuilder::new(ms.uri(), "syn")
        .connect()
        .unwrap();
    let keys = [
        "car", "phone", "house", "tv", "laptop", "shoe", "bike", "book",
    ];
    let mut reversed = keys;
    reversed.reverse();
    let features = Default::default();
    let first = spec_with_synonyms(&keys);
    let fp = storage.settings_fingerprint("syn", &features, Some(&first));
    // Many independent deserializations (fresh HashMap seeds each time).
    for i in 0..20 {
        let spec = spec_with_synonyms(if i % 2 == 0 { &keys } else { &reversed });
        assert_eq!(
            storage.settings_fingerprint("syn", &features, Some(&spec)),
            fp
        );
    }

    storage
        .ensure_configured("syn", &features, Some(&first))
        .await;
    storage
        .ensure_configured("syn", &features, Some(&spec_with_synonyms(&reversed)))
        .await;
    // PATCH expect(1) is verified when the mock server drops.
}
