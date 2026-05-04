//! Startup auto-check + active-window toast notification for GitHub releases.
//!
//! Listens for `GithubUpdateState` transitions; when a check resolves to
//! `UpdateAvailable`, surfaces one persistent toast in the active window.
//! Idempotent within a session — at most one toast per warp launch. Dismissing
//! the toast leaves no persistent state, so the next launch re-checks and
//! re-notifies if the update is still pending. This satisfies the desired UX:
//! "click [Update] → goes to Settings → About; click [Later] / dismiss → next
//! launch reminds again".

use std::time::Duration;

use warpui::windowing::WindowManager;
use warpui::{AppContext, Entity, ModelContext, SingletonEntity, Timer};

use crate::view_components::DismissibleToast;
use crate::workspace::toast_stack::ToastStack;

use super::GithubUpdateState;

/// Delay between init and first GitHub check. Long enough for the main window
/// to be fully constructed (so `WindowManager::active_window()` is populated)
/// without making the user wait noticeably.
const STARTUP_DELAY: Duration = Duration::from_secs(5);

pub(crate) struct UpdateNotificationModel {
    shown_in_session: bool,
}

impl UpdateNotificationModel {
    fn new(ctx: &mut ModelContext<Self>) -> Self {
        let state_handle = GithubUpdateState::handle(ctx);
        ctx.subscribe_to_model(&state_handle, Self::on_state_change);

        // Schedule one-shot check after the main window has had a chance to
        // come up. Triggered via `GithubUpdateState::trigger_check`, which
        // already debounces re-entrancy and consults the 24h SHA→tag cache,
        // so this never burns extra GitHub rate-limit budget.
        ctx.spawn(
            async {
                Timer::after(STARTUP_DELAY).await;
            },
            |_self, _, ctx| {
                GithubUpdateState::trigger_check(ctx);
            },
        );

        Self {
            shown_in_session: false,
        }
    }

    fn on_state_change(&mut self, _event: &(), ctx: &mut ModelContext<Self>) {
        if self.shown_in_session {
            return;
        }
        let state = GithubUpdateState::as_ref(ctx).clone();
        if let GithubUpdateState::UpdateAvailable { tag, .. } = state {
            self.shown_in_session = true;
            self.push_toast(tag, ctx);
        }
    }

    fn push_toast(&self, tag: String, ctx: &mut ModelContext<Self>) {
        let Some(window_id) = WindowManager::as_ref(ctx).active_window() else {
            log::warn!("auto-update toast skipped: no active window at notify time");
            return;
        };
        let text = warp_i18n::t!(
            "settings-account-update-toast-available",
            tag = tag.as_str()
        )
        .to_string();
        let toast = DismissibleToast::default(text);
        ToastStack::handle(ctx).update(ctx, |stack, ctx| {
            stack.add_persistent_toast(toast, window_id, ctx);
        });
    }
}

impl Entity for UpdateNotificationModel {
    type Event = ();
}

impl SingletonEntity for UpdateNotificationModel {}

/// Register the notification model. Construction schedules the startup check
/// and subscribes to state transitions.
pub(crate) fn register(ctx: &mut AppContext) {
    ctx.add_singleton_model(UpdateNotificationModel::new);
}
