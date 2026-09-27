//! LLM provider abstraction for multi-provider support
//!
//! Supports OpenAI, Anthropic, Google Gemini, and Mistral providers.

pub mod anthropic;
pub mod gemini;
pub mod mistral;
pub mod openai;

use crate::client::AiClientError;
use async_trait::async_trait;

/// Message role in a conversation
#[derive(Debug, Clone)]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

/// A chat message
#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

/// An image attached to a vision request.
#[derive(Debug, Clone)]
pub struct ImageInput {
    /// Media type: `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: String,
    /// Raw image bytes (base64-encoded by the provider).
    pub data: Vec<u8>,
}

impl ImageInput {
    pub fn png(data: Vec<u8>) -> Self {
        Self {
            media_type: "image/png".to_string(),
            data,
        }
    }

    pub(crate) fn base64(&self) -> String {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        STANDARD.encode(&self.data)
    }

    pub(crate) fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.base64())
    }
}

/// Normalized chat response from any provider
pub struct ChatResponse {
    pub content: String,
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub finish_reason: Option<String>,
}

/// Trait for LLM providers
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(
        &self,
        messages: Vec<Message>,
        model: &str,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
    ) -> Result<ChatResponse, AiClientError>;

    /// Single-turn vision request: a system prompt, then one user turn
    /// holding `image` followed by `prompt`.
    async fn vision(
        &self,
        _system: &str,
        _prompt: &str,
        _image: &ImageInput,
        _model: &str,
        _max_tokens: Option<u32>,
    ) -> Result<ChatResponse, AiClientError> {
        Err(AiClientError::Config(
            "this provider does not support image input".to_string(),
        ))
    }
}
