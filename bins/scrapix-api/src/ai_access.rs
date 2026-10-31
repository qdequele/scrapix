//! Which AI service an account's LLM calls use. Standalone: the env-configured
//! service. Hosted: the team's Lumen gateway, with the key the Lab serves
//! (`GET /internal/accounts/{id}/ai`), one service per key.

use std::future::Future;
use std::sync::Arc;

use scrapix_ai::AiService;
use scrapix_core::ScrapixError;

use crate::lab_client::{log_lab_error, LabError};
use crate::{AccountContext, ApiError, AppState};

/// Refusal for AI work when the account has no AI service.
pub(crate) const NO_AI_MESSAGE: &str = "AI is not available for this account";

/// The AI service for one request, and what is needed to call it.
#[derive(Clone)]
pub(crate) struct ResolvedAi {
    pub service: Arc<AiService>,
    /// Hosted: the Lab's vision model (OCR). `None` in standalone, where the
    /// OCR engine picks its own.
    pub vision_model: Option<String>,
    /// Hosted: the Lab's id for the key (sent back as `rejected_key_id` when
    /// Lumen refuses it). `None` in standalone.
    pub key_id: Option<String>,
}

impl std::fmt::Debug for ResolvedAi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedAi")
            .field("vision_model", &self.vision_model)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// The AI service for a request. Standalone: the env service (`None` when
/// no provider is configured). Hosted: the account's Lumen target from the
/// Lab (`None` when it has none); `rejected_key_id` is the key Lumen just
/// refused, so the Lab replaces it.
pub(crate) async fn resolve_ai(
    state: &AppState,
    ctx: Option<&AccountContext>,
    rejected_key_id: Option<&str>,
) -> Result<Option<ResolvedAi>, ApiError> {
    let Some(lab) = state.lab_api.as_ref() else {
        return Ok(state.ai_service.clone().map(|service| ResolvedAi {
            service,
            vision_model: None,
            key_id: None,
        }));
    };
    // Hosted requests are always authenticated; without an account there is
    // no team, so no AI (never the operator's env service).
    let Some(ctx) = ctx else {
        return Ok(None);
    };
    let target = match lab.ai(&ctx.account_id, rejected_key_id).await {
        Ok(Some(target)) => target,
        Ok(None) => return Ok(None),
        Err(e @ LabError::Unavailable(_)) => {
            log_lab_error(&e, "AI lookup");
            return Err(ApiError::new(
                "AI configuration unavailable, retry shortly",
                "service_unavailable",
            )
            .with_retry_after(5));
        }
        Err(e) => {
            log_lab_error(&e, "AI lookup");
            return Err(ApiError::new("AI configuration error", "internal_error"));
        }
    };
    let service = state.hosted_ai.get_or_build(&target).map_err(|e| {
        tracing::error!(error = %e, key_id = %target.key_id, "cannot build the Lumen AI client");
        ApiError::new("AI configuration error", "internal_error")
    })?;
    Ok(Some(ResolvedAi {
        service,
        vision_model: Some(target.vision_model),
        key_id: Some(target.key_id),
    }))
}

