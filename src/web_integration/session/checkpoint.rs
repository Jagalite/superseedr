// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host checkpoint; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    pub fn prepare_checkpoint(&mut self, observed_unix_secs: u64) -> crate::app::PersistPayload {
        // Only catalog entries hydrated before their runtime starts are deferred.
        // An intentionally removed runtime must never be inferred to need restoration.
        let deferred = self
            .pending_catalog_restores
            .iter()
            .filter(|hash| !self.app_state.torrents.contains_key(*hash))
            .cloned()
            .collect();
        crate::app::prepare_checkpoint(
            &mut self.client_configs,
            &mut self.app_state,
            &deferred,
            observed_unix_secs,
        )
    }

    pub fn complete_checkpoint(&mut self, revision: u64, result: Result<(), String>) {
        crate::app::reduce_app_action(
            &mut self.app_state,
            crate::app::AppAction::CheckpointCompleted { revision, result },
        );
        self.finish_shutdown_if_ready();
    }

    pub fn has_uncommitted_checkpoint(&self) -> bool {
        self.app_state.checkpoint.is_dirty()
    }
}
