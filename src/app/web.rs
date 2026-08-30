// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal WASM-selected application surface for production TUI reducers.
//!
//! This type owns shared state, settings, and the existing `AppCommand` channel only. Browser
//! fixtures, command fulfillment, timers, rendering cadence, and browser integration remain under
//! `web`.

use tokio::sync::mpsc;

use super::{AppCommand, AppState};
use crate::config::Settings;

pub(crate) struct WebApp {
    pub app_state: AppState,
    pub client_configs: Settings,
    pub app_command_tx: mpsc::Sender<AppCommand>,
    pub app_command_rx: mpsc::Receiver<AppCommand>,
}

impl WebApp {
    pub(crate) fn new(app_state: AppState, client_configs: Settings) -> Self {
        let (app_command_tx, app_command_rx) = mpsc::channel(32);
        Self {
            app_state,
            client_configs,
            app_command_tx,
            app_command_rx,
        }
    }

    pub(crate) fn try_send_command(&self, command: AppCommand) {
        let _ = self.app_command_tx.try_send(command);
    }
}
