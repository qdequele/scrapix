//! Replace-strategy stale cleanup is scoped to the crawl: two saved configs
//! writing to one index (a blog crawl and a docs crawl) must not delete each
//! other's documents.

use scrapix_core::CrawlScope;
use scrapix_storage::meilisearch::MeilisearchStorage;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const INDEX: &str = "my-crawl";
const JOB: &str = "3703d937-3cf9-41ca-9aee-3df7f4c41fd4";

fn task_accepted(uid: u32) -> ResponseTemplate {
    ResponseTemplate::new(202).set_body_json(serde_json::json!({
        "taskUid": uid, "indexUid": INDEX, "status": "enqueued",
        "type": "documentDeletion", "enqueuedAt": "2026-01-01T00:00:00Z"}))
}

fn task_succeeded(uid: u32) -> serde_json::Value {
    serde_json::json!({
        "uid": uid, "indexUid": INDEX, "status": "succeeded",
        "type": "documentDeletion", "details": null, "canceledBy": null, "error": null,
        "duration": "PT0.001S", "enqueuedAt": "2026-01-01T00:00:00Z",
        "startedAt": "2026-01-01T00:00:00Z", "finishedAt": "2026-01-01T00:00:00Z"})
}

/// A Meilisearch with `filterable` attributes on `my-crawl` whose unscoped
/// documents (no `_crawl_scope`) are `unscoped`.
async fn meilisearch(filterable: serde_json::Value, unscoped: serde_json::Value) -> MockServer {
    let ms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"results": [], "total": 0, "limit": 20, "from": null, "next": null}),
        ))
        .mount(&ms)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/tasks/\d+$"))
        .respond_with(|req: &Request| {
            let uid: u32 = req.url.path().rsplit('/').next().unwrap().parse().unwrap();
            ResponseTemplate::new(200).set_body_json(task_succeeded(uid))
        })
        .mount(&ms)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/indexes/{INDEX}/settings/filterable-attributes"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(filterable))
        .mount(&ms)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "/indexes/{INDEX}/settings/filterable-attributes"
        )))
        .respond_with(task_accepted(1))
        .mount(&ms)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/indexes/{INDEX}/documents/delete")))
        .respond_with(task_accepted(2))
        .mount(&ms)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/indexes/{INDEX}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uid": INDEX, "primaryKey": "uid",
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z"})))
        .mount(&ms)
        .await;
    let total = unscoped.as_array().map_or(0, Vec::len);
    Mock::given(method("POST"))
        .and(path(format!("/indexes/{INDEX}/documents/fetch")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": unscoped, "limit": 1000, "offset": 0, "total": total})))
        .mount(&ms)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/indexes/{INDEX}/documents/delete-batch")))
        .respond_with(task_accepted(3))
        .mount(&ms)
        .await;
    ms
}

async fn bodies(ms: &MockServer, m: &str, p: &str) -> Vec<serde_json::Value> {
    ms.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == m && r.url.path() == p)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn deletes_only_this_scopes_stale_documents() {
    let blog = CrawlScope::from_start_urls(&["https://www.meilisearch.com/blog"]);
    // Documents indexed before `_crawl_scope` existed: the blog's old pages
    // and the docs crawl's pages, side by side in `my-crawl`.
    let unscoped = serde_json::json!([
        {"uid": "old-blog-post", "url": "https://www.meilisearch.com/blog/old-post"},
        {"uid": "docs-page", "url": "https://www.meilisearch.com/docs/learn/getting_started"},
        {"uid": "blogging", "url": "https://www.meilisearch.com/blogging"},
        {"uid": "manual", "title": "added by hand, no url"},
    ]);
    let ms = meilisearch(serde_json::json!(["_crawl_job_id"]), unscoped).await;

    MeilisearchStorage::delete_stale_documents(&ms.uri(), None, INDEX, JOB, &blog)
        .await
        .unwrap();

    // `_crawl_scope` was made filterable before filtering on it.
    let puts = bodies(
        &ms,
        "PUT",
        &format!("/indexes/{INDEX}/settings/filterable-attributes"),
    )
    .await;
    assert_eq!(
        puts,
        vec![serde_json::json!(["_crawl_job_id", "_crawl_scope"])]
    );

    // The filtered delete names this crawl's scope, not the whole index.
    let deletes = bodies(&ms, "POST", &format!("/indexes/{INDEX}/documents/delete")).await;
    assert_eq!(
        deletes,
        vec![serde_json::json!({
            "filter": format!("_crawl_scope = \"{}\" AND _crawl_job_id != \"{JOB}\"", blog.id())
        })]
    );

    // Unscoped documents are listed with the job excluded, and only the
    // blog's own old page is deleted.
    let fetches = bodies(&ms, "POST", &format!("/indexes/{INDEX}/documents/fetch")).await;
    assert_eq!(
        fetches[0]["filter"],
        format!("_crawl_scope NOT EXISTS AND _crawl_job_id != \"{JOB}\"")
    );
    let batch = bodies(
        &ms,
        "POST",
        &format!("/indexes/{INDEX}/documents/delete-batch"),
    )
    .await;
    assert_eq!(batch, vec![serde_json::json!(["old-blog-post"])]);
}

#[tokio::test]
async fn already_filterable_and_nothing_unscoped_in_scope() {
    let docs = CrawlScope::from_start_urls(&["https://www.meilisearch.com/docs"]);
    let unscoped = serde_json::json!([
        {"uid": "blog-page", "url": "https://www.meilisearch.com/blog"},
    ]);
    let ms = meilisearch(
        serde_json::json!(["url", "_crawl_job_id", "_crawl_scope"]),
        unscoped,
    )
    .await;

    MeilisearchStorage::delete_stale_documents(&ms.uri(), None, INDEX, JOB, &docs)
        .await
        .unwrap();

    let requests = ms.received_requests().await.unwrap();
    assert!(
        !requests.iter().any(|r| r.method.as_str() == "PUT"),
        "filterable attributes left alone"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.url.path().ends_with("/documents/delete-batch")),
        "the blog page of another crawl is kept"
    );
}

/// A Replace job counts its own documents (`_crawl_job_id`), not the
/// whole index another crawl shares.
#[tokio::test]
async fn job_document_count_filters_on_the_job() {
    let ms = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/indexes/{INDEX}/documents/fetch")))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["filter"], format!("_crawl_job_id = \"{JOB}\""));
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{"_crawl_job_id": JOB}], "limit": 1, "offset": 0, "total": 7}))
        })
        .mount(&ms)
        .await;
    let n = MeilisearchStorage::count_job_documents(&ms.uri(), None, INDEX, JOB).await;
    assert_eq!(n, Some(7));
}

/// Failed tasks of earlier jobs on the index are not reported: only
/// document tasks enqueued since the job started.
#[tokio::test]
async fn failed_tasks_are_scoped_to_the_job_start() {
    let ms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks"))
        .respond_with(|req: &Request| {
            let query = req.url.query().unwrap_or_default().to_string();
            assert!(query.contains("afterEnqueuedAt="), "{query}");
            assert!(query.contains("types=documentAdditionOrUpdate"), "{query}");
            assert!(query.contains("statuses=failed"), "{query}");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "total": 3, "limit": 20, "from": null, "next": null}))
        })
        .mount(&ms)
        .await;
    let since = time::OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
    let failed = MeilisearchStorage::log_failed_tasks(&ms.uri(), None, INDEX, since).await;
    assert_eq!(failed, 3, "the total, not one page of results");
}
