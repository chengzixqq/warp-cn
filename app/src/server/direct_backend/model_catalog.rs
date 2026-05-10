//! warp-cn fork: dynamic model catalog fetcher.
//!
//! Replaces the upstream `FreeAvailableModels` GraphQL fetch with a direct
//! `/v1/models` (or equivalent) call against whichever provider the user has
//! configured in `DirectBackendConfig`. Maps the provider's native list shape
//! into the same `ModelsByFeature` struct the rest of the app already
//! consumes, so zero changes are needed downstream of `LLMPreferences`.
//!
//! Provider endpoints:
//!   OpenAI / OpenAI-compatible: `GET {base_url}/v1/models`
//!     Authorization: Bearer {api_key}
//!     Response: `{ "data": [{ "id": "...", "owned_by": "...", ... }, ...] }`
//!   Anthropic: `GET {base_url}/v1/models`
//!     x-api-key, anthropic-version: 2023-06-01
//!     Response: `{ "data": [{ "id": "...", "display_name": "...", ... }, ...] }`
//!   Gemini: `GET {base_url}/v1beta/models`
//!     x-goog-api-key
//!     Response: `{ "models": [{ "name": "models/...", "displayName": "...", ... }] }`
//!   Ollama (auto-detected by base_url containing :11434 or /api): `GET {base_url}/api/tags`
//!     Response: `{ "models": [{ "name": "llama3:8b", "size": ..., ... }] }`
//!
//! Catalog entries are emitted with `spec: None`, so the picker side panel
//! won't show fabricated Intelligence / Speed / Cost bars for third-party
//! models — those are Warp-specific benchmarks. `disable_reason: None`
//! since we already have a key configured for the chosen provider.

use std::collections::HashMap;
use std::time::Duration;

use ai::direct_backend::{current_snapshot, DirectProviderKind};
use ai::LLMId;
use anyhow::{anyhow, Context};
use reqwest::Client;
use serde::Deserialize;

use crate::ai::llms::{
    AvailableLLMs, LLMContextWindow, LLMInfo, LLMProvider, LLMUsageMetadata, ModelsByFeature,
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Detect Ollama by URL shape — it uses `/api/tags`, not `/v1/models`.
fn looks_like_ollama(base_url: &str) -> bool {
    base_url.contains(":11434") || base_url.contains("/api/")
}

/// Top-level entry. Reads the active provider from `DirectBackendConfig` snapshot,
/// fetches its model list, returns a fully-populated `ModelsByFeature` ready for
/// `LLMPreferences::on_server_update`. Returns `None` when no provider is
/// configured (caller should fall back to the upstream GraphQL fetch).
pub async fn fetch_dynamic_catalog() -> Option<anyhow::Result<ModelsByFeature>> {
    let snap = current_snapshot();
    for kind in [
        DirectProviderKind::Anthropic,
        DirectProviderKind::OpenAi,
        DirectProviderKind::Gemini,
    ] {
        let overrides = match kind {
            DirectProviderKind::Anthropic => &snap.anthropic,
            DirectProviderKind::OpenAi => &snap.openai,
            DirectProviderKind::Gemini => &snap.gemini,
            DirectProviderKind::OpenAiCompatible => continue,
        };
        let api_key = overrides.api_key.trim();
        if api_key.is_empty() {
            continue;
        }
        let base_url = if overrides.base_url.trim().is_empty() {
            default_base_url(kind).to_string()
        } else {
            overrides.base_url.trim().trim_end_matches('/').to_string()
        };
        return Some(fetch_for_provider(kind, &base_url, api_key).await);
    }
    None
}

fn default_base_url(kind: DirectProviderKind) -> &'static str {
    match kind {
        DirectProviderKind::OpenAi | DirectProviderKind::OpenAiCompatible => {
            "https://api.openai.com"
        }
        DirectProviderKind::Anthropic => "https://api.anthropic.com",
        DirectProviderKind::Gemini => "https://generativelanguage.googleapis.com",
    }
}

