//! warp-cn fork: thin dispatch layer that lets `ServerApi` short-circuit AI
//! requests to a user-configured LLM provider instead of Warp cloud.
//!
//! The configuration (per-provider base_url + model_id + active provider) is
//! owned by `ai::direct_backend::DirectBackendConfig`; the secret keys are
//! owned by `ai::api_keys::ApiKeyManager`. This module only assembles them
//! into a request-time [`LlmBackend`] and exposes [`active_backend`] for the
//! three GraphQL AI methods on `ServerApi` to early-return through.
//!
//! Stream-bearing endpoints (`generate_multi_agent_output`) are wired in M4.

pub mod anthropic;
pub mod common;
pub mod computer_use_adapter;
pub mod gemini;
pub mod model_catalog;
pub mod multi_agent;
pub mod openai;

use std::sync::Arc;

use ai::api_keys::ApiKeyManager;
use ai::direct_backend::{DirectBackendConfig, DirectProviderKind, ProviderOverrides};
use async_trait::async_trait;
use warp_core::features::FeatureFlag;
use warpui::{AppContext, SingletonEntity};

use crate::ai_assistant::{
    requests::GenerateDialogueResult, utils::TranscriptPart, AIGeneratedCommand,
    GenerateCommandsFromNaturalLanguageError,
};
use crate::drive::workflows::ai_assist::{GeneratedCommandMetadata, GeneratedCommandMetadataError};

/// Resolved provider configuration ready to be sent on the wire.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub kind: DirectProviderKind,
    pub api_key: String,
    pub base_url: String,
    pub model_id: String,
}

/// Trait every provider client implements. Mirrors the three "simple LLM"
/// endpoints on `AIClient`; multi-agent streaming is handled separately so
/// providers without tool-calling support (or M2-only setups) can opt out.
#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
pub trait LlmBackend: Send + Sync + 'static {
    async fn generate_dialogue_answer(
        &self,
        transcript: Vec<TranscriptPart>,
        prompt: String,
    ) -> anyhow::Result<GenerateDialogueResult>;

    async fn generate_commands_from_natural_language(
        &self,
        prompt: String,
    ) -> Result<Vec<AIGeneratedCommand>, GenerateCommandsFromNaturalLanguageError>;

    async fn generate_metadata_for_command(
        &self,
        command: String,
    ) -> Result<GeneratedCommandMetadata, GeneratedCommandMetadataError>;
}

/// Returns the active backend, or `None` if Direct mode can't be assembled
/// (no compile-time feature, no runtime flag, or no credentials anywhere).
///
/// Resolution order:
///   1. `DirectBackendConfig` (settings-page UI: per-provider Base URL +
///      optional API key overrides) merged with `ApiKeyManager` keys.
///   2. `WARP_CN_*` env vars (headless / dev / QA entry point).
///
/// Active provider is auto-derived: whichever key in `ApiKeyManager` (or the
/// `DirectBackendConfig` override) is non-empty wins, in priority order
/// Anthropic > OpenAI > Gemini. The settings UI no longer needs a master
/// toggle or "Set Active" button — having a key implies "use it".
pub fn active_backend(ctx: &AppContext) -> Option<Arc<dyn LlmBackend>> {
    if !FeatureFlag::DirectLlmBackend.is_enabled() {
        return None;
    }
    let config = DirectBackendConfig::as_ref(ctx);
    let api_keys = ApiKeyManager::as_ref(ctx);
    let resolved = resolve(config, api_keys).or_else(resolve_from_env)?;
    instantiate(resolved)
}

fn instantiate(resolved: ResolvedProvider) -> Option<Arc<dyn LlmBackend>> {
    match resolved.kind {
        DirectProviderKind::OpenAi | DirectProviderKind::OpenAiCompatible => {
            Some(Arc::new(openai::OpenAiBackend::new(resolved)))
        }
        DirectProviderKind::Anthropic => Some(Arc::new(anthropic::AnthropicBackend::new(resolved))),
        DirectProviderKind::Gemini => Some(Arc::new(gemini::GeminiBackend::new(resolved))),
    }
}

