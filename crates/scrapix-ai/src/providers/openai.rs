//! OpenAI provider using the async-openai crate

use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestMessageContentPartText,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
        CreateChatCompletionRequest, CreateChatCompletionRequestArgs, ImageDetail, ImageUrl,
    },
    Client as OpenAIClient,
};
use async_trait::async_trait;

use super::{ChatResponse, ImageInput, LlmProvider, Message, MessageRole};
use crate::client::AiClientError;

/// OpenAI provider backed by async-openai
pub struct OpenAiProvider {
    client: OpenAIClient<OpenAIConfig>,
}

impl OpenAiProvider {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        let mut config = OpenAIConfig::new().with_api_key(api_key);
        if let Some(url) = base_url {
            config = config.with_api_base(url);
        }
        Self {
            client: OpenAIClient::with_config(config),
        }
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
            request_builder.max_tokens(tokens);
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
            // `max_tokens` is rejected by current reasoning models.
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
        let response = self.client.chat().create(request).await?;

        let choice = response
            .choices
            .first()
            .ok_or(AiClientError::EmptyResponse)?;

        let content = choice.message.content.clone().unwrap_or_default();
        let usage = response.usage.as_ref();

        Ok(ChatResponse {
            content,
            model: response.model,
            prompt_tokens: usage.map(|u| u.prompt_tokens).unwrap_or(0),
            completion_tokens: usage.map(|u| u.completion_tokens).unwrap_or(0),
            total_tokens: usage.map(|u| u.total_tokens).unwrap_or(0),
            finish_reason: choice.finish_reason.as_ref().map(|r| format!("{:?}", r)),
        })
    }
}
