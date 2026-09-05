// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host runtime; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    /// Starts orderly app shutdown. The caller must persist the returned snapshot and
    /// report its result; neither closing a page nor queueing a write proves durability.
    pub fn request_shutdown(&mut self, observed_unix_secs: u64) -> crate::app::PersistPayload {
        self.app_state.should_quit = true;
        if self.app_state.lifecycle.phase == crate::app::AppPhase::Running {
            self.unsent_shutdowns = self.torrent_manager_command_txs.keys().cloned().collect();
            self.app_state
                .lifecycle
                .begin_shutdown(self.unsent_shutdowns.iter().cloned());
        }
        let checkpoint = self.prepare_checkpoint(observed_unix_secs);
        // Consume already queued terminal observations before treating a closed command
        // channel as an unacknowledged manager failure.
        self.drain_manager_messages();
        checkpoint
    }

    /// True once teardown has finished with a manager or host failure.
    pub fn shutdown_failed(&self) -> bool {
        self.app_state.lifecycle.phase == crate::app::AppPhase::Incomplete
    }

    pub(super) fn retry_pending_shutdowns(&mut self) {
        if self.app_state.lifecycle.phase != crate::app::AppPhase::Stopping {
            return;
        }
        for hash in self.unsent_shutdowns.clone() {
            let result = self
                .torrent_manager_command_txs
                .get(&hash)
                .map(|sender| sender.try_send(ManagerCommand::Shutdown));
            match result {
                Some(Ok(())) => {
                    self.unsent_shutdowns.remove(&hash);
                }
                Some(Err(mpsc::error::TrySendError::Full(_))) => {}
                Some(Err(mpsc::error::TrySendError::Closed(_))) | None => {
                    // Effect backpressure can defer an acknowledgement already in the event
                    // queue. Let the host drain its effects before declaring a missing result.
                    if !self.manager_event_rx.is_empty() {
                        continue;
                    }
                    let message = format!(
                        "Manager command channel closed during shutdown: {}",
                        hex::encode(&hash)
                    );
                    self.app_state
                        .lifecycle
                        .manager_stopped(&hash, Err(message.clone()));
                    self.set_browser_error(message);
                    self.release_torrent_runtime(&hash, false);
                }
            }
        }
        self.finish_shutdown_if_ready();
    }

    pub fn shutdown_complete(&self) -> bool {
        self.app_state.lifecycle.phase == crate::app::AppPhase::Stopped
    }

    pub(super) fn finish_shutdown_if_ready(&mut self) {
        if self.app_state.lifecycle.phase != crate::app::AppPhase::Running
            && self.app_state.lifecycle.pending_count() == 0
            && !self.app_state.checkpoint.is_dirty()
        {
            self.app_state.lifecycle.finish(true);
        }
        self.app_state.shutdown_progress = self.app_state.lifecycle.progress();
    }

    pub async fn dispatch_event(&mut self, event: Event) {
        if self.app_state.lifecycle.phase != crate::app::AppPhase::Running {
            return;
        }
        crate::tui::runtime::handle_event(self, event).await;
        self.sync_browser_torrent_preview_request();
    }

    pub async fn flush_pending_paste_burst(&mut self) {
        crate::tui::runtime::flush_pending_paste_burst(self).await;
        self.sync_browser_torrent_preview_request();
    }

    pub fn draw(&self, frame: &mut Frame) {
        crate::tui::render::draw(
            frame,
            &self.app_state,
            &self.dht_status,
            &self.dht_wave_telemetry,
            &self.client_configs,
        );
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.app_state.screen_area = ratatui::layout::Rect::new(0, 0, width.max(1), height.max(1));
        self.app_state.ui.needs_redraw = true;
    }

    pub fn set_screen(&mut self, screen: BrowserScreen) {
        match screen {
            BrowserScreen::Config => {
                *self.app_state.ui.config.settings_edit = self.client_configs.clone();
                self.app_state.ui.config.selected_index = 0;
                self.app_state.ui.config.items = ConfigItem::iter().collect();
                self.refresh_browser_network_interfaces();
            }
            BrowserScreen::DeleteConfirm => {
                if let Some(info_hash) = self.app_state.torrent_list_order.first() {
                    self.app_state.ui.delete_confirm.info_hash = info_hash.clone();
                    self.app_state.ui.delete_confirm.with_files = false;
                }
            }
            BrowserScreen::TorrentManagement => {
                crate::tui::screens::torrents::initialize_torrent_management_cursor(
                    &mut self.app_state,
                );
            }
            BrowserScreen::FileBrowser => {
                self.app_state.ui.file_browser.browser_mode = FileBrowserMode::Directory;
            }
            _ => {}
        }
        self.app_state.mode = match screen {
            BrowserScreen::Welcome => AppMode::Welcome,
            BrowserScreen::Normal => AppMode::Normal,
            BrowserScreen::Help => AppMode::Help,
            BrowserScreen::Journal => AppMode::Journal,
            BrowserScreen::PeerManagement => AppMode::PeerManagement,
            BrowserScreen::TorrentManagement => AppMode::TorrentManagement,
            BrowserScreen::PowerSaving => AppMode::PowerSaving,
            BrowserScreen::DeleteConfirm => AppMode::DeleteConfirm,
            BrowserScreen::Config => AppMode::Config,
            BrowserScreen::FileBrowser => AppMode::FileBrowser,
            BrowserScreen::Rss => AppMode::Rss,
        };
        self.app_state.ui.needs_redraw = true;
    }

    pub fn set_browser_error(&mut self, message: impl Into<String>) {
        self.app_state.system_error = Some(message.into());
        self.app_state.ui.needs_redraw = true;
    }

    pub fn clear_browser_error(&mut self) {
        self.app_state.system_error = None;
        self.app_state.ui.needs_redraw = true;
    }
}
