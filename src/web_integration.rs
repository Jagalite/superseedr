// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow WASM-only bridge from browser-owned behavior to production reducers and rendering.

use std::path::PathBuf;

use ratatui::Frame;

use crate::app::{App, AppCommand, AppMode, TorrentControlState};
use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
use crate::integrations::control::ControlRequest;
use crate::presentation::{PresentationFixture, PresentationState};
use crate::terminal_event::Event;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserCommand {
    AddMagnet {
        magnet_link: String,
        download_path: Option<PathBuf>,
        container_name: Option<String>,
        validation_status: bool,
    },
    Pause {
        info_hash_hex: String,
    },
    Resume {
        info_hash_hex: String,
    },
    Delete {
        info_hash_hex: String,
        delete_files: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserTorrentControlState {
    Running,
    Paused,
    Deleting,
}

#[derive(Clone, Debug)]
pub struct BrowserTorrentUpdate {
    pub info_hash: Vec<u8>,
    pub torrent_name: String,
    pub torrent_or_magnet: String,
    pub pieces_total: u32,
    pub pieces_completed: u32,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
    pub activity_message: String,
}

pub struct BrowserSession {
    app: App,
    dht_status: DhtStatus,
    dht_wave_telemetry: DhtWaveTelemetry,
}

impl BrowserSession {
    pub fn from_fixture(width: u16, height: u16, fixture: PresentationFixture) -> Self {
        let presentation = PresentationState::from_fixture(width, height, fixture);
        let (app_state, dht_status, dht_wave_telemetry, settings) = presentation.into_parts();
        Self {
            app: App::new(app_state, settings),
            dht_status,
            dht_wave_telemetry,
        }
    }

    pub async fn dispatch_event(&mut self, event: Event) {
        crate::tui::events::handle_event(event, &mut self.app).await;
    }

    pub async fn flush_pending_paste_burst(&mut self) {
        crate::tui::events::flush_pending_paste_burst(&mut self.app).await;
    }

    pub fn draw(&self, frame: &mut Frame) {
        crate::tui::view::draw(
            frame,
            &self.app.app_state,
            &self.dht_status,
            &self.dht_wave_telemetry,
            &self.app.client_configs,
        );
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.app.app_state.screen_area =
            ratatui::layout::Rect::new(0, 0, width.max(1), height.max(1));
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn screen_size(&self) -> (u16, u16) {
        (
            self.app.app_state.screen_area.width,
            self.app.app_state.screen_area.height,
        )
    }

    pub fn drain_commands(&mut self) -> Vec<BrowserCommand> {
        let mut commands = Vec::new();
        while let Ok(command) = self.app.app_command_rx.try_recv() {
            let AppCommand::SubmitControlRequest(request) = command else {
                continue;
            };
            let command = match request {
                ControlRequest::AddMagnet {
                    magnet_link,
                    download_path,
                    container_name,
                    validation_status,
                    ..
                } => BrowserCommand::AddMagnet {
                    magnet_link,
                    download_path,
                    container_name,
                    validation_status,
                },
                ControlRequest::Pause { info_hash_hex } => BrowserCommand::Pause { info_hash_hex },
                ControlRequest::Resume { info_hash_hex } => {
                    BrowserCommand::Resume { info_hash_hex }
                }
                ControlRequest::Delete {
                    info_hash_hex,
                    delete_files,
                } => BrowserCommand::Delete {
                    info_hash_hex,
                    delete_files,
                },
                _ => continue,
            };
            commands.push(command);
        }
        commands
    }

    pub fn upsert_mock_torrent(&mut self, update: BrowserTorrentUpdate) {
        let mut display = self
            .app
            .app_state
            .torrents
            .remove(&update.info_hash)
            .unwrap_or_default();
        display.latest_state.info_hash = update.info_hash.clone();
        display.latest_state.torrent_name = update.torrent_name;
        display.latest_state.torrent_or_magnet = update.torrent_or_magnet;
        display.latest_state.number_of_pieces_total = update.pieces_total;
        display.latest_state.number_of_pieces_completed = update.pieces_completed;
        display.latest_state.download_speed_bps = update.download_speed_bps;
        display.latest_state.upload_speed_bps = update.upload_speed_bps;
        display.latest_state.activity_message = update.activity_message;
        display.latest_state.torrent_control_state = TorrentControlState::Running;
        display.smoothed_download_speed_bps = display.latest_state.download_speed_bps;
        display.smoothed_upload_speed_bps = display.latest_state.upload_speed_bps;
        self.app
            .app_state
            .torrents
            .insert(update.info_hash.clone(), display);
        if !self
            .app
            .app_state
            .torrent_list_order
            .contains(&update.info_hash)
        {
            self.app.app_state.torrent_list_order.push(update.info_hash);
        }
        self.app.app_state.ui.needs_redraw = true;
    }

    pub fn set_torrent_paused_hex(&mut self, info_hash_hex: &str, paused: bool) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let Some(torrent) = self.app.app_state.torrents.get_mut(&info_hash) else {
            return false;
        };
        torrent.latest_state.torrent_control_state = if paused {
            TorrentControlState::Paused
        } else {
            TorrentControlState::Running
        };
        self.app.app_state.ui.needs_redraw = true;
        true
    }

    pub fn remove_torrent_hex(&mut self, info_hash_hex: &str) -> bool {
        let Ok(info_hash) = hex::decode(info_hash_hex) else {
            return false;
        };
        let removed = self.app.app_state.torrents.remove(&info_hash).is_some();
        self.app
            .app_state
            .torrent_list_order
            .retain(|candidate| candidate != &info_hash);
        if self.app.app_state.torrent_list_order.is_empty() {
            self.app.app_state.ui.selected_torrent_index = 0;
        } else {
            self.app.app_state.ui.selected_torrent_index = self
                .app
                .app_state
                .ui
                .selected_torrent_index
                .min(self.app.app_state.torrent_list_order.len() - 1);
        }
        self.app.app_state.ui.needs_redraw = true;
        removed
    }

    pub fn torrent_control_state_hex(
        &self,
        info_hash_hex: &str,
    ) -> Option<BrowserTorrentControlState> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app.app_state.torrents.get(&info_hash).map(|torrent| {
            match torrent.latest_state.torrent_control_state {
                TorrentControlState::Running => BrowserTorrentControlState::Running,
                TorrentControlState::Paused => BrowserTorrentControlState::Paused,
                TorrentControlState::Deleting => BrowserTorrentControlState::Deleting,
            }
        })
    }

    pub fn torrent_delete_files_hex(&self, info_hash_hex: &str) -> Option<bool> {
        let info_hash = hex::decode(info_hash_hex).ok()?;
        self.app
            .app_state
            .torrents
            .get(&info_hash)
            .map(|torrent| torrent.latest_state.delete_files)
    }

    pub fn delete_confirmation(&self) -> Option<(&[u8], bool)> {
        matches!(self.app.app_state.mode, AppMode::DeleteConfirm).then_some((
            self.app.app_state.ui.delete_confirm.info_hash.as_slice(),
            self.app.app_state.ui.delete_confirm.with_files,
        ))
    }

    pub fn torrent_count(&self) -> usize {
        self.app.app_state.torrents.len()
    }
}
