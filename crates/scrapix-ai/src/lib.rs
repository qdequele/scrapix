//! # Scrapix AI
//!
//! AI-powered extraction and enrichment for web crawling.
//!
//! This crate provides AI capabilities for the Scrapix crawler:
//!
//! - **Extraction** - Extract structured data using custom prompts
//! - **Summarization** - Generate summaries of varying lengths and styles
//!
//! Supports multiple LLM providers: OpenAI, Anthropic, Google Gemini, and Mistral.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use scrapix_ai::{AiClient, AiClientConfig, Summarizer, AiExtractor};
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create the AI client (defaults to OpenAI)
//!     let config = AiClientConfig {
//!         api_key: std::env::var("OPENAI_API_KEY")?,
//!         ..Default::default()
//!     };
//!     let client = Arc::new(AiClient::new(config)?);
//!
//!     // Or use a different provider
//!     // let config = AiClientConfig {
//!     //     api_key: std::env::var("ANTHROPIC_API_KEY")?,
//!     //     provider: "anthropic".to_string(),
//!     //     ..Default::default()
//!     // };
//!
//!     // Summarize content
//!     let summarizer = Summarizer::with_defaults(client.clone());
//!     let summary = summarizer.summarize("Long content to summarize...").await?;
//!     println!("Summary: {}", summary.summary);
//!
//!     // Extract structured data
//!     let extractor = AiExtractor::with_defaults(client.clone());
//!     let result = extractor.extract_fields(
//!         "Article about technology...",
//!         &[("title", "Article title"), ("author", "Author name")]
//!     ).await?;
//!     println!("Extracted: {:?}", result.data);
//!
//!     Ok(())
//! }
//! ```

pub mod client;
pub mod extraction;
pub mod providers;
pub mod summary;

// Re-export main types from client
pub use client::{
    AiClient, AiClientConfig, AiClientError, AiUsageContext, AiUsageEvent, AiUsageReceiver,
    ChatResponse, AI_USAGE_CONTEXT,
};

/// Error codes of the Lumen AI gateway (the `code` of its
/// `{"error":{"code","message","type"}}` bodies).
pub mod gateway_codes {
    /// 402: the team's budget is used up.
    pub const BUDGET_EXCEEDED: &str = "LM-4001";
    /// 401: the gateway refused the API key (revoked or rotated).
    pub const KEY_REFUSED: &str = "LM-4004";
    /// 404: the requested model is not served.
    pub const UNKNOWN_MODEL: &str = "LM-2001";
    /// 400: the request itself is invalid.
    pub const INVALID_REQUEST: &str = "LM-1001";
}

pub use providers::ImageInput;

// Re-export extraction types
pub use extraction::{
    AiExtractor, ExtractionConfig, ExtractionResult, ExtractionSchema, FieldDefinition,
    SchemaBuilder, DEFAULT_EXTRACTION_MODEL, DEFAULT_MAX_RESPONSE_TOKENS,
};

// Re-export summary types
pub use summary::{
    Summarizer, SummaryConfig, SummaryConfigBuilder, SummaryLength, SummaryResult, SummaryStyle,
    DEFAULT_SUMMARY_MODEL,
};

use scrapix_core::{Result, ScrapixError};
use std::sync::Arc;

/// The gateway code (see [`gateway_codes`]) of an AI error, when the AI
/// gateway refused the call.
pub fn ai_gateway_code(err: &ScrapixError) -> Option<&str> {
    match err {
        ScrapixError::AiGateway { code, .. } => Some(code),
        _ => None,
    }
}

/// Convert an AI failure: a gateway refusal keeps its code
/// ([`ScrapixError::AiGateway`]), anything else is [`ScrapixError::Ai`].
fn ai_error<E: std::error::Error + 'static>(err: E) -> ScrapixError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&err);
    while let Some(e) = source {
        if let Some(client_err) = e.downcast_ref::<AiClientError>() {
            if let (Some(code), Some(message)) =
                (client_err.gateway_code(), client_err.gateway_message())
            {
                return ScrapixError::AiGateway {
                    code: code.to_string(),
                    message: message.to_string(),
                };
            }
        }
        source = e.source();
    }
    ScrapixError::Ai(err.to_string())
}

/// Unified AI service providing all AI capabilities
///
/// This struct combines extraction and summarization into a single service.
pub struct AiService {
    client: Arc<AiClient>,
    extractor: Option<AiExtractor>,
    summarizer: Option<Summarizer>,
}

impl AiService {
    /// Create a new AI service with all features enabled
    pub fn new(client: Arc<AiClient>) -> Self {
        Self {
            extractor: Some(AiExtractor::with_defaults(client.clone())),
            summarizer: Some(Summarizer::with_defaults(client.clone())),
            client,
        }
    }

    /// Create a minimal service (no features enabled)
    pub fn minimal(client: Arc<AiClient>) -> Self {
        Self {
            client,
            extractor: None,
            summarizer: None,
        }
    }

    /// Enable extraction with default config
    pub fn with_extraction(mut self) -> Self {
        self.extractor = Some(AiExtractor::with_defaults(self.client.clone()));
        self
    }