/// Read provider config from the `WARP_CN_*` env vars. Intended for dev/QA
/// before the settings UI ships in M3; users with a populated
/// `DirectBackendConfig` always take precedence.
fn resolve_from_env() -> Option<ResolvedProvider> {
    let kind = match std::env::var("WARP_CN_DIRECT_PROVIDER").ok()?.as_str() {
        "openai" => DirectProviderKind::OpenAi,
        "openai-compatible" | "compat" => DirectProviderKind::OpenAiCompatible,
        "anthropic" => DirectProviderKind::Anthropic,
        "gemini" => DirectProviderKind::Gemini,
        _ => return None,
    };
    let api_key = std::env::var("WARP_CN_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())?;
    let base_url = std::env::var("WARP_CN_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| default_base_url(kind).to_string());
    let model_id = std::env::var("WARP_CN_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_model_id(kind).to_string());
    Some(ResolvedProvider {
        kind,
        api_key,
        base_url,
        model_id,
    })
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

fn default_model_id(kind: DirectProviderKind) -> &'static str {
    match kind {
        DirectProviderKind::OpenAi | DirectProviderKind::OpenAiCompatible => "gpt-4o-mini",
        DirectProviderKind::Anthropic => "claude-sonnet-4-6",
        DirectProviderKind::Gemini => "gemini-2.5-flash",
    }
}

fn resolve(config: &DirectBackendConfig, api_keys: &ApiKeyManager) -> Option<ResolvedProvider> {
    // Auto-derive: whichever provider has a usable key wins. Priority order
    // is Anthropic > OpenAI > Gemini (Claude is the strongest tool-using
    // model in 2026); within each, override key beats `ApiKeyManager` key.
    // OpenAI-compatible is handled implicitly: if the OpenAI Base URL is
    // overridden to a non-default host, the user is pointing at a gateway
    // and we still dispatch through the OpenAI client.
    for kind in [
        DirectProviderKind::Anthropic,
        DirectProviderKind::OpenAi,
        DirectProviderKind::Gemini,
    ] {
        let overrides = config.overrides_for(kind);
        if let Some(api_key) = pick_api_key(kind, overrides, api_keys) {
            return Some(ResolvedProvider {
                kind,
                api_key,
                base_url: pick_base_url(kind, overrides),
                model_id: pick_model_id(kind, overrides),
            });
        }
    }
    None
}

/// Picks the API key. Priority: per-provider override (filled via Direct LLM
/// settings UI) → upstream `ApiKeyManager` (BYOK store, unlocked by our fork
/// patch). Returns `None` only if neither has a non-empty value.
///
/// `OpenAiCompatible` deliberately does NOT fall back to the OpenAI-slot key
/// in `ApiKeyManager`: the same physical key can't realistically be both an
/// OpenAI key and a DeepSeek/Qwen key. Compatible-gateway users must configure
/// the key directly in the override.
fn pick_api_key(
    kind: DirectProviderKind,
    overrides: &ProviderOverrides,
    api_keys: &ApiKeyManager,
) -> Option<String> {
    let trimmed_override = overrides.api_key.trim();
    if !trimmed_override.is_empty() {
        return Some(trimmed_override.to_owned());
    }
    let key_owner = api_keys.keys();
    match kind {
        DirectProviderKind::OpenAi => key_owner.openai.clone().filter(|s| !s.is_empty()),
        DirectProviderKind::Anthropic => key_owner.anthropic.clone().filter(|s| !s.is_empty()),
        DirectProviderKind::Gemini => key_owner.google.clone().filter(|s| !s.is_empty()),
        DirectProviderKind::OpenAiCompatible => None,
    }
}

fn pick_base_url(kind: DirectProviderKind, overrides: &ProviderOverrides) -> String {
    let trimmed = overrides.base_url.trim().trim_end_matches('/').to_owned();
    if !trimmed.is_empty() {
        return trimmed;
    }
    default_base_url(kind).to_string()
}

fn pick_model_id(kind: DirectProviderKind, overrides: &ProviderOverrides) -> String {
    let trimmed = overrides.model_id.trim().to_owned();
    if !trimmed.is_empty() {
        return trimmed;
    }
    default_model_id(kind).to_string()
}
