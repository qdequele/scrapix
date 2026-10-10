//! Billing Accuracy Tests (P0)
//!
//! content_length and account_id flow through the pipeline for billing.
//! If content_length is wrong, customers are over/under-charged.
//! If account_id is lost, usage can't be attributed.

use scrapix_core::CrawlUrl;
use scrapix_queue::{CrawlEvent, RawPageMessage, UrlMessage};

// ============================================================================
// Content length tracking through messages
// ============================================================================

#[test]
fn test_content_length_propagates_in_raw_page_message() {
    let html = "<html><body><p>Hello World</p></body></html>";
    let content_length = html.len() as u64;

    let msg = RawPageMessage {
        url: "https://example.com".to_string(),
        final_url: "https://example.com".to_string(),
        status: 200,
        html: html.to_string(),
        content_type: Some("text/html".to_string()),
        content_length,
        js_rendered: false,
        fetched_at: 1704067200000,
        fetch_duration_ms: 100,
        job_id: "job-1".to_string(),
        index_uid: "idx".to_string(),
        account_id: Some("acct_123".to_string()),
        source: None,
        message_id: "msg-1".to_string(),
        etag: None,
        last_modified: None,
        meilisearch_url: None,
        meilisearch_api_key: None,
        features: None,
        job: None,
        url_message_id: "url-msg-1".to_string(),
    };

    let json = serde_json::to_string(&msg).unwrap();
    let d: RawPageMessage = serde_json::from_str(&json).unwrap();

    assert_eq!(d.content_length, content_length);
    assert_eq!(d.content_length, html.len() as u64);
}

#[test]
fn test_content_length_zero_default_in_raw_page_message() {
    // Older messages without content_length field should default to 0
    let json = r#"{
        "url": "https://example.com",
        "final_url": "https://example.com",
        "status": 200,
        "html": "<html></html>",
        "content_type": null,
        "js_rendered": false,
        "fetched_at": 1704067200000,
        "fetch_duration_ms": 100,
        "job_id": "j",
        "index_uid": "i",
        "message_id": "m"
    }"#;

    let msg: RawPageMessage = serde_json::from_str(json).unwrap();
    assert_eq!(msg.content_length, 0);
}

#[test]
fn test_content_length_in_crawl_event() {
    let event = CrawlEvent::page_crawled_with_billing(
        "job-1",
        Some("acct_123".to_string()),
        "https://example.com",
        200,
        54321,
        150,
    );

    let json = serde_json::to_string(&event).unwrap();
    let d: CrawlEvent = serde_json::from_str(&json).unwrap();

    match d {
        CrawlEvent::PageCrawled { content_length, .. } => {
            assert_eq!(content_length, 54321);
        }
        _ => panic!("Wrong variant"),
    }
}

#[test]
fn test_content_length_zero_in_page_crawled_without_billing() {
    // Non-billing event defaults content_length to 0
    let event = CrawlEvent::page_crawled("job-1", "https://example.com", 200, 150);

    match event {
        CrawlEvent::PageCrawled { content_length, .. } => {
            assert_eq!(content_length, 0);
        }
        _ => panic!("Wrong variant"),
    }
}

// ============================================================================
// Account ID propagation
// ============================================================================

#[test]
fn test_account_id_propagates_through_url_message() {
    let msg = UrlMessage::with_account(
        CrawlUrl::seed("https://example.com"),
        "job-1",
        "idx",
        "acct_billing",
    );

    assert_eq!(msg.account_id, Some("acct_billing".to_string()));

    let json = serde_json::to_string(&msg).unwrap();
    let d: UrlMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(d.account_id, Some("acct_billing".to_string()));
}

#[test]
fn test_account_id_none_for_anonymous_crawls() {
    let msg = UrlMessage::new(CrawlUrl::seed("https://example.com"), "job-1", "idx");

    assert!(msg.account_id.is_none());

    let json = serde_json::to_string(&msg).unwrap();
    let d: UrlMessage = serde_json::from_str(&json).unwrap();
    assert!(d.account_id.is_none());
}

#[test]
fn test_account_id_in_job_started_event() {
    let event = CrawlEvent::job_started_with_account(
        "job-1",
        "index-1",
        "acct_123",
        vec!["https://example.com".to_string()],
    );

    match event {
        CrawlEvent::JobStarted { account_id, .. } => {
            assert_eq!(account_id, Some("acct_123".to_string()));
        }
        _ => panic!("Wrong variant"),
    }
}

#[test]
fn test_account_id_none_in_events_without_account() {
    let event = CrawlEvent::page_crawled("job-1", "https://example.com", 200, 100);

    match event {
        CrawlEvent::PageCrawled { account_id, .. } => {
            assert!(account_id.is_none());
        }
        _ => panic!("Wrong variant"),
    }
}
