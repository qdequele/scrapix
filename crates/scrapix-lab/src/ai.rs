//! Calling an account's Lumen gateway with the key the Lab serves
//! ([`LabClient::ai`](crate::LabClient::ai)): one AI service per key, and
//! the one-retry rule for a key Lumen refuses. Shared by the API server and
//! the content worker.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use scrapix_ai::{
    ai_gateway_code, gateway_codes, AiClient, AiClientConfig, AiClientError, AiService,
    AiUsageSender, ExtractionConfig, SummaryConfig,
};
use scrapix_core::ScrapixError;

use crate::AiTarget;

/// Past this many keys the hosted service cache starts over.
const MAX_HOSTED_CLIENTS: usize = 1024;

/// One [`AiService`] per Lab-minted key, so calls for a team share a client
/// (connection pool, concurrency limit). Keyed by `key_id`; an entry whose
/// target changed (URL, models, secret) is rebuilt.
#[derive(Default)]
pub struct HostedAiClients {
    clients: parking_lot::Mutex<HashMap<String, (AiTarget, Arc<AiService>)>>,
    /// Where every built client reports its calls (the same channel as the
    /// env client); `None` when calls are not tracked.
    usage_tx: Option<AiUsageSender>,
}

impl HostedAiClients {
    pub fn new(usage_tx: Option<AiUsageSender>) -> Self {
        Self {
            clients: Default::default(),
            usage_tx,
        }
    }

    /// The service for `target`, built on first use.
    pub fn get_or_build(&self, target: &AiTarget) -> Result<Arc<AiService>, AiClientError> {
        let mut clients = self.clients.lock();
        if let Some((cached, service)) = clients.get(&target.key_id) {
            if cached == target {
                return Ok(service.clone());
            }
        }
        let service = Arc::new(build_service(target, self.usage_tx.clone())?);
        if clients.len() >= MAX_HOSTED_CLIENTS {
            clients.clear();
        }
        clients.insert(target.key_id.clone(), (target.clone(), service.clone()));
        Ok(service)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.clients.lock().len()
    }
}

/// An OpenAI-compatible service pointed at the target's Lumen gateway, with
/// the Lab's default model for extraction and summaries.
fn build_service(
    target: &AiTarget,
    usage_tx: Option<AiUsageSender>,
) -> Result<AiService, AiClientError> {
    let mut client = AiClient::new(AiClientConfig {
        api_key: target.api_key.clone(),
        provider: "openai".to_string(),
        // The Lab may send `…/v1/`: keep request URLs free of `//`.
        base_url: Some(target.base_url.trim_end_matches('/').to_string()),
        max_retries: 2,
        lumen_metadata: true,
        ..Default::default()
    })?;
    if let Some(tx) = usage_tx {
        client = client.with_usage_sender(tx);
    }
    Ok(AiService::minimal(Arc::new(client))
        .with_extraction_config(ExtractionConfig {
            model: target.default_model.clone(),
            ..Default::default()
        })
        .with_summary_config(SummaryConfig {
            model: target.default_model.clone(),
            ..Default::default()
        }))
}

/// Run an AI call with `ai`; when Lumen refuses its key (`LM-4004`) and the
/// key has an id, ask `re_resolve` (given the refused `key_id`) for a
/// replacement and run the call once more with it. Exactly one re-resolve
/// and one retry; when no replacement comes back (`None`), the original
/// gateway error is returned.
pub async fn retry_on_key_refused<A, T, F, Fut, R, RFut>(
    ai: A,
    key_id: Option<String>,
    call: F,
    re_resolve: R,
) -> Result<T, ScrapixError>
where
    F: Fn(A) -> Fut,
    Fut: Future<Output = Result<T, ScrapixError>>,
    R: FnOnce(String) -> RFut,
    RFut: Future<Output = Option<A>>,
{
    let err = match call(ai).await {
        Ok(value) => return Ok(value),
        Err(err) => err,
    };
    let Some(rejected) =
        key_id.filter(|_| ai_gateway_code(&err) == Some(gateway_codes::KEY_REFUSED))
    else {
        return Err(err);
    };
    match re_resolve(rejected).await {
        Some(next) => call(next).await,
        None => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn gateway(code: &str) -> ScrapixError {
        ScrapixError::AiGateway {
            code: code.into(),
            message: "refused".into(),
        }
    }

    #[test]
    fn the_client_cache_is_bounded() {
        let clients = HostedAiClients::default();
        for i in 0..=MAX_HOSTED_CLIENTS {
            clients
                .get_or_build(&AiTarget {
                    base_url: "http://lumen/v1".into(),
                    api_key: format!("fg-{i}"),
                    key_id: format!("k-{i}"),
                    default_model: "m".into(),
                    vision_model: "v".into(),
                })
                .unwrap();
        }
        assert!(clients.len() <= MAX_HOSTED_CLIENTS);
    }

    #[tokio::test]
    async fn a_refused_key_is_replaced_once() {
        let calls = AtomicUsize::new(0);
        let resolves = AtomicUsize::new(0);
        // Key 1 is refused, key 2 works.
        let result = retry_on_key_refused(
            1,
            Some("k-1".to_string()),
            |key| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if key == 1 {
                        Err(gateway(gateway_codes::KEY_REFUSED))
                    } else {
                        Ok(key)
                    }
                }
            },
            |rejected| {
                resolves.fetch_add(1, Ordering::SeqCst);
                assert_eq!(rejected, "k-1");
                async { Some(2) }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(resolves.load(Ordering::SeqCst), 1);

        // Every key refused: one retry, and the gateway error surfaces.
        let calls = AtomicUsize::new(0);
        let err = retry_on_key_refused(
            1,
            Some("k-1".to_string()),
            |_: i32| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err::<i32, _>(gateway(gateway_codes::KEY_REFUSED)) }
            },
            |_| async { Some(2) },
        )
        .await
        .unwrap_err();
        assert_eq!(ai_gateway_code(&err), Some(gateway_codes::KEY_REFUSED));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn other_errors_and_missing_replacements_are_not_retried() {
        // Not a key refusal: no re-resolve.
        let err = retry_on_key_refused(
            1,
            Some("k-1".to_string()),
            |_: i32| async { Err::<i32, _>(gateway(gateway_codes::BUDGET_EXCEEDED)) },
            |_| async { panic!("budget errors are not re-resolved") },
        )
        .await
        .unwrap_err();
        assert_eq!(ai_gateway_code(&err), Some(gateway_codes::BUDGET_EXCEEDED));

        // No key id (standalone): no re-resolve.
        let err = retry_on_key_refused(
            1,
            None,
            |_: i32| async { Err::<i32, _>(gateway(gateway_codes::KEY_REFUSED)) },
            |_| async { panic!("a key without an id is not re-resolved") },
        )
        .await
        .unwrap_err();
        assert_eq!(ai_gateway_code(&err), Some(gateway_codes::KEY_REFUSED));

        // No replacement: the original error.
        let calls = AtomicUsize::new(0);
        let err = retry_on_key_refused(
            1,
            Some("k-1".to_string()),
            |_: i32| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err::<i32, _>(gateway(gateway_codes::KEY_REFUSED)) }
            },
            |_| async { None },
        )
        .await
        .unwrap_err();
        assert_eq!(ai_gateway_code(&err), Some(gateway_codes::KEY_REFUSED));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
