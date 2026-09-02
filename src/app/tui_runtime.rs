// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Target selector and shared input driver for the native/browser TUI runtimes.

use super::{App, AppCommand};
use crate::terminal_event::Event;
use web_time::Instant;

#[cfg(target_arch = "wasm32")]
#[path = "tui_runtime/browser.rs"]
mod browser;
#[cfg(not(target_arch = "wasm32"))]
#[path = "tui_runtime/native.rs"]
mod native;

#[cfg(target_arch = "wasm32")]
use browser as platform;
#[cfg(not(target_arch = "wasm32"))]
use native as platform;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::native_pasted_text_supported;
#[cfg(all(not(target_arch = "wasm32"), test))]
pub(crate) use native::spawn_app_command_batch_sender;
#[cfg(all(not(target_arch = "wasm32"), test))]
pub(crate) use native::{
    execute_browser_dialog_effects, execute_browser_fs_effects, execute_normal_effects,
};

#[cfg(all(not(target_arch = "wasm32"), test))]
pub(crate) async fn execute_native_confirm_decision(
    app: &mut App,
    decision: crate::tui::effects::ConfirmDecision,
) -> Option<crate::tui::effects::BrowserTransition> {
    native::execute_native_confirm_decision(app, decision).await
}

pub(crate) async fn handle_event(app: &mut App, event: Event) {
    handle_event_at(event, app, Instant::now()).await;
}

pub(crate) async fn flush_pending_paste_burst(app: &mut App) {
    flush_pending_paste_burst_at(app, Instant::now()).await;
}

pub(crate) async fn handle_event_at(event: Event, app: &mut App, now: Instant) {
    let pending_text = crate::tui::events::pending_paste_text_before_event(&event, &app.app_state)
        .map(str::to_owned);
    let pending_is_paste = pending_text
        .as_deref()
        .is_some_and(|text| app.accepts_pasted_text(text));
    let translated =
        crate::tui::events::translate_event(event, &mut app.app_state, now, pending_is_paste);
    reduce_and_execute_events(app, translated).await;
}

pub(crate) async fn flush_pending_paste_burst_at(app: &mut App, now: Instant) {
    let pending_text = crate::tui::events::due_paste_text(&app.app_state, now).map(str::to_owned);
    let pending_is_paste = pending_text
        .as_deref()
        .is_some_and(|text| app.accepts_pasted_text(text));
    let translated =
        crate::tui::events::flush_due_events(&mut app.app_state, now, pending_is_paste);
    reduce_and_execute_events(app, translated).await;
}

async fn reduce_and_execute_events(app: &mut App, events: Vec<Event>) {
    if events.is_empty() {
        return;
    }

    for event in events {
        let settings = app.client_configs.clone();
        let shared_follower = app.is_current_shared_follower();
        let effects =
            crate::tui::events::reduce_event(event, &mut app.app_state, &settings, shared_follower);
        platform::execute_tui_effects(app, effects).await;
    }
    app.app_state.ui.needs_redraw = true;
}
