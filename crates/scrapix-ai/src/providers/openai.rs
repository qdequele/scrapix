//! OpenAI provider using the async-openai crate

use async_openai::{
    config::{Config, OpenAIConfig},
    error::{ApiError, OpenAIError},
    types::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
        CreateChatCompletionRequest, CreateChatCompletionRequestArgs, CreateChatCompletionResponse,
        ImageDetail, ImageUrl,
    },
    Client as OpenAIClient,
};
use async_trait::async_trait;
use backoff::ExponentialBackoff;
use reqwest::header::{HeaderMap, HeaderValue};
use secrecy::SecretString;
use std::time::Duration;

use super::{ChatResponse, ImageInput, LlmProvider, Message, MessageRole};
use crate::client::{AiClientError, AiUsageContext, AI_USAGE_CONTEXT};

/// Header the Lumen gateway reads call attribution from.
const LUMEN_METADATA_HEADER: &str = "x-lumen-metadata";
/// Header a Lumen gateway reports a call's settled cost in (micro-USD).
const LUMEN_COST_HEADER: &str = "x-lumen-cost-micro-usd";
/// Longest string value in the metadata header, in bytes.
const METADATA_VALUE_MAX_BYTES: usize = 256;
/// Longest metadata header, in bytes.
const METADATA_MAX_BYTES: usize = 4096;
/// How long async-openai keeps retrying a rate-limited or failing request.
const RETRY_MAX_ELAPSED: Duration = Duration::from_secs(60);

/// OpenAI provider backed by async-openai
pub struct OpenAiProvider {
    config: OpenAIConfig,
    http: reqwest::Client,
    /// Send `x-lumen-metadata` (only to a Lumen gateway, see
    /// [`OpenAiProvider::with_lumen_metadata`]).
    lumen_metadata: bool,
}

/// An [`OpenAIConfig`] that also sends the Lumen metadata header, if any.
#[derive(Clone)]
struct MetadataConfig {
    inner: OpenAIConfig,
    metadata: Option<HeaderValue>,
}

impl Config for MetadataConfig {
    fn headers(&self) -> HeaderMap {
        let mut headers = self.inner.headers();
        if let Some(metadata) = &self.metadata {
            headers.insert(LUMEN_METADATA_HEADER, metadata.clone());
        }
        headers
    }

    fn url(&self, path: &str) -> String {
        self.inner.url(path)
    }

    fn query(&self) -> Vec<(&str, &str)> {
        self.inner.query()
    }

    fn api_base(&self) -> &str {
        self.inner.api_base()
    }

    fn api_key(&self) -> &SecretString {
        self.inner.api_key()
    }
}

