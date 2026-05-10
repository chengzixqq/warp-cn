//! warp-cn fork: bypass-Warp-cloud LLM backend.
//!
//! This module is the dispatch surface used by `app::server::server_api::*` to
//! short-circuit AI requests when the user has enabled
//! [`FeatureFlag::DirectLlmBackend`] *and* configured at least one provider.
//!
//! ## Architecture
//!
//! `ApiKeyManager` (upstream) stores secret keys; [`config::DirectBackendConfig`]
//! (this module) stores per-provider `base_url` + `model_id` overrides plus the
//! "active provider" selection. The two are intentionally separate so the
//! upstream secure-storage payload of API keys is unchanged and we don't
//! conflict with future Warp BYOK schema migrations.
//!
//! M1 ships only the configuration plumbing; M2 wires in the OpenAI Chat
//! Completions client and the first early-return hook in
//! `ServerApi::generate_dialogue_answer`.

pub mod config;

pub use config::{
    current_snapshot, DirectBackendConfig, DirectBackendConfigEvent, DirectBackendState,
    DirectProviderKind, ProviderOverrides,
};