async fn fetch_for_provider(
    kind: DirectProviderKind,
    base_url: &str,
    api_key: &str,
) -> anyhow::Result<ModelsByFeature> {
    let infos = match kind {
        DirectProviderKind::OpenAi | DirectProviderKind::OpenAiCompatible => {
            if looks_like_ollama(base_url) {
                fetch_ollama(base_url).await?
            } else {
                fetch_openai_like(base_url, api_key).await?
            }
        }
        DirectProviderKind::Anthropic => fetch_anthropic(base_url, api_key).await?,
        DirectProviderKind::Gemini => fetch_gemini(base_url, api_key).await?,
    };
    if infos.is_empty() {
        return Err(anyhow!("provider returned an empty model list"));
    }
    build_models_by_feature(infos)
}

/// Project a flat `Vec<LLMInfo>` into the 4-feature catalog. The same list is
/// reused for every feature: third-party providers don't differentiate by
/// agent/coding/cli/computer-use the way Warp's internal routing does, and
/// the picker's tab UI still works (each tab renders the same set).
fn build_models_by_feature(infos: Vec<LLMInfo>) -> anyhow::Result<ModelsByFeature> {
    let default_id = infos[0].id.clone();
    let agent_mode = AvailableLLMs::new(default_id.clone(), infos.clone(), None)?;
    let coding = AvailableLLMs::new(default_id.clone(), infos.clone(), None)?;
    let cli_agent = AvailableLLMs::new(default_id, infos, None)?;
    Ok(ModelsByFeature {
        agent_mode,
        coding,
        cli_agent: Some(cli_agent),
        computer_use: None,
    })
}

// ── OpenAI / OpenAI-compatible ──────────────────────────────────────────────

#[derive(Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelEntry>,
}

#[derive(Deserialize)]
struct OpenAiModelEntry {
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    owned_by: Option<String>,
}

async fn fetch_openai_like(base_url: &str, api_key: &str) -> anyhow::Result<Vec<LLMInfo>> {
    let url = format!("{base_url}/v1/models");
    let client = Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("OpenAI catalog client init")?;
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .context("OpenAI catalog transport error")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "OpenAI catalog HTTP {}: {}",
            status.as_u16(),
            truncate(&body, 256)
        ));
    }
    let parsed: OpenAiModelsResponse = serde_json::from_str(&body)
        .with_context(|| format!("OpenAI catalog parse: {}", truncate(&body, 256)))?;
    let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
    ids.sort();
    Ok(ids
        .into_iter()
        .map(|id| build_info(LLMProvider::OpenAI, &id, &id))
        .collect())
}

// ── Anthropic ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AnthropicModelsResponse {
    data: Vec<AnthropicModelEntry>,
}

#[derive(Deserialize)]
struct AnthropicModelEntry {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
}

async fn fetch_anthropic(base_url: &str, api_key: &str) -> anyhow::Result<Vec<LLMInfo>> {
    let url = format!("{base_url}/v1/models");
    let client = Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("Anthropic catalog client init")?;
    let resp = client
        .get(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .context("Anthropic catalog transport error")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "Anthropic catalog HTTP {}: {}",
            status.as_u16(),
            truncate(&body, 256)
        ));
    }
    let parsed: AnthropicModelsResponse = serde_json::from_str(&body)
        .with_context(|| format!("Anthropic catalog parse: {}", truncate(&body, 256)))?;
    Ok(parsed
        .data
        .into_iter()
        .map(|m| {
            let display = m.display_name.unwrap_or_else(|| m.id.clone());
            build_info(LLMProvider::Anthropic, &m.id, &display)
        })
        .collect())
}

// ── Gemini ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GeminiModelsResponse {
    #[serde(default)]
    models: Vec<GeminiModelEntry>,
}

#[derive(Deserialize)]
struct GeminiModelEntry {
    /// Always prefixed with "models/" — strip it for the wire id.
    name: String,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default, rename = "supportedGenerationMethods")]
    supported_methods: Vec<String>,
}

async fn fetch_gemini(base_url: &str, api_key: &str) -> anyhow::Result<Vec<LLMInfo>> {
    let url = format!("{base_url}/v1beta/models");
    let client = Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("Gemini catalog client init")?;
    let resp = client
        .get(&url)
        .header("x-goog-api-key", api_key)
        .send()
        .await
        .context("Gemini catalog transport error")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "Gemini catalog HTTP {}: {}",
            status.as_u16(),
            truncate(&body, 256)
        ));
    }
    let parsed: GeminiModelsResponse = serde_json::from_str(&body)
        .with_context(|| format!("Gemini catalog parse: {}", truncate(&body, 256)))?;
    Ok(parsed
        .models
        .into_iter()
        // Filter to chat-capable models — Gemini lists embedding/image models too.
        .filter(|m| {
            m.supported_methods.is_empty()
                || m.supported_methods
                    .iter()
                    .any(|s| s == "generateContent" || s == "streamGenerateContent")
        })
        .map(|m| {
            let id = m.name.strip_prefix("models/").unwrap_or(&m.name).to_string();
            let display = m.display_name.unwrap_or_else(|| id.clone());
            build_info(LLMProvider::Google, &id, &display)
        })
        .collect())
}