    /// Enable extraction with custom config
    pub fn with_extraction_config(mut self, config: ExtractionConfig) -> Self {
        self.extractor = Some(AiExtractor::new(self.client.clone(), config));
        self
    }

    /// Enable summarization with default config
    pub fn with_summarization(mut self) -> Self {
        self.summarizer = Some(Summarizer::with_defaults(self.client.clone()));
        self
    }

    /// Enable summarization with custom config
    pub fn with_summary_config(mut self, config: SummaryConfig) -> Self {
        self.summarizer = Some(Summarizer::new(self.client.clone(), config));
        self
    }

    /// Get the underlying client
    pub fn client(&self) -> &Arc<AiClient> {
        &self.client
    }

    /// Get the extractor (if enabled)
    pub fn extractor(&self) -> Option<&AiExtractor> {
        self.extractor.as_ref()
    }

    /// Get the summarizer (if enabled)
    pub fn summarizer(&self) -> Option<&Summarizer> {
        self.summarizer.as_ref()
    }

    /// Extract data using a custom prompt
    pub async fn extract(&self, content: &str, prompt: &str) -> Result<ExtractionResult> {
        let extractor = self
            .extractor
            .as_ref()
            .ok_or_else(|| ScrapixError::Ai("Extraction not enabled".to_string()))?;

        extractor
            .extract_with_prompt(content, prompt)
            .await
            .map_err(ai_error)
    }

    /// Extract structured data using a schema
    pub async fn extract_schema(
        &self,
        content: &str,
        schema: &ExtractionSchema,
    ) -> Result<ExtractionResult> {
        let extractor = self
            .extractor
            .as_ref()
            .ok_or_else(|| ScrapixError::Ai("Extraction not enabled".to_string()))?;

        extractor
            .extract_with_schema(content, schema)
            .await
            .map_err(ai_error)
    }

    /// Summarize content
    pub async fn summarize(&self, content: &str) -> Result<SummaryResult> {
        let summarizer = self
            .summarizer
            .as_ref()
            .ok_or_else(|| ScrapixError::Ai("Summarization not enabled".to_string()))?;

        summarizer.summarize(content).await.map_err(ai_error)
    }

    /// Generate a TL;DR
    pub async fn tldr(&self, content: &str) -> Result<String> {
        let summarizer = self
            .summarizer
            .as_ref()
            .ok_or_else(|| ScrapixError::Ai("Summarization not enabled".to_string()))?;

        summarizer.tldr(content).await.map_err(ai_error)
    }

    /// Generate a headline
    pub async fn headline(&self, content: &str) -> Result<String> {
        let summarizer = self
            .summarizer
            .as_ref()
            .ok_or_else(|| ScrapixError::Ai("Summarization not enabled".to_string()))?;

        summarizer
            .generate_headline(content)
            .await
            .map_err(ai_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_client() -> Arc<AiClient> {
        Arc::new(
            AiClient::new(AiClientConfig {
                api_key: "test-key".to_string(),
                ..Default::default()
            })
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn gateway_errors_become_ai_gateway() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(402).set_body_json(serde_json::json!({
                "error": {
                    "code": "LM-4001",
                    "message": "budget exceeded",
                    "type": "lumen_error"
                }
            })))
            .mount(&server)
            .await;
        let client = Arc::new(
            AiClient::new(AiClientConfig {
                api_key: "test-key".to_string(),
                provider: "openai".to_string(),
                base_url: Some(format!("{}/v1", server.uri())),
                ..Default::default()
            })
            .unwrap(),
        );

        let err = AiService::new(client)
            .extract("some page", "extract the title")
            .await
            .unwrap_err();

        assert_eq!(ai_gateway_code(&err), Some(gateway_codes::BUDGET_EXCEEDED));
        match err {
            ScrapixError::AiGateway { code, message } => {
                assert_eq!(code, "LM-4001");
                assert!(message.contains("budget exceeded"), "{message}");
            }
            other => panic!("expected AiGateway, got {other:?}"),
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn ai_gateway_code_ignores_other_errors() {
        assert_eq!(ai_gateway_code(&ScrapixError::Ai("x".into())), None);
    }

    #[test]
    fn test_ai_service_creation() {
        let client = create_test_client();
        let service = AiService::new(client);

        assert!(service.extractor().is_some());
        assert!(service.summarizer().is_some());
    }

    #[test]
    fn test_ai_service_minimal() {
        let client = create_test_client();
        let service = AiService::minimal(client);

        assert!(service.extractor().is_none());
        assert!(service.summarizer().is_none());
    }

    #[test]
    fn test_ai_service_builder() {
        let client = create_test_client();
        let service = AiService::minimal(client).with_summarization();

        assert!(service.extractor().is_none());
        assert!(service.summarizer().is_some());
    }

    #[test]
    fn test_token_counting() {
        let text = "Hello, world!";
        let tokens = AiClient::count_tokens(text, "gpt-4").unwrap();
        assert!(tokens > 0);
        assert!(tokens < 10);
    }
}
