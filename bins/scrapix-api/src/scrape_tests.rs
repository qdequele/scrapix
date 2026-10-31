//! `/scrape` pipeline tests that need no browser: AI enrichment and its
//! billing, request shapes, and the HTTP status of the errors.

use super::*;
use scrapix_ai::AiClientConfig;
use scrapix_queue::ChannelBus;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::ai_access::NO_AI_MESSAGE;
use crate::results::test_support::{
    hosted_ai_state, hosted_ctx, lumen_answering, lumen_metadata, lumen_refusing, lumen_target,
    test_state_with_ai_and_lab, test_state_with_lab,
};

fn ctx() -> Option<AccountContext> {
    Some(AccountContext {
        account_id: "7f1c2a8e-0000-4000-8000-000000000001".into(),
        api_key_id: None,
        tier: "free".into(),
        user_role: None,
        limits: None,
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

/// Pre-v2 `credits` of every `usage.recorded` event (transition release).
fn credits(outbox: &lab_events::MemoryOutbox) -> Vec<i64> {
    outbox
        .events()
        .iter()
        .filter(|e| e.kind == "usage.recorded")
        .map(|e| e.data["credits"].as_i64().expect("usage carries credits"))
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
            serde_json::json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 0, "ai_extraction": 0,
                               "feature_pages": 1})
        ]
    );
    // f2ab8d2: scrape_credits([markdown], false, false) = 1.
    assert_eq!(credits(&outbox), vec![1]);
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
            serde_json::json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 0, "ai_extraction": 0,
                               "feature_pages": 1})
        ]
    );
    // f2ab8d2: scrape_credits([markdown], false, false) = 1.
    assert_eq!(credits(&outbox), vec![1]);
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
            serde_json::json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 1, "ai_extraction": 0,
                               "feature_pages": 1})
        ]
    );
    // AI adds no credits: scrape_credits([markdown], true, false) = 1.
    assert_eq!(credits(&outbox), vec![1]);
}

/// Transition release: the pre-v2 scrape formula counted feature formats
/// (base formats are free); that count is the page's `feature_pages`.
#[tokio::test]
async fn feature_formats_are_reported_as_feature_pages_with_pre_v2_credits() {
    let (_site, url) = site().await;
    let bus = ChannelBus::new();
    let (state, outbox) = test_state_with_lab(&bus);
    let req = request(serde_json::json!({
        "url": url, "formats": ["markdown", "links", "metadata", "html", "rawhtml", "content"]
    }));
    perform_scrape(&state, &ctx(), &req).await.unwrap();
    assert_eq!(charged(&outbox)[0]["feature_pages"], 3);
    // f2ab8d2: scrape_credits(3 feature formats, false, false) = 3.
    assert_eq!(credits(&outbox), vec![3]);
    lab_events::assert_contract_valid(&outbox.events());
}

// ---- hosted: the team's Lumen gateway, resolved through the Lab ----

#[tokio::test]
async fn scrape_ai_uses_the_team_key_and_requested_model() {
    let (_site, url) = site().await;
    let lumen = lumen_answering("A short summary.").await;
    let (_lab, state, _) = hosted_ai_state(Some(lumen_target(&lumen, "fg-s"))).await;
    let req = request(serde_json::json!({
        "url": url, "formats": ["markdown"], "ai": {"summary": true, "model": "m-1"}
    }));
    let res = perform_scrape(&state, &hosted_ctx(), &req).await.unwrap();
    assert_eq!(
        res.ai.and_then(|a| a.summary).as_deref(),
        Some("A short summary.")
    );
    assert!(res.warning.is_none(), "{:?}", res.warning);
    let calls = lumen.received_requests().await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].headers.get("authorization").unwrap(),
        "Bearer fg-s"
    );
    let body: serde_json::Value = calls[0].body_json().unwrap();
    assert_eq!(body["model"], "m-1");
    // A scrape has no job: only the feature is sent.
    assert_eq!(
        lumen_metadata(&calls[0]),
        serde_json::json!({"product": "scrapix", "feature": "ai_summary"})
    );
}