// ── Ollama ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagEntry>,
}

#[derive(Deserialize)]
struct OllamaTagEntry {
    name: String,
}

async fn fetch_ollama(base_url: &str) -> anyhow::Result<Vec<LLMInfo>> {
    let url = format!("{base_url}/api/tags");
    let client = Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("Ollama catalog client init")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .context("Ollama catalog transport error (is `ollama serve` running?)")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "Ollama catalog HTTP {}: {}",
            status.as_u16(),
            truncate(&body, 256)
        ));
    }
    let parsed: OllamaTagsResponse = serde_json::from_str(&body)
        .with_context(|| format!("Ollama catalog parse: {}", truncate(&body, 256)))?;
    Ok(parsed
        .models
        .into_iter()
        .map(|m| build_info(LLMProvider::Unknown, &m.name, &m.name))
        .collect())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_info(provider: LLMProvider, id: &str, display: &str) -> LLMInfo {
    LLMInfo {
        display_name: display.to_string(),
        base_model_name: id.to_string(),
        id: LLMId::from(id),
        reasoning_level: None,
        usage_metadata: LLMUsageMetadata {
            request_multiplier: 1,
            credit_multiplier: None,
        },
        description: None,
        // Already authenticated — we have the key on file.
        disable_reason: None,
        // Conservatively false; users who know better can still use these
        // models with vision-capable requests, but the picker won't
        // misadvertise (e.g. `gpt-4o-mini` *does* support vision but the
        // /v1/models endpoint doesn't expose that flag).
        vision_supported: false,
        // None → picker side panel hides the bars (Phase 3 of M4.3 ships the
        // "no benchmark" rendering path).
        spec: None,
        provider,
        host_configs: HashMap::new(),
        discount_percentage: None,
        context_window: LLMContextWindow::default(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_url_detection() {
        assert!(looks_like_ollama("http://localhost:11434"));
        assert!(looks_like_ollama("http://192.168.1.50:11434"));
        assert!(looks_like_ollama("http://gateway/api/v1"));
        assert!(!looks_like_ollama("https://api.openai.com"));
        assert!(!looks_like_ollama("https://api.deepseek.com"));
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        let s = "中文测试";
        let t = truncate(s, 5);
        assert!(s.starts_with(&t.trim_end_matches('…')));
    }

    #[test]
    fn parses_openai_response() {
        let body = r#"{"data":[{"id":"gpt-4o-mini","object":"model","owned_by":"openai"},{"id":"gpt-4o"}]}"#;
        let parsed: OpenAiModelsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].id, "gpt-4o-mini");
    }

    #[test]
    fn parses_anthropic_response() {
        let body = r#"{"data":[{"id":"claude-sonnet-4-6","display_name":"Claude Sonnet 4.6"}]}"#;
        let parsed: AnthropicModelsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.data[0].id, "claude-sonnet-4-6");
        assert_eq!(parsed.data[0].display_name.as_deref(), Some("Claude Sonnet 4.6"));
    }

    #[test]
    fn parses_gemini_response() {
        let body = r#"{
            "models": [
                {"name":"models/gemini-2.5-flash","displayName":"Gemini 2.5 Flash","supportedGenerationMethods":["generateContent"]},
                {"name":"models/text-embedding-004","displayName":"Embedding","supportedGenerationMethods":["embedContent"]}
            ]
        }"#;
        let parsed: GeminiModelsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.models.len(), 2);
    }

    #[test]
    fn parses_ollama_response() {
        let body = r#"{"models":[{"name":"llama3:8b","size":1000},{"name":"qwen2.5:14b"}]}"#;
        let parsed: OllamaTagsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.models.len(), 2);
        assert_eq!(parsed.models[0].name, "llama3:8b");
    }
}
