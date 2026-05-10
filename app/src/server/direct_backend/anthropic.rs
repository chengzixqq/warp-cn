//! Anthropic Messages API client used by the Direct backend.
//!
//! Endpoint: `POST {base_url}/v1/messages`
//! Auth:    `x-api-key` header (NOT bearer)
//! Version: pinned via `anthropic-version` header
//!
//! Anthropic doesn't expose an OpenAI-style `response_format: json_object`,
//! so the JSON-only endpoints (`generate_commands` / `generate_metadata`) lean
//! on prompt-level instructions plus the `common::strip_json_fence` parser to
//! defend against accidental Markdown wrapping.

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

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;

pub struct AnthropicBackend {
    provider: ResolvedProvider,
    client: Client,
}

impl AnthropicBackend {
    pub fn new(provider: ResolvedProvider) -> Self {
        Self {
            provider,
            client: Client::new(),
        }
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.provider.base_url)
    }

    async fn message(
        &self,
        system: &str,
        messages: Vec<AnthropicMessage>,
    ) -> anyhow::Result<String> {
        let body = AnthropicRequest {
            model: &self.provider.model_id,
            max_tokens: DEFAULT_MAX_TOKENS,
            system,
            messages,
        };

        let resp = self
            .client
            .post(self.messages_url())
            .header("x-api-key", &self.provider.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .context("Anthropic request transport error")?;

        let status = resp.status();
        let raw = resp.text().await.context("Anthropic response read error")?;

        if !status.is_success() {
            return Err(anyhow!(
                "Anthropic returned HTTP {} (model={}): {}",
                status.as_u16(),
                self.provider.model_id,
                truncate_for_log(&raw, 1024)
            ));
        }

        let parsed: AnthropicResponse = serde_json::from_str(&raw).with_context(|| {
            format!(
                "Anthropic response parse error: {}",
                truncate_for_log(&raw, 256)
            )
        })?;

        let text = parsed
            .content
            .into_iter()
            .filter_map(|block| match block {
                AnthropicContentBlock::Text { text } => Some(text),
                AnthropicContentBlock::Other => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if text.is_empty() {
            return Err(anyhow!("Anthropic response had no text blocks"));
        }
        Ok(text)
    }
}

#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
impl LlmBackend for AnthropicBackend {
    async fn generate_dialogue_answer(
        &self,
        transcript: Vec<TranscriptPart>,
        prompt: String,
    ) -> anyhow::Result<GenerateDialogueResult> {
        let mut messages = Vec::with_capacity(transcript.len() * 2 + 1);
        for part in transcript.iter() {
            let user = part.raw_user_prompt().to_string();
            let assistant = part.raw_assistant_answer().to_string();
            if !user.is_empty() {
                messages.push(AnthropicMessage {
                    role: "user",
                    content: user,
                });
            }
            if !assistant.is_empty() {
                messages.push(AnthropicMessage {
                    role: "assistant",
                    content: assistant,
                });
            }
        }
        messages.push(AnthropicMessage {
            role: "user",
            content: prompt,
        });

        let answer = self.message(DIALOGUE_SYSTEM_PROMPT, messages).await?;
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
        let messages = vec![AnthropicMessage {
            role: "user",
            content: prompt,
        }];

        let raw = self
            .message(COMMANDS_SYSTEM_PROMPT, messages)
            .await
            .map_err(|e| {
                log::error!("DirectBackend Anthropic generate_commands failed: {e:#}");
                GenerateCommandsFromNaturalLanguageError::Other
            })?;

        parse_commands_payload(&raw).map_err(|e| {
            log::error!("DirectBackend Anthropic generate_commands parse failed: {e:#}");
            GenerateCommandsFromNaturalLanguageError::Other
        })
    }

    async fn generate_metadata_for_command(
        &self,
        command: String,
    ) -> Result<GeneratedCommandMetadata, GeneratedCommandMetadataError> {
        let messages = vec![AnthropicMessage {
            role: "user",
            content: command,
        }];

        let raw = self
            .message(METADATA_SYSTEM_PROMPT, messages)
            .await
            .map_err(|e| {
                log::error!("DirectBackend Anthropic generate_metadata failed: {e:#}");
                GeneratedCommandMetadataError::Other
            })?;

        parse_metadata_payload(&raw).map_err(|e| {
            log::error!("DirectBackend Anthropic generate_metadata parse failed: {e:#}");
            GeneratedCommandMetadataError::Other
        })
    }
}

// ── Wire types ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}
