// SPDX-License-Identifier: GPL-3.0-or-later
//! Application transitions for live managers. The engine host executes physical effects.
use super::*;

pub(super) enum ControlEffect {
    Accepted,
    Restart { source: String },
    RemoveStopped { delete_files: bool },
}

impl BrowserSession {
    /// Sources come from the application catalog, never from manager telemetry.
    pub(super) fn publish_catalog_row(&mut self, metrics: TorrentMetrics) {
        let hash = metrics.info_hash.clone();
        let source = metrics.torrent_or_magnet.clone();
        let control = metrics.torrent_control_state.clone();
        let effects = crate::app::reduce_app_action(
            &mut self.app_state,
            crate::app::AppAction::ManagerMetrics(Box::new(metrics)),
        );
        for effect in effects {
            self.execute_app_effect(effect);
        }
        if let Some(display) = self.app_state.torrents.get_mut(&hash) {
            display.latest_state.torrent_or_magnet = source;
            display.latest_state.torrent_control_state = control;
        }
    }

    pub(super) fn remember_catalog_torrent(
        &mut self,
        hash: &[u8],
        settings: crate::config::TorrentSettings,
    ) {
        self.client_configs.torrents.retain(|entry| {
            crate::torrent_identity::info_hash_from_torrent_source(&entry.torrent_or_magnet)
                .as_deref()
                != Some(hash)
        });
        self.client_configs.torrents.push(settings);
        self.failed_managers.remove(hash);
        self.note_torrent_added();
    }

    pub(super) fn request_manager_control(
        &mut self,
        hash: &[u8],
        command: ManagerCommand,
    ) -> Result<ControlEffect, String> {
        if self.failed_managers.contains_key(hash) {
            return match command {
                ManagerCommand::Shutdown => Ok(ControlEffect::RemoveStopped {
                    delete_files: false,
                }),
                ManagerCommand::DeleteFile => {
                    Ok(ControlEffect::RemoveStopped { delete_files: true })
                }
                ManagerCommand::Resume => {
                    let source = self
                        .app_state
                        .torrents
                        .get(hash)
                        .ok_or("Torrent is unavailable")?
                        .latest_state
                        .torrent_or_magnet
                        .clone();
                    Ok(ControlEffect::Restart { source })
                }
                ManagerCommand::Pause => Ok(ControlEffect::Accepted),
                _ => Err("Torrent manager stopped; retry or remove the torrent".into()),
            };
        }
        let control = match &command {
            ManagerCommand::Pause => Some(TorrentControlState::Paused),
            ManagerCommand::Resume => Some(TorrentControlState::Running),
            _ => None,
        };
        if !self.send_manager_command(hash, command) {
            return Err("Manager command was not accepted".into());
        }
        if let Some(control) = control {
            if let Some(display) = self.app_state.torrents.get_mut(hash) {
                display.latest_state.torrent_control_state = control;
            }
            self.checkpoint_requested = true;
        }
        Ok(ControlEffect::Accepted)
    }
    pub(super) fn manager_finished(
        &mut self,
        hash: Vec<u8>,
        source: crate::app::ManagerSource,
        result: Result<(), String>,
    ) {
        // Apply terminal observations first; normal removals release their source.
        self.drain_manager_messages();
        if !source.is_current() {
            // A terminal event can release registration before the final payload
            // close returns. Still surface a late physical cleanup failure.
            if let Err(error) = result {
                self.set_browser_error(error);
                if self.app_state.lifecycle.phase != crate::app::AppPhase::Running {
                    self.app_state.lifecycle.host_cleanup_failed = true;
                }
            }
            return;
        }
        if self.app_state.lifecycle.phase == crate::app::AppPhase::Stopping {
            self.app_state
                .lifecycle
                .manager_stopped(&hash, result.clone());
        }
        self.release_torrent_runtime(&hash, false);
        let error = result
            .err()
            .unwrap_or_else(|| "Torrent manager stopped; retry or remove the torrent".into());
        self.record_manager_failure(hash, error);
    }
    pub(super) fn record_manager_failure(&mut self, hash: Vec<u8>, error: String) {
        let saved = self
            .client_configs
            .torrents
            .iter()
            .find(|entry| {
                crate::torrent_identity::info_hash_from_torrent_source(&entry.torrent_or_magnet)
                    .as_deref()
                    == Some(&hash)
            })
            .cloned();
        if !self.app_state.torrents.contains_key(&hash) {
            if let Some(saved) = &saved {
                self.publish_catalog_row(TorrentMetrics {
                    info_hash: hash.clone(),
                    torrent_or_magnet: saved.torrent_or_magnet.clone(),
                    torrent_name: saved.name.clone(),
                    torrent_control_state: TorrentControlState::Paused,
                    ..Default::default()
                });
            }
        }
        if let Some(display) = self.app_state.torrents.get_mut(&hash) {
            if display.latest_state.torrent_or_magnet.is_empty() {
                display.latest_state.torrent_or_magnet = saved
                    .map(|entry| entry.torrent_or_magnet)
                    .unwrap_or_default();
            }
            display.latest_state.torrent_control_state = TorrentControlState::Paused;
            display.latest_state.download_speed_bps = 0;
            display.latest_state.upload_speed_bps = 0;
            display.latest_state.peers.clear();
            display.latest_state.number_of_successfully_connected_peers = 0;
        }
        self.set_browser_error(&error);
        self.failed_managers.insert(hash, error);
        self.checkpoint_requested = true;
    }
}

#[cfg(feature = "browser-contract")]
impl BrowserSession {
    pub(crate) fn deletion_intent_contract() {
        for delete_files in [false, true] {
            let mut session = BrowserSession::from_settings(80, 24, Settings::default());
            let hash = vec![42; 20];
            let endpoint = session
                .register_torrent_manager_with_metrics(TorrentMetrics {
                    info_hash: hash.clone(),
                    torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hex::encode(&hash)),
                    ..Default::default()
                })
                .unwrap();
            session.publish_catalog_row(endpoint.metrics_tx.borrow().clone());
            session.drain_manager_messages();
            let command = if delete_files {
                ManagerCommand::DeleteFile
            } else {
                ManagerCommand::Shutdown
            };
            session.request_manager_control(&hash, command).unwrap();
            // A watch publication queued before the command was processed must
            // not replace accepted catalog intent with stale manager state.
            endpoint.metrics_tx.send_modify(|metrics| {
                metrics.delete_files = !delete_files;
                metrics.torrent_control_state = TorrentControlState::Running;
            });
            session.drain_manager_messages();
            let row = &session.app_state.torrents[&hash].latest_state;
            assert_eq!(row.torrent_control_state, TorrentControlState::Deleting);
            assert_eq!(row.delete_files, delete_files);
            assert!(session.checkpoint_requested);
            let saved = session.prepare_checkpoint(10);
            assert_eq!(saved.settings.torrents.len(), 1);
            assert_eq!(
                saved.settings.torrents[0].torrent_control_state,
                TorrentControlState::Deleting
            );
            assert_eq!(saved.settings.torrents[0].delete_files, delete_files);
        }
    }
}