/// Cut `s` to at most `max_bytes`, backing up to a char boundary.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Escape everything outside printable ASCII as `\uXXXX`, so the JSON is a
/// valid header value whatever the strings in it. Only called on serialized
/// JSON, where such characters can only sit inside strings.
fn escape_non_ascii(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let mut units = [0u16; 2];
    for c in json.chars() {
        if c.is_ascii() && c != '\u{7f}' {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// The `x-lumen-metadata` value for a call: a flat JSON object with
/// `product`, plus `feature` and `job_id` when the caller scoped them in
/// [`AI_USAGE_CONTEXT`]. The account and the page URL are never sent.
fn lumen_metadata(ctx: Option<&AiUsageContext>) -> String {
    let mut metadata = serde_json::Map::new();
    metadata.insert("product".to_string(), "scrapix".into());
    if let Some(ctx) = ctx {
        for (key, value) in [("feature", &ctx.feature), ("job_id", &ctx.job_id)] {
            if !value.is_empty() {
                metadata.insert(
                    key.to_string(),
                    truncate_utf8(value, METADATA_VALUE_MAX_BYTES).into(),
                );
            }
        }
    }
    let json = escape_non_ascii(&serde_json::Value::Object(metadata).to_string());
    if json.len() <= METADATA_MAX_BYTES {
        json
    } else {
        // Defensive: the capped values escape to well under the limit.
        r#"{"product":"scrapix"}"#.to_string()
    }
}

impl OpenAiProvider {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        let mut config = OpenAIConfig::new().with_api_key(api_key);
        if let Some(url) = base_url {
            config = config.with_api_base(url);
        }
        Self {
            config,
            http: reqwest::Client::new(),
            lumen_metadata: false,
        }
    }

    /// Attach `x-lumen-metadata` to every call: set for a Lumen gateway
    /// (hosted), never for a provider's own API (standalone), which gets no
    /// job ids.
    pub fn with_lumen_metadata(mut self, enabled: bool) -> Self {
        self.lumen_metadata = enabled;
        self
    }

    /// A client for one call, carrying that call's metadata header.
    fn client(&self) -> OpenAIClient<MetadataConfig> {
        let metadata = self.lumen_metadata.then(|| {
            let ctx = AI_USAGE_CONTEXT.try_with(|c| c.clone()).ok();
            HeaderValue::from_str(&lumen_metadata(ctx.as_ref()))
                .unwrap_or_else(|_| HeaderValue::from_static(r#"{"product":"scrapix"}"#))
        });
        OpenAIClient::with_config(MetadataConfig {
            inner: self.config.clone(),
            metadata,
        })
        .with_http_client(self.http.clone())
        .with_backoff(ExponentialBackoff {
            max_elapsed_time: Some(RETRY_MAX_ELAPSED),
            ..Default::default()
        })
    }

    fn convert_messages(
        messages: &[Message],
    ) -> Result<Vec<ChatCompletionRequestMessage>, AiClientError> {
        messages
            .iter()
            .map(|m| match m.role {
                MessageRole::System => Ok(ChatCompletionRequestMessage::System(
                    ChatCompletionRequestSystemMessageArgs::default()
                        .content(m.content.as_str())
                        .build()
                        .map_err(|e| AiClientError::Config(e.to_string()))?,
                )),
                MessageRole::User => Ok(ChatCompletionRequestMessage::User(
                    ChatCompletionRequestUserMessageArgs::default()
                        .content(m.content.as_str())
                        .build()
                        .map_err(|e| AiClientError::Config(e.to_string()))?,
                )),
                MessageRole::Assistant => Ok(ChatCompletionRequestMessage::Assistant(
                    ChatCompletionRequestAssistantMessageArgs::default()
                        .content(m.content.as_str())
                        .build()
                        .map_err(|e| AiClientError::Config(e.to_string()))?,
                )),
            })
            .collect()
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat(
        &self,
        messages: Vec<Message>,
        model: &str,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
    ) -> Result<ChatResponse, AiClientError> {
        let openai_messages = Self::convert_messages(&messages)?;

        let mut request_builder = CreateChatCompletionRequestArgs::default();
        request_builder.model(model).messages(openai_messages);

        if let Some(tokens) = max_tokens {
            // `max_tokens` is rejected by current reasoning models.
            request_builder.max_completion_tokens(tokens);
        }
        if let Some(temp) = temperature {
            request_builder.temperature(temp);
        }

        let request = request_builder
            .build()
            .map_err(|e| AiClientError::Config(e.to_string()))?;

        self.send(request).await
    }

    async fn vision(
        &self,
        system: &str,
        prompt: &str,
        image: &ImageInput,
        model: &str,
        max_tokens: Option<u32>,
    ) -> Result<ChatResponse, AiClientError> {
        let config_err = |e: async_openai::error::OpenAIError| AiClientError::Config(e.to_string());
        let messages = vec![
            ChatCompletionRequestMessage::System(
                ChatCompletionRequestSystemMessageArgs::default()
                    .content(system)
                    .build()
                    .map_err(config_err)?,
            ),
            ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessageArgs::default()
                    .content(ChatCompletionRequestUserMessageContent::Array(vec![
                        ChatCompletionRequestUserMessageContentPart::ImageUrl(
                            ChatCompletionRequestMessageContentPartImage {
                                image_url: ImageUrl {
                                    url: image.data_url(),
                                    detail: Some(ImageDetail::High),
                                },
                            },
                        ),
                        ChatCompletionRequestUserMessageContentPart::Text(
                            ChatCompletionRequestMessageContentPartText {
                                text: prompt.to_string(),
                            },
                        ),
                    ]))
                    .build()
                    .map_err(config_err)?,
            ),
        ];

        let mut request_builder = CreateChatCompletionRequestArgs::default();
        request_builder.model(model).messages(messages);
        if let Some(tokens) = max_tokens {
            request_builder.max_completion_tokens(tokens);
        }
        let request = request_builder.build().map_err(config_err)?;
        self.send(request).await
    }
}

impl OpenAiProvider {
    async fn send(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<ChatResponse, AiClientError> {
        let (response, cost_micro_usd) = if self.lumen_metadata {
            self.send_to_lumen(&request).await?
        } else {
            (self.client().chat().create(request).await?, None)
        };

        let choice = response
            .choices
            .first()
            .ok_or(AiClientError::EmptyResponse)?;

        let content = choice.message.content.clone().unwrap_or_default();
        let usage = response.usage.as_ref();

        Ok(ChatResponse {
            content,
            model: response.model.clone(),
            prompt_tokens: usage.map(|u| u.prompt_tokens).unwrap_or(0),
            completion_tokens: usage.map(|u| u.completion_tokens).unwrap_or(0),
            total_tokens: usage.map(|u| u.total_tokens).unwrap_or(0),
            finish_reason: choice.finish_reason.as_ref().map(|r| format!("{:?}", r)),
            cost_micro_usd,
        })
    }

    /// The chat call to a Lumen gateway, made by hand so the response's
    /// `x-lumen-cost-micro-usd` header can be read (async-openai drops
    /// headers). Same headers, same retries and same errors as
    /// async-openai's own `create`: 5xx and rate-limit 429 are retried within
    /// [`RETRY_MAX_ELAPSED`], any other error body is an
    /// [`OpenAIError::ApiError`].
    async fn send_to_lumen(
        &self,
        request: &CreateChatCompletionRequest,
    ) -> Result<(CreateChatCompletionResponse, Option<u64>), AiClientError> {
        let client = self.client();
        let config = client.config().clone();
        let url = config.url("/chat/completions");
        let backoff = ExponentialBackoff {
            max_elapsed_time: Some(RETRY_MAX_ELAPSED),
            ..Default::default()
        };
        let (bytes, cost) = backoff::future::retry(backoff, || async {
            let response = self
                .http
                .post(&url)
                .query(&config.query())
                .headers(config.headers())
                .json(request)
                .send()
                .await
                .map_err(|e| backoff::Error::Permanent(OpenAIError::Reqwest(e)))?;
            let status = response.status();
            let cost = response
                .headers()
                .get(LUMEN_COST_HEADER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let bytes = response
                .bytes()
                .await
                .map_err(|e| backoff::Error::Permanent(OpenAIError::Reqwest(e)))?;
            if status.is_success() {
                return Ok((bytes, cost));
            }
            Err(api_error(status, &bytes))
        })
        .await?;
        let response = serde_json::from_slice(&bytes).map_err(OpenAIError::JSONDeserialize)?;
        Ok((response, cost))
    }
}

/// An error answer as async-openai reports it: a 5xx (its body may not be
/// JSON) and a rate-limit 429 are retried, anything else is final.
fn api_error(status: reqwest::StatusCode, bytes: &[u8]) -> backoff::Error<OpenAIError> {
    #[derive(serde::Deserialize)]
    struct Wrapped {
        error: ApiError,
    }
    if status.is_server_error() {
        return backoff::Error::transient(OpenAIError::ApiError(ApiError {
            message: String::from_utf8_lossy(bytes).into_owned(),
            r#type: None,
            param: None,
            code: None,
        }));
    }
    let error = match serde_json::from_slice::<Wrapped>(bytes) {
        Ok(wrapped) => wrapped.error,
        Err(e) => return backoff::Error::Permanent(OpenAIError::JSONDeserialize(e)),
    };
    if status.as_u16() == 429 && error.r#type.as_deref() != Some("insufficient_quota") {
        backoff::Error::transient(OpenAIError::ApiError(error))
    } else {
        backoff::Error::Permanent(OpenAIError::ApiError(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{AiUsageContext, AI_USAGE_CONTEXT};
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn completion_body() -> Value {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
    }

    async fn lumen_mock() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion_body()))
            .mount(&server)
            .await;
        server
    }

    /// A provider for a Lumen target (sends the metadata header).
    fn provider(server: &MockServer) -> OpenAiProvider {
        OpenAiProvider::new("test-key", Some(&format!("{}/v1", server.uri())))
            .with_lumen_metadata(true)
    }

    fn user_message() -> Vec<Message> {
        vec![Message {
            role: MessageRole::User,
            content: "hi".to_string(),
        }]
    }

    async fn only_request(server: &MockServer) -> wiremock::Request {
        let mut requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        requests.remove(0)
    }

    fn metadata_header(request: &wiremock::Request) -> Value {
        let raw = request
            .headers
            .get("x-lumen-metadata")
            .expect("x-lumen-metadata header")
            .to_str()
            .unwrap();
        serde_json::from_str(raw).unwrap()
    }

    #[tokio::test]
    async fn lumen_reports_the_call_cost_from_its_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(completion_body())
                    .insert_header("x-lumen-cost-micro-usd", "1234"),
            )
            .mount(&server)
            .await;

        let response = provider(&server)
            .chat(user_message(), "gpt-4o-mini", None, None)
            .await
            .unwrap();

        assert_eq!(response.cost_micro_usd, Some(1234));
        assert_eq!(response.content, "hello");
        assert_eq!(response.total_tokens, 2);
        let request = only_request(&server).await;
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Bearer test-key"
        );
    }

    #[tokio::test]
    async fn lumen_without_a_cost_header_reports_no_cost() {
        let server = lumen_mock().await;
        let response = provider(&server)
            .chat(user_message(), "gpt-4o-mini", None, None)
            .await
            .unwrap();
        assert_eq!(response.cost_micro_usd, None);
    }

    /// A gateway refusal keeps its code, so the key-rotation and budget
    /// handling still see it; it is not retried.
    #[tokio::test]
    async fn lumen_error_bodies_stay_api_errors_with_their_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": {"message": "key revoked", "type": "authentication_error", "code": "LM-4004"}
            })))
            .mount(&server)
            .await;

        let err = provider(&server)
            .chat(user_message(), "gpt-4o-mini", None, None)
            .await
            .err()
            .expect("an error");

        assert_eq!(err.gateway_code(), Some("LM-4004"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn lumen_server_errors_are_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("waking up"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(completion_body())
                    .insert_header("x-lumen-cost-micro-usd", "7"),
            )
            .mount(&server)
            .await;

        let response = provider(&server)
            .chat(user_message(), "gpt-4o-mini", None, None)
            .await
            .unwrap();

        assert_eq!(response.cost_micro_usd, Some(7));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn openai_sends_lumen_metadata_from_the_usage_context() {
        let server = lumen_mock().await;
        let provider = provider(&server);
        let ctx = AiUsageContext {
            job_id: "j-1".to_string(),
            account_id: Some("a".to_string()),
            feature: "extract".to_string(),
            url: "https://example.com/page".to_string(),
        };

        AI_USAGE_CONTEXT
            .scope(
                ctx,
                provider.chat(user_message(), "gpt-4o-mini", None, None),
            )
            .await
            .unwrap();

        // Neither the account nor the page URL leave the engine.
        assert_eq!(
            metadata_header(&only_request(&server).await),
            json!({"product": "scrapix", "feature": "extract", "job_id": "j-1"})
        );
    }

    #[tokio::test]
    async fn openai_sends_only_the_product_without_a_usage_context() {
        let server = lumen_mock().await;

        provider(&server)
            .chat(user_message(), "gpt-4o-mini", None, None)
            .await
            .unwrap();

        assert_eq!(
            metadata_header(&only_request(&server).await),
            json!({"product": "scrapix"})
        );
    }

    #[tokio::test]
    async fn openai_sends_no_lumen_metadata_unless_enabled() {
        let server = lumen_mock().await;
        let provider = OpenAiProvider::new("test-key", Some(&format!("{}/v1", server.uri())));
        let ctx = AiUsageContext {
            job_id: "j-1".to_string(),
            feature: "extract".to_string(),
            ..Default::default()
        };

        AI_USAGE_CONTEXT
            .scope(
                ctx,
                provider.chat(user_message(), "gpt-4o-mini", Some(50), None),
            )
            .await
            .unwrap();

        // Standalone calls (e.g. to api.openai.com) carry no job ids.
        let request = only_request(&server).await;
        assert!(request.headers.get("x-lumen-metadata").is_none());
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["max_completion_tokens"], 50);
    }

    #[tokio::test]
    async fn openai_omits_unknown_metadata_fields() {
        let server = lumen_mock().await;
        let provider = provider(&server);
        let ctx = AiUsageContext {
            feature: "ai_summary".to_string(),
            ..Default::default()
        };

        AI_USAGE_CONTEXT
            .scope(
                ctx,
                provider.chat(user_message(), "gpt-4o-mini", None, None),
            )
            .await
            .unwrap();

        assert_eq!(
            metadata_header(&only_request(&server).await),
            json!({"product": "scrapix", "feature": "ai_summary"})
        );
    }

    #[tokio::test]
    async fn openai_metadata_truncates_long_values() {
        let server = lumen_mock().await;
        let provider = provider(&server);
        // 1000 bytes of 2-byte chars: byte 256 is a char boundary, 255 is not.
        let ctx = AiUsageContext {
            job_id: "é".repeat(500),
            ..Default::default()
        };

        AI_USAGE_CONTEXT
            .scope(
                ctx,
                provider.chat(user_message(), "gpt-4o-mini", None, None),
            )
            .await
            .unwrap();

        let metadata = metadata_header(&only_request(&server).await);
        assert_eq!(metadata["job_id"], "é".repeat(128));

        // A cut that lands inside a character backs up to the boundary.
        let odd = format!("a{}", "é".repeat(500));
        let cut = truncate_utf8(&odd, 256);
        assert_eq!(cut.len(), 255);
        assert!(odd.starts_with(cut));
    }

    #[test]
    fn openai_metadata_stays_under_4_kib() {
        let ctx = AiUsageContext {
            job_id: "\u{1}".repeat(1000),
            feature: "\u{1}".repeat(1000),
            ..Default::default()
        };
        let header = lumen_metadata(Some(&ctx));
        assert!(header.len() <= 4096);
        let parsed: Value = serde_json::from_str(&header).unwrap();
        assert_eq!(parsed["product"], "scrapix");
    }

    #[tokio::test]
    async fn openai_chat_sends_max_completion_tokens() {
        let server = lumen_mock().await;

        provider(&server)
            .chat(user_message(), "gpt-4o-mini", Some(123), None)
            .await
            .unwrap();

        let body: Value = only_request(&server).await.body_json().unwrap();
        assert_eq!(body["max_completion_tokens"], 123);
        assert!(body.get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn openai_keeps_the_bearer_key() {
        let server = lumen_mock().await;

        provider(&server)
            .chat(user_message(), "gpt-4o-mini", None, None)
            .await
            .unwrap();

        let request = only_request(&server).await;
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Bearer test-key"
        );
    }
}