#[tokio::test]
async fn scrape_ai_credits_are_pages_only() {
    let formats = [ScrapeFormat::Markdown, ScrapeFormat::Links];
    assert_eq!(
        legacy_credits::scrape_credits(&formats, true, true),
        legacy_credits::scrape_credits(&formats, false, false)
    );

    let (_site, url) = site().await;
    let lumen = lumen_answering(r#"{"prices": [42]}"#).await;
    let (_lab, state, outbox) = hosted_ai_state(Some(lumen_target(&lumen, "fg-1"))).await;
    let req = request(serde_json::json!({
        "url": url, "formats": ["markdown"],
        "ai": {"summary": true, "extract": {"prompt": "prices"}}
    }));
    let res = perform_scrape(&state, &hosted_ctx(), &req).await.unwrap();
    assert!(res.warning.is_none(), "{:?}", res.warning);
    assert_eq!(lumen.received_requests().await.unwrap().len(), 2);
    // The delivered AI work is still counted in the units.
    assert_eq!(
        charged(&outbox),
        vec![
            serde_json::json!({"pages_http": 1, "pages_browser": 0, "ai_summary": 1, "ai_extraction": 1,
                               "feature_pages": 1})
        ]
    );
    assert_eq!(credits(&outbox), vec![1]);
    lab_events::assert_contract_valid(&outbox.events());
}

#[tokio::test]
async fn scrape_ai_gateway_error_is_a_warning() {
    let (_site, url) = site().await;
    for (status, code, message, expected) in [
        (
            404,
            "LM-2001",
            "model 'm-1' not found",
            "model 'm-1' not found".to_string(),
        ),
        (
            402,
            "LM-4001",
            "budget exceeded",
            billing::insufficient_credits_message(0),
        ),
    ] {
        let lumen = lumen_refusing(status, code, message).await;
        let (_lab, state, outbox) = hosted_ai_state(Some(lumen_target(&lumen, "fg-1"))).await;
        let req = request(serde_json::json!({
            "url": url, "formats": ["markdown"], "ai": {"summary": true, "model": "m-1"}
        }));
        let res = perform_scrape(&state, &hosted_ctx(), &req)
            .await
            .unwrap_or_else(|e| panic!("{code}: {}", e.error));
        assert!(res.success);
        assert!(res.ai.is_none());
        let warning = res.warning.expect("the failure is reported");
        assert!(warning.contains(&expected), "{code}: {warning}");
        // Not retried; the page is still charged.
        assert_eq!(lumen.received_requests().await.unwrap().len(), 1, "{code}");
        assert_eq!(credits(&outbox), vec![1], "{code}");
        assert_eq!(charged(&outbox)[0]["ai_summary"], 0, "{code}");
    }
}

#[tokio::test]
async fn scrape_ai_hosted_without_ai_is_503() {
    let (_site, url) = site().await;
    let (lab, state, outbox) = hosted_ai_state(None).await;
    let req = request(serde_json::json!({
        "url": url, "formats": ["markdown"], "ai": {"summary": true}
    }));
    let err = perform_scrape(&state, &hosted_ctx(), &req)
        .await
        .err()
        .unwrap_or_else(|| panic!("AI without a Lab target must be refused"));
    assert_eq!(err.code, "service_unavailable");
    assert_eq!(err.error, NO_AI_MESSAGE);
    assert_eq!(status_of(err), StatusCode::SERVICE_UNAVAILABLE);
    assert!(charged(&outbox).is_empty(), "nothing billed");
    assert_eq!(lab.ai_queries(), vec![None]);

    // No AI work asked: the account's AI is never looked up.
    for body in [
        serde_json::json!({"url": url, "formats": ["markdown"]}),
        serde_json::json!({"url": url, "formats": ["markdown"], "ai": {"model": "m-1"}}),
    ] {
        perform_scrape(&state, &hosted_ctx(), &request(body))
            .await
            .unwrap_or_else(|e| panic!("{}", e.error));
    }
    assert_eq!(lab.ai_queries(), vec![None]);
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

/// `/map` with `render_js` uses the browser: a plan without JS rendering is
/// refused before anything is fetched; one with it gets past the plan check.
#[tokio::test]
async fn map_with_render_js_follows_the_plans_js_rendering_limit() {
    use crate::lab_client::{
        testing::{FakeLab, INSTANCE_ID, SECRET},
        LabClient,
    };
    const ACCT: &str = "7f1c2a8e-0000-4000-8000-000000000001";
    let lab = FakeLab::start().await;
    lab.set_account(ACCT, FakeLab::identity(ACCT, "free", 10));
    let bus = ChannelBus::new();
    let (mut state, _outbox) = test_state_with_lab(&bus);
    Arc::get_mut(&mut state).unwrap().lab_api =
        Some(Arc::new(LabClient::new(&lab.url, INSTANCE_ID, SECRET)));
    let map = |limits: scrapix_auth::Limits| {
        let state = state.clone();
        async move {
            let account = AuthenticatedAccount {
                account_id: ACCT.into(),
                tier: "free".into(),
                api_key_id: None,
                role: None,
                limits: Some(limits),
            };
            let request: MapRequest = serde_json::from_value(
                serde_json::json!({"url": "https://a.test", "render_js": true}),
            )
            .unwrap();
            map_url(State(state), Some(Extension(account)), Json(request))
                .await
                .err()
                .unwrap()
        }
    };
    let no_js = scrapix_auth::Limits {
        concurrent_jobs: 1,
        rate_limit_rpm: 60,
        max_depth: 3,
        js_rendering: false,
    };
    let err = map(no_js.clone()).await;
    assert_eq!(err.code, "quota_exceeded", "{}", err.error);
    assert!(err.error.contains("JS rendering"), "{}", err.error);
    // Past the plan check, the test server has no browser.
    let err = map(scrapix_auth::Limits {
        js_rendering: true,
        ..no_js
    })
    .await;
    assert_eq!(err.code, "render_js_unavailable", "{}", err.error);
}
