//! Backend selection for codebase indexing.
//!
//! Single source of truth: when upstream renames `FullSourceCodeEmbedding` or
//! reshapes the gate logic, only this module needs to change.

use settings::Setting;
use warpui::{AppContext, SingletonEntity};

use crate::{
    ai::request_usage_model::{AIRequestUsageModel, CodebaseContextUsageLimit},
    auth::AuthStateProvider,
    features::FeatureFlag,
    settings::CodeSettings,
    workspaces::user_workspaces::UserWorkspaces,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodebaseIndexBackend {
    WarpCloud,
    Auggie,
}

/// Selects the active codebase index backend.
///
/// - `WarpCloud` when the user is logged in and `FullSourceCodeEmbedding` is enabled.
/// - `Auggie` when `AuggieCodebaseIndex` is enabled (regardless of login state)
///   AND the Auggie implementation is compiled in for this target.
/// - `None` otherwise.
pub(crate) fn codebase_index_backend(ctx: &AppContext) -> Option<CodebaseIndexBackend> {
    let auth = AuthStateProvider::as_ref(ctx).get();
    if auth.is_logged_in() && FeatureFlag::FullSourceCodeEmbedding.is_enabled() {
        Some(CodebaseIndexBackend::WarpCloud)
    } else if cfg!(all(
        not(target_family = "wasm"),
        feature = "auggie_codebase_index"
    )) && FeatureFlag::AuggieCodebaseIndex.is_enabled()
    {
        Some(CodebaseIndexBackend::Auggie)
    } else {
        None
    }
}

pub(crate) fn is_local_codebase_index_backend(ctx: &AppContext) -> bool {
    codebase_index_backend(ctx) == Some(CodebaseIndexBackend::Auggie)
}

pub(crate) fn is_codebase_index_feature_available(ctx: &AppContext) -> bool {
    codebase_index_backend(ctx).is_some()
}

/// `true` when the active backend is Auggie AND a previous spawn attempt
/// failed. Used by the settings UI to surface the "auggie unavailable"
/// tooltip without forcing an eager spawn at render time.
pub(crate) fn is_auggie_backend_unavailable(ctx: &AppContext) -> bool {
    if codebase_index_backend(ctx) != Some(CodebaseIndexBackend::Auggie) {
        return false;
    }
    #[cfg(all(not(target_family = "wasm"), feature = "auggie_codebase_index"))]
    {
        crate::ai::auggie_mcp::AuggieMcpClientModel::as_ref(ctx).is_unavailable()
    }
    #[cfg(not(all(not(target_family = "wasm"), feature = "auggie_codebase_index")))]
    {
        false
    }
}

/// Whether codebase indexing should be enabled for the active backend.
///
/// - `Auggie`: only the user toggle (`code.indexing.agent_mode_codebase_context`); no global-AI gate.
/// - `WarpCloud`: existing `UserWorkspaces::is_codebase_context_enabled` (org policy + global AI + user toggle).
/// - `None`: disabled.
pub(crate) fn is_codebase_context_enabled_for_indexing(ctx: &AppContext) -> bool {
    match codebase_index_backend(ctx) {
        Some(CodebaseIndexBackend::Auggie) => {
            *CodeSettings::as_ref(ctx).codebase_context_enabled.value()
        }
        Some(CodebaseIndexBackend::WarpCloud) => {
            UserWorkspaces::as_ref(ctx).is_codebase_context_enabled(ctx)
        }
        None => false,
    }
}

/// Returns codebase context usage limits for the active backend.
///
/// - `Auggie`: local defaults (unlimited indices, 10k files/repo, batch 100).
/// - `WarpCloud` / `None`: server-driven limits from `AIRequestUsageModel`.
pub(crate) fn codebase_context_limits_for_backend(ctx: &AppContext) -> CodebaseContextUsageLimit {
    match codebase_index_backend(ctx) {
        Some(CodebaseIndexBackend::Auggie) => CodebaseContextUsageLimit {
            max_indices_allowed: None,
            max_files_per_repo: 10_000,
            embedding_generation_batch_size: 100,
        },
        Some(CodebaseIndexBackend::WarpCloud) | None => {
            AIRequestUsageModel::as_ref(ctx).codebase_context_limits()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::server_api::ServerApiProvider;
    use warpui::App;

    fn install_auth(app: &mut App, logged_in: bool) {
        if logged_in {
            app.add_singleton_model(|_| AuthStateProvider::new_for_test());
        } else {
            app.add_singleton_model(|_| AuthStateProvider::new_logged_out_for_test());
        }
    }

    fn install_request_usage_model(app: &mut App) {
        app.add_singleton_model(|_| ServerApiProvider::new_for_test());
        app.add_singleton_model(|ctx| {
            AIRequestUsageModel::new_for_test(ServerApiProvider::as_ref(ctx).get_ai_client(), ctx)
        });
    }

    fn assert_backend(
        logged_in: bool,
        cloud: bool,
        auggie: bool,
        expected: Option<CodebaseIndexBackend>,
    ) {
        App::test((), |mut app| async move {
            let _c = FeatureFlag::FullSourceCodeEmbedding.override_enabled(cloud);
            let _a = FeatureFlag::AuggieCodebaseIndex.override_enabled(auggie);
            install_auth(&mut app, logged_in);
            app.read(|ctx| assert_eq!(codebase_index_backend(ctx), expected));
        });
    }

    #[test]
    fn cloud_when_logged_in_and_cloud_flag_on() {
        assert_backend(true, true, false, Some(CodebaseIndexBackend::WarpCloud));
    }

    #[test]
    fn cloud_wins_when_logged_in_and_both_flags_on() {
        assert_backend(true, true, true, Some(CodebaseIndexBackend::WarpCloud));
    }

    #[test]
    fn auggie_when_only_auggie_flag_on() {
        assert_backend(true, false, true, Some(CodebaseIndexBackend::Auggie));
    }

    #[test]
    fn auggie_when_logged_out_even_if_cloud_flag_on() {
        assert_backend(false, true, true, Some(CodebaseIndexBackend::Auggie));
    }

    #[test]
    fn none_when_no_flags_on() {
        assert_backend(false, false, false, None);
    }

    #[test]
    fn auggie_limits_when_flag_on_and_logged_out() {
        App::test((), |mut app| async move {
            let _c = FeatureFlag::FullSourceCodeEmbedding.override_enabled(false);
            let _a = FeatureFlag::AuggieCodebaseIndex.override_enabled(true);
            install_auth(&mut app, false);

            app.read(|ctx| {
                let limits = codebase_context_limits_for_backend(ctx);
                assert_eq!(limits.max_indices_allowed, None);
                assert_eq!(limits.max_files_per_repo, 10_000);
                assert_eq!(limits.embedding_generation_batch_size, 100);
            });
        });
    }

    #[test]
    fn cloud_limits_when_logged_in_and_cloud_flag_on() {
        App::test((), |mut app| async move {
            let _c = FeatureFlag::FullSourceCodeEmbedding.override_enabled(true);
            let _a = FeatureFlag::AuggieCodebaseIndex.override_enabled(false);
            install_auth(&mut app, true);
            install_request_usage_model(&mut app);

            app.read(|ctx| {
                let limits = codebase_context_limits_for_backend(ctx);
                assert_eq!(limits.max_indices_allowed, Some(3));
                assert_eq!(limits.max_files_per_repo, 5000);
                assert_eq!(limits.embedding_generation_batch_size, 100);
            });
        });
    }

    // Group 7 will add integration coverage for
    // `is_codebase_context_enabled_for_indexing` against the full settings stack.

    #[cfg(all(not(target_family = "wasm"), feature = "auggie_codebase_index"))]
    #[test]
    fn auggie_unavailable_reflects_recorded_spawn_failure() {
        use crate::ai::auggie_mcp::AuggieMcpClientModel;

        App::test((), |mut app| async move {
            let _c = FeatureFlag::FullSourceCodeEmbedding.override_enabled(false);
            let _a = FeatureFlag::AuggieCodebaseIndex.override_enabled(true);
            install_auth(&mut app, false);
            app.add_singleton_model(|_| AuggieMcpClientModel::new_for_test_unavailable());

            app.read(|ctx| assert!(is_auggie_backend_unavailable(ctx)));
        });
    }

    #[cfg(all(not(target_family = "wasm"), feature = "auggie_codebase_index"))]
    #[test]
    fn auggie_unavailable_false_when_backend_not_auggie() {
        App::test((), |mut app| async move {
            let _c = FeatureFlag::FullSourceCodeEmbedding.override_enabled(false);
            let _a = FeatureFlag::AuggieCodebaseIndex.override_enabled(false);
            install_auth(&mut app, false);

            app.read(|ctx| assert!(!is_auggie_backend_unavailable(ctx)));
        });
    }
}
