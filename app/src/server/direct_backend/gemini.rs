//! Gemini `generateContent` client used by the Direct backend.
//!
//! Endpoint: `POST {base_url}/v1beta/models/{model}:generateContent`
//! Auth:    `x-goog-api-key` header (plain API key)
//!
//! Gemini renames assistant→model and groups text into `parts`. JSON-only
//! endpoints set `generationConfig.responseMimeType = "application/json"`,
//! which the API enforces server-side; we still run the response through
//! `common::strip_json_fence` because some compat gateways are not strict.

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

pub struct GeminiBackend {
    provider: ResolvedProvider,
    client: Client,
}

impl GeminiBackend {
    pub fn new(provider: ResolvedProvider) -> Self {
        Self {
            provider,
            client: Client::new(),
        }
    }

    fn generate_url(&self) -> String {
        format!(
            "{}/v1beta/models/{}:generateContent",
            self.provider.base_url, self.provider.model_id
        )
    }

    async fn generate(
        &self,
        system: &str,
        contents: Vec<GeminiContent>,
        force_json: bool,
    ) -> anyhow::Result<String> {
        let body = GeminiRequest {
            system_instruction: GeminiContent {
                role: "user",
                parts: vec![GeminiPart {
                    text: system.to_string(),
                }],
            },
            contents,
            generation_config: force_json.then_some(GenerationConfig {
                response_mime_type: "application/json",
            }),
        };

        let resp = self
            .client
            .post(self.generate_url())
            .header("x-goog-api-key", &self.provider.api_key)
            .json(&body)
            .send()
            .await
            .context("Gemini request transport error")?;

        let status = resp.status();
        let raw = resp.text().await.context("Gemini response read error")?;

        if !status.is_success() {
            return Err(anyhow!(
                "Gemini returned HTTP {} (model={}): {}",
                status.as_u16(),
                self.provider.model_id,
                truncate_for_log(&raw, 1024)
            ));
        }

        let parsed: GeminiResponse = serde_json::from_str(&raw).with_context(|| {
            format!(
                "Gemini response parse error: {}",
                truncate_for_log(&raw, 256)
            )
        })?;

        let text = parsed
            .candidates
            .into_iter()
            .next()
            .and_then(|c| {
                let joined: String = c
                    .content
                    .parts
                    .into_iter()
                    .map(|p| p.text)
                    .collect::<Vec<_>>()
                    .join("\n");
                (!joined.is_empty()).then_some(joined)
            })
            .ok_or_else(|| anyhow!("Gemini response had no text candidates"))?;

        Ok(text)
    }

    fn build_contents(transcript: &[TranscriptPart], prompt: String) -> Vec<GeminiContent> {
        let mut contents = Vec::with_capacity(transcript.len() * 2 + 1);
        for part in transcript.iter() {
            let user = part.raw_user_prompt().to_string();
            let assistant = part.raw_assistant_answer().to_string();
            if !user.is_empty() {
                contents.push(GeminiContent {
                    role: "user",
                    parts: vec![GeminiPart { text: user }],
                });
            }
            if !assistant.is_empty() {
                contents.push(GeminiContent {
                    role: "model",
                    parts: vec![GeminiPart { text: assistant }],
                });
            }
        }
        contents.push(GeminiContent {
            role: "user",
            parts: vec![GeminiPart { text: prompt }],
        });
        contents
    }
}

#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
impl LlmBackend for GeminiBackend {
    async fn generate_dialogue_answer(
        &self,
        transcript: Vec<TranscriptPart>,
        prompt: String,
    ) -> anyhow::Result<GenerateDialogueResult> {
        let contents = Self::build_contents(&transcript, prompt);
        let answer = self
            .generate(DIALOGUE_SYSTEM_PROMPT, contents, false)
            .await?;
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
        let contents = vec![GeminiContent {
            role: "user",
            parts: vec![GeminiPart { text: prompt }],
        }];

        let raw = self
            .generate(COMMANDS_SYSTEM_PROMPT, contents, true)
            .await
            .map_err(|e| {
                log::error!("DirectBackend Gemini generate_commands failed: {e:#}");
                GenerateCommandsFromNaturalLanguageError::Other
            })?;

        parse_commands_payload(&raw).map_err(|e| {
            log::error!("DirectBackend Gemini generate_commands parse failed: {e:#}");
            GenerateCommandsFromNaturalLanguageError::Other
        })
    }

    async fn generate_metadata_for_command(
        &self,
        command: String,
    ) -> Result<GeneratedCommandMetadata, GeneratedCommandMetadataError> {
        let contents = vec![GeminiContent {
            role: "user",
            parts: vec![GeminiPart { text: command }],
        }];

        let raw = self
            .generate(METADATA_SYSTEM_PROMPT, contents, true)
            .await
            .map_err(|e| {
                log::error!("DirectBackend Gemini generate_metadata failed: {e:#}");
                GeneratedCommandMetadataError::Other
            })?;

        parse_metadata_payload(&raw).map_err(|e| {
            log::error!("DirectBackend Gemini generate_metadata parse failed: {e:#}");
            GeneratedCommandMetadataError::Other
        })
    }
}

// ── Wire types ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct GeminiRequest {
    #[serde(rename = "systemInstruction")]
    system_instruction: GeminiContent,
    contents: Vec<GeminiContent>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(rename = "responseMimeType")]
    response_mime_type: &'static str,
}

#[derive(Serialize)]
struct GeminiContent {
    role: &'static str,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiPart {
    text: String,
}

#[derive(Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiResponseContent,
}

#[derive(Deserialize)]
struct GeminiResponseContent {
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    #[serde(default)]
    text: String,
}
