//! OpenAI Chat Completions client used by the Direct backend.
//!
//! Targets non-streaming responses for the three "simple LLM" endpoints
//! (dialogue / NL→command / command metadata). Multi-agent streaming and
//! tool-calling get their own client in M4.
//!
//! Same wire format powers OpenAI-compatible gateways (vLLM, LiteLLM,
//! DeepSeek, Qwen, Ollama, OpenRouter, …) — only the `base_url` differs.

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::ai_assistant::{
    requests::GenerateDialogueResult, utils::TranscriptPart, AIGeneratedCommand,
    GenerateCommandsFromNaturalLanguageError,
};
use crate::drive::workflows::ai_assist::{GeneratedCommandMetadata, GeneratedCommandMetadataError};

use super::common::{
    byok_unlimited_request_limit, parse_commands_payload, parse_metadata_payload, truncate_for_log,
    COMMANDS_SYSTEM_PROMPT, DIALOGUE_SYSTEM_PROMPT, METADATA_SYSTEM_PROMPT,
};
use super::{LlmBackend, ResolvedProvider};

pub struct OpenAiBackend {
    provider: ResolvedProvider,
    client: Client,
}

impl OpenAiBackend {
    pub fn new(provider: ResolvedProvider) -> Self {
        Self {
            provider,
            client: Client::new(),
        }
    }

    fn completions_url(&self) -> String {
        format!("{}/v1/chat/completions", self.provider.base_url)
    }

    async fn chat(&self, messages: Vec<ChatMessage>, force_json: bool) -> anyhow::Result<String> {
        let response_format = force_json.then_some(ResponseFormat {
            r#type: "json_object",
        });
        let body = ChatRequest {
            model: &self.provider.model_id,
            messages,
            stream: false,
            response_format,
        };

        let resp = self
            .client
            .post(self.completions_url())
            .bearer_auth(&self.provider.api_key)
            .json(&body)
            .send()
            .await
            .context("OpenAI request transport error")?;

        let status = resp.status();
        let raw = resp.text().await.context("OpenAI response read error")?;

        if !status.is_success() {
            return Err(anyhow!(
                "OpenAI returned HTTP {} (model={}): {}",
                status.as_u16(),
                self.provider.model_id,
                truncate_for_log(&raw, 1024)
            ));
        }

        let parsed: ChatResponse = serde_json::from_str(&raw).with_context(|| {
            format!(
                "OpenAI response parse error: {}",
                truncate_for_log(&raw, 256)
            )
        })?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| anyhow!("OpenAI response had no choices/content"))?;

        Ok(content)
    }
}

#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
impl LlmBackend for OpenAiBackend {
    async fn generate_dialogue_answer(
        &self,
        transcript: Vec<TranscriptPart>,
        prompt: String,
    ) -> anyhow::Result<GenerateDialogueResult> {
        let mut messages = Vec::with_capacity(transcript.len() * 2 + 2);
        messages.push(ChatMessage {
            role: "system",
            content: DIALOGUE_SYSTEM_PROMPT.to_string(),
        });
        for part in transcript.iter() {
            let user = part.raw_user_prompt().to_string();
            let assistant = part.raw_assistant_answer().to_string();
            if !user.is_empty() {
                messages.push(ChatMessage {
                    role: "user",
                    content: user,
                });
            }
            if !assistant.is_empty() {
                messages.push(ChatMessage {
                    role: "assistant",
                    content: assistant,
                });
            }
        }
        messages.push(ChatMessage {
            role: "user",
            content: prompt,
        });

        let answer = self.chat(messages, false).await?;
        Ok(GenerateDialogueResult::Success {
            answer,
            truncated: false,
            request_limit_info: byok_unlimited_request_limit(),
            transcript_summarized: false,
        })
    }

    async fn generate_commands_from_natural_language(
        &self,
        prompt: String,
    ) -> Result<Vec<AIGeneratedCommand>, GenerateCommandsFromNaturalLanguageError> {
        let messages = vec![
            ChatMessage {
                role: "system",
                content: COMMANDS_SYSTEM_PROMPT.to_string(),
            },
            ChatMessage {
                role: "user",
                content: prompt,
            },
        ];

        let raw = self.chat(messages, true).await.map_err(|e| {
            log::error!("DirectBackend OpenAI generate_commands failed: {e:#}");
            GenerateCommandsFromNaturalLanguageError::Other
        })?;

        parse_commands_payload(&raw).map_err(|e| {
            log::error!("DirectBackend OpenAI generate_commands parse failed: {e:#}");
            GenerateCommandsFromNaturalLanguageError::Other
        })
    }

    async fn generate_metadata_for_command(
        &self,
        command: String,
    ) -> Result<GeneratedCommandMetadata, GeneratedCommandMetadataError> {
        let messages = vec![
            ChatMessage {
                role: "system",
                content: METADATA_SYSTEM_PROMPT.to_string(),
            },
            ChatMessage {
                role: "user",
                content: command,
            },
        ];

        let raw = self.chat(messages, true).await.map_err(|e| {
            log::error!("DirectBackend OpenAI generate_metadata failed: {e:#}");
            GeneratedCommandMetadataError::Other
        })?;

        parse_metadata_payload(&raw).map_err(|e| {
            log::error!("DirectBackend OpenAI generate_metadata parse failed: {e:#}");
            GeneratedCommandMetadataError::Other
        })
    }
}

// ── Wire types ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct ResponseFormat {
    r#type: &'static str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
}