/// Run an AI call; when Lumen refuses the key (`LM-4004`), ask the Lab for a
/// new one and run it once more (`scrapix_lab::retry_on_key_refused`). If no
/// new key comes back, the original gateway error is returned.
pub(crate) async fn with_key_retry<T, F, Fut>(
    state: &AppState,
    ctx: Option<&AccountContext>,
    ai: ResolvedAi,
    call: F,
) -> Result<T, ScrapixError>
where
    F: Fn(ResolvedAi) -> Fut,
    Fut: Future<Output = Result<T, ScrapixError>>,
{
    let key_id = ai.key_id.clone();
    scrapix_lab::retry_on_key_refused(ai, key_id, call, |rejected| async move {
        match resolve_ai(state, ctx, Some(&rejected)).await {
            Ok(next) => next,
            Err(e) => {
                tracing::warn!(error = %e.error, key_id = %rejected, "no replacement for a refused AI key");
                None
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab_client::testing::FakeLab;
    use crate::lab_client::AiTarget;
    use crate::results::test_support::{
        test_state_with_ai, test_state_with_lab_ai, test_state_with_lab_ai_usage,
    };
    use axum::response::IntoResponse;
    use scrapix_ai::{gateway_codes, AiClient, AiClientConfig};
    use scrapix_queue::ChannelBus;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ACCOUNT: &str = "acct-1";

    fn ctx() -> AccountContext {
        AccountContext {
            account_id: ACCOUNT.into(),
            api_key_id: None,
            tier: "pro".into(),
            user_role: None,
            limits: None,
        }
    }

    /// A Lab answer pointing at `lumen` (with the trailing `/` the Lab may send).
    fn target(lumen: &MockServer, key_id: &str, api_key: &str) -> AiTarget {
        AiTarget {
            base_url: format!("{}/v1/", lumen.uri()),
            api_key: api_key.into(),
            key_id: key_id.into(),
            default_model: "gpt-4o-mini".into(),
            vision_model: "gpt-4o".into(),
        }
    }

    fn completion() -> serde_json::Value {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "{\"title\": \"Hello\"}"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })
    }

    fn key_refused() -> ResponseTemplate {
        ResponseTemplate::new(401).set_body_json(json!({
            "error": {"code": "LM-4004", "message": "key refused", "type": "lumen_error"}
        }))
    }

    /// Lumen answering 200 to `Bearer <ok>` and `LM-4004` to `Bearer <refused>`.
    async fn lumen(ok: &[&str], refused: &[&str]) -> MockServer {
        let server = MockServer::start().await;
        for key in ok {
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .and(header("authorization", format!("Bearer {key}").as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(completion()))
                .mount(&server)
                .await;
        }
        for key in refused {
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .and(header("authorization", format!("Bearer {key}").as_str()))
                .respond_with(key_refused())
                .mount(&server)
                .await;
        }
        server
    }

    async fn extract(ai: ResolvedAi) -> Result<scrapix_ai::ExtractionResult, ScrapixError> {
        ai.service
            .extract("A page about things.", "the title")
            .await
    }

    #[tokio::test]
    async fn standalone_returns_the_env_service() {
        let service = Arc::new(AiService::new(Arc::new(
            AiClient::new(AiClientConfig {
                api_key: "env-key".into(),
                ..Default::default()
            })
            .unwrap(),
        )));
        let state = test_state_with_ai(&ChannelBus::new(), Some(service.clone()));

        let ai = resolve_ai(&state, Some(&ctx()), None)
            .await
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&ai.service, &service));
        assert_eq!(ai.vision_model, None);
        assert_eq!(ai.key_id, None);

        let none = test_state_with_ai(&ChannelBus::new(), None);
        assert!(resolve_ai(&none, Some(&ctx()), None)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn hosted_builds_one_service_per_key() {
        let lumen = lumen(&["fg-1", "fg-2"], &[]).await;
        let lab = FakeLab::start().await;
        lab.set_ai(ACCOUNT, Some(target(&lumen, "k-1", "fg-1")));
        lab.rotate_ai(ACCOUNT, target(&lumen, "k-2", "fg-2"));
        let (state, _) = test_state_with_lab_ai(&ChannelBus::new(), &lab);

        let first = resolve_ai(&state, Some(&ctx()), None)
            .await
            .unwrap()
            .unwrap();
        let again = resolve_ai(&state, Some(&ctx()), None)
            .await
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first.service, &again.service));
        assert_eq!(first.key_id.as_deref(), Some("k-1"));
        assert_eq!(first.vision_model.as_deref(), Some("gpt-4o"));

        // The Lab rotates the key: a new service, with the new key.
        let rotated = resolve_ai(&state, Some(&ctx()), Some("k-1"))
            .await
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&first.service, &rotated.service));
        assert_eq!(rotated.key_id.as_deref(), Some("k-2"));
    }

    #[tokio::test]
    async fn hosted_service_calls_lumen_with_the_key_and_default_model() {
        let lumen = lumen(&["fg-s"], &[]).await;
        let lab = FakeLab::start().await;
        lab.set_ai(ACCOUNT, Some(target(&lumen, "k-s", "fg-s")));
        let (state, _) = test_state_with_lab_ai(&ChannelBus::new(), &lab);

        let ai = resolve_ai(&state, Some(&ctx()), None)
            .await
            .unwrap()
            .unwrap();
        extract(ai).await.unwrap();

        let requests = lumen.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        // No `//chat/completions`: the Lab's trailing `/` is trimmed.
        assert_eq!(requests[0].url.path(), "/v1/chat/completions");
        assert_eq!(
            requests[0].headers.get("authorization").unwrap(),
            "Bearer fg-s"
        );
        assert!(requests[0].headers.get("x-lumen-metadata").is_some());
        let body: serde_json::Value = requests[0].body_json().unwrap();
        assert_eq!(body["model"], "gpt-4o-mini");
    }

    #[tokio::test]
    async fn hosted_calls_are_recorded_for_clickhouse() {
        let lumen = lumen(&["fg-u"], &[]).await;
        let lab = FakeLab::start().await;
        lab.set_ai(ACCOUNT, Some(target(&lumen, "k-u", "fg-u")));
        let (state, mut usage) = test_state_with_lab_ai_usage(&ChannelBus::new(), &lab);

        let ai = resolve_ai(&state, Some(&ctx()), None)
            .await
            .unwrap()
            .unwrap();
        let attribution = scrapix_ai::AiUsageContext {
            job_id: "j-1".into(),
            account_id: Some(ACCOUNT.into()),
            feature: "extract".into(),
            url: String::new(),
        };
        scrapix_ai::AI_USAGE_CONTEXT
            .scope(attribution.clone(), extract(ai))
            .await
            .unwrap();

        // Same channel as the env client: drained into ClickHouse `ai_usage_events`.
        let event = usage.try_recv().unwrap();
        assert_eq!(event.provider, "openai");
        assert_eq!(event.model, "gpt-4o-mini");
        assert_eq!(event.total_tokens, 15);
        assert_eq!(event.context, Some(attribution));
        assert!(usage.try_recv().is_err());
    }

    #[tokio::test]
    async fn hosted_without_lab_target_is_none() {
        let lab = FakeLab::start().await;
        let (state, _) = test_state_with_lab_ai(&ChannelBus::new(), &lab);

        assert!(resolve_ai(&state, Some(&ctx()), None)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn lab_down_is_503_retry_after_5() {
        let lab = FakeLab::start().await;
        lab.set_down(true);
        let (state, _) = test_state_with_lab_ai(&ChannelBus::new(), &lab);

        let err = resolve_ai(&state, Some(&ctx()), None).await.unwrap_err();
        assert_eq!(err.code, "service_unavailable");
        let resp = err.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers()["retry-after"], "5");
    }

    #[tokio::test]
    async fn key_refused_reresolves_once() {
        // The refused key is replaced and the call retried with the new one.
        let lumen_ok = lumen(&["fg-2"], &["fg-1"]).await;
        let lab = FakeLab::start().await;
        lab.set_ai(ACCOUNT, Some(target(&lumen_ok, "k-1", "fg-1")));
        lab.rotate_ai(ACCOUNT, target(&lumen_ok, "k-2", "fg-2"));
        let (state, _) = test_state_with_lab_ai(&ChannelBus::new(), &lab);
        let ctx = ctx();

        let ai = resolve_ai(&state, Some(&ctx), None).await.unwrap().unwrap();
        let result = with_key_retry(&state, Some(&ctx), ai, extract).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(lab.ai_queries(), vec![None, Some("k-1".to_string())]);
        assert_eq!(lumen_ok.received_requests().await.unwrap().len(), 2);

        // Both keys refused: one retry only, and the gateway error surfaces.
        let lumen_no = lumen(&[], &["fg-1", "fg-2"]).await;
        let lab = FakeLab::start().await;
        lab.set_ai(ACCOUNT, Some(target(&lumen_no, "k-1", "fg-1")));
        lab.rotate_ai(ACCOUNT, target(&lumen_no, "k-2", "fg-2"));
        let (state, _) = test_state_with_lab_ai(&ChannelBus::new(), &lab);

        let ai = resolve_ai(&state, Some(&ctx), None).await.unwrap().unwrap();
        let err = with_key_retry(&state, Some(&ctx), ai, extract)
            .await
            .unwrap_err();
        assert_eq!(
            scrapix_ai::ai_gateway_code(&err),
            Some(gateway_codes::KEY_REFUSED)
        );
        assert_eq!(lumen_no.received_requests().await.unwrap().len(), 2);
        assert_eq!(lab.ai_queries(), vec![None, Some("k-1".to_string())]);
    }

    #[tokio::test]
    async fn key_refused_with_lab_down_keeps_the_gateway_error() {
        let lumen = lumen(&[], &["fg-1"]).await;
        let lab = FakeLab::start().await;
        lab.set_ai(ACCOUNT, Some(target(&lumen, "k-1", "fg-1")));
        let (state, _) = test_state_with_lab_ai(&ChannelBus::new(), &lab);
        let ctx = ctx();

        let ai = resolve_ai(&state, Some(&ctx), None).await.unwrap().unwrap();
        lab.set_down(true);
        let err = with_key_retry(&state, Some(&ctx), ai, extract)
            .await
            .unwrap_err();
        assert_eq!(
            scrapix_ai::ai_gateway_code(&err),
            Some(gateway_codes::KEY_REFUSED)
        );
        assert_eq!(lumen.received_requests().await.unwrap().len(), 1);
    }
}
