//! `/scrape` pipeline tests that need no browser: AI enrichment and its
//! billing, request shapes, and the HTTP status of the errors.

use super::*;
use scrapix_ai::AiClientConfig;
use scrapix_queue::ChannelBus;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::results::test_support::{test_state_with_ai_and_lab, test_state_with_lab};

fn ctx() -> Option<AccountContext> {
    Some(AccountContext {
        account_id: "7f1c2a8e-0000-4000-8000-000000000001".into(),
        api_key_id: None,
        tier: "free".into(),
        user_role: None,
    })
}

/// A local page, addressed as `localhost` (the test fetcher allows it).
async fn site() -> (MockServer, String) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<html><head><title>T</title></head><body><main><h1>Hello</h1>\
             <p class=\"price\">42 euros</p><p class=\"price\">7 euros</p></main></body></html>",
            "text/html",
        ))
        .mount(&server)
        .await;
    let url = format!("{}/", server.uri().replace("127.0.0.1", "localhost"));
    (server, url)
}

/// An OpenAI-compatible provider: a completion, or (`ok == false`) a
/// 400 error (a 5xx would be retried by the client for minutes).
async fn llm(ok: bool) -> MockServer {
    let server = MockServer::start().await;
    let response = if ok {
        ResponseTemplate::new(200).set_body_json(completion())
    } else {
        ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "boom", "type": "invalid_request_error", "param": null, "code": null}
        }))
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

fn completion() -> serde_json::Value {
    serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "mock",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "A short summary." },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
    })
}

fn ai(llm: &MockServer) -> Arc<AiService> {
    let client = AiClient::new(AiClientConfig {
        api_key: "test".into(),
        provider: "openai".into(),
        base_url: Some(format!("{}/v1", llm.uri())),
        max_retries: 0,
        retry_delay_ms: 1,
        ..Default::default()
    })
    .unwrap();
    Arc::new(AiService::new(Arc::new(client)))
}

fn request(body: serde_json::Value) -> ScrapeRequest {
    serde_json::from_value(body).unwrap()
}

/// Units of every `usage.recorded` event.
fn charged(outbox: &lab_events::MemoryOutbox) -> Vec<serde_json::Value> {
    outbox
        .events()
        .iter()
        .filter(|e| e.kind == "usage.recorded")
        .map(|e| e.data["units"].clone())
        .collect()
}

fn status_of(e: ApiError) -> StatusCode {
    e.into_response().status()
}

#[tokio::test]
async fn ai_without_a_provider_is_503_and_bills_nothing() {
    let (_site, url) = site().await;
    let bus = ChannelBus::new();
    let (state, outbox) = test_state_with_lab(&bus);
    for ai in [
        serde_json::json!({"summary": true}),
        serde_json::json!({"extract": {"prompt": "prices"}}),
    ] {
        let req = request(serde_json::json!({"url": url, "formats": ["markdown"], "ai": ai}));
        let err = perform_scrape(&state, &ctx(), &req)
            .await
            .err()
            .unwrap_or_else(|| panic!("{ai} must be refused"));
        assert_eq!(err.code, "service_unavailable", "{}", err.error);
        assert!(err.error.contains("AI provider"), "{}", err.error);
        assert_eq!(status_of(err), StatusCode::SERVICE_UNAVAILABLE);
    }
    assert!(charged(&outbox).is_empty(), "nothing billed");
}

#[tokio::test]
async fn ai_options_asking_for_nothing_are_not_ai() {
    let (_site, url) = site().await;
    let bus = ChannelBus::new();
    let (state, outbox) = test_state_with_lab(&bus);
    let req = request(serde_json::json!({"url": url, "formats": ["markdown"], "ai": {}}));
    let res = perform_scrape(&state, &ctx(), &req).await.unwrap();
    assert!(res.success);
    assert_eq!(
        charged(&outbox),
        vec![
            serde_json::json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 0, "ai_extraction": 0})
        ]
    );
}

#[tokio::test]
async fn a_failed_ai_call_is_not_billed() {
    let (_site, url) = site().await;
    let provider = llm(false).await;
    let bus = ChannelBus::new();
    let (state, outbox) = test_state_with_ai_and_lab(&bus, Some(ai(&provider)));
    let req = request(serde_json::json!({
        "url": url, "formats": ["markdown"], "ai": {"summary": true}
    }));
    let res = perform_scrape(&state, &ctx(), &req).await.unwrap();
    assert!(res.success);
    assert!(res.ai.is_none());
    let warning = res.warning.expect("the failure is reported");
    assert!(warning.contains("not billed"), "{warning}");
    assert_eq!(
        charged(&outbox),
        vec![
            serde_json::json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 0, "ai_extraction": 0})
        ]
    );
}

#[tokio::test]
async fn a_successful_ai_summary_is_billed() {
    let (_site, url) = site().await;
    let provider = llm(true).await;
    let bus = ChannelBus::new();
    let (state, outbox) = test_state_with_ai_and_lab(&bus, Some(ai(&provider)));
    let req = request(serde_json::json!({
        "url": url, "formats": ["markdown"], "ai": {"summary": true}
    }));
    let res = perform_scrape(&state, &ctx(), &req).await.unwrap();
    assert_eq!(
        res.ai.and_then(|a| a.summary).as_deref(),
        Some("A short summary.")
    );
    assert!(res.warning.is_none());
    assert_eq!(
        charged(&outbox),
        vec![
            serde_json::json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 1, "ai_extraction": 0})
        ]
    );
}

#[tokio::test]
async fn extract_accepts_a_bare_selector_a_list_or_a_definition() {
    let (_site, url) = site().await;
    let bus = ChannelBus::new();
    let state = crate::results::test_support::test_state(&bus);
    let req = request(serde_json::json!({
        "url": url,
        "formats": ["markdown"],
        "extract": {
            "title": "h1",
            "prices": [".missing", ".price"],
            "first_price": {"selector": ".price"},
            "all_prices": {"selector": ".price", "mode": "list"}
        }
    }));
    let res = perform_scrape(&state, &None, &req).await.unwrap();
    let extract = serde_json::to_value(res.extract.unwrap()).unwrap();
    assert_eq!(extract["title"], "Hello");
    assert_eq!(extract["prices"], "42 euros");
    assert_eq!(extract["first_price"], "42 euros");
    assert_eq!(
        extract["all_prices"],
        serde_json::json!(["42 euros", "7 euros"])
    );
}

#[test]
fn extract_rejects_other_shapes() {
    for bad in [serde_json::json!(3), serde_json::json!({"multiple": true})] {
        let body = serde_json::json!({"url": "https://a.test", "extract": {"x": bad}});
        assert!(
            serde_json::from_value::<ScrapeRequest>(body).is_err(),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn a_browser_feature_without_a_browser_is_503() {
    let bus = ChannelBus::new();
    let state = crate::results::test_support::test_state(&bus);
    let req = request(serde_json::json!({"url": "https://a.test", "render_js": true}));
    let err = perform_scrape(&state, &None, &req).await.err().unwrap();
    assert_eq!(err.code, "render_js_unavailable");
    assert_eq!(status_of(err), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn an_unreachable_page_is_a_502() {
    let bus = ChannelBus::new();
    let state = crate::results::test_support::test_state(&bus);
    // Nothing listens on port 1.
    let req = request(serde_json::json!({"url": "http://localhost:1/"}));
    let err = perform_scrape(&state, &None, &req).await.err().unwrap();
    assert_eq!(err.code, "fetch_error");
    assert_eq!(status_of(err), StatusCode::BAD_GATEWAY);
}
