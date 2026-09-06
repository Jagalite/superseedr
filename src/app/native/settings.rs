// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native settings execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) async fn apply_reloaded_settings(&mut self, mut new_settings: Settings) {
        if self.persisted_network_binding_override.is_some() {
            self.persisted_network_binding_override = Some(new_settings.network_binding.clone());
            new_settings.network_binding = self.client_configs.network_binding.clone();
        }
        if new_settings != self.client_configs {
            self.apply_settings_update(new_settings, false).await;
            self.app_state
                .ui
                .visualization_focus
                .apply_settings(&self.client_configs);
        }
    }

    pub(super) async fn apply_settings_update(
        &mut self,
        mut new_settings: Settings,
        persist: bool,
    ) {
        let old_settings = self.client_configs.clone();
        if new_settings.download_mode != old_settings.download_mode {
            new_settings.set_download_mode(new_settings.download_mode);
        }
        // A metadata preview already has a manager but is deliberately absent
        // from the saved catalog until the user confirms the add.
        let preview_mode_changed = new_settings.download_mode != old_settings.download_mode;
        preserve_bound_random_client_port(&old_settings, &mut new_settings);
        let settings_revision = begin_settings_application(&mut self.app_state, &new_settings);
        let rss_changed = rss_settings_changed(&old_settings, &new_settings);
        let network_binding_changed = new_settings.network_binding != old_settings.network_binding;
        let mut config_error = None;

        if network_binding_changed {
            self.persisted_network_binding_override = None;
            tracing::info!("Config update: Network binding policy changed.");
            if self
                .network_handle
                .reconfigure(new_settings.network_binding.clone())
                .await
                .is_err()
            {
                config_error =
                    Some("Could not submit the updated network binding policy.".to_string());
            }
        }

        self.client_configs = new_settings.clone();
        let _ = self.rss_settings_tx.send(self.client_configs.clone());
        if !self
            .sync_runtime_torrents_from_settings(&old_settings, &new_settings)
            .await
        {
            config_error = Some(
                "One or more torrent managers did not accept the configuration request.".into(),
            );
        }

        if preview_mode_changed {
            if let Some(manager_tx) = self
                .app_state
                .pending_magnet_preview_info_hash
                .as_ref()
                .and_then(|hash| self.torrent_manager_command_txs.get(hash))
            {
                if !self
                    .send_manager_command_until_shutdown(
                        manager_tx,
                        ManagerCommand::SetDownloadMode(new_settings.download_mode),
                    )
                    .await
                {
                    config_error = Some(
                        "The pending torrent preview did not accept the download order.".into(),
                    );
                }
            }
        }

        if let Err(error) = crate::config::ensure_watch_directories(&self.client_configs) {
            tracing::warn!(
                "Failed to ensure configured watch directories exist after config update: {}",
                error
            );
        }
        self.reconcile_watched_paths(&new_settings);

        apply_settings_projection(&mut self.app_state, &old_settings, &new_settings);
        if new_settings.ui_refresh_rate != old_settings.ui_refresh_rate {
            for manager_tx in self.torrent_manager_command_txs.values() {
                let _ = manager_tx.try_send(ManagerCommand::SetDataRate(
                    new_settings.ui_refresh_rate.as_ms(),
                ));
            }
        }

        let port_changed = new_settings.randomize_client_port != old_settings.randomize_client_port
            || (!new_settings.randomize_client_port
                && new_settings.client_port != old_settings.client_port);
        let pinning_current_random_port = old_settings.randomize_client_port
            && !new_settings.randomize_client_port
            && self.listener.as_ref().and_then(ListenerSet::local_port)
                == Some(new_settings.client_port);
        let bootstrap_changed = new_settings.bootstrap_nodes != old_settings.bootstrap_nodes;

        if !network_binding_changed {
            if pinning_current_random_port {
                tracing::info!(
                    "Config update: Pinned current random listen port {} without rebinding",
                    new_settings.client_port
                );
            }

            if port_changed && !pinning_current_random_port {
                let requested_port = requested_listener_port(&new_settings);
                tracing::info!(
                    "Config update: Port changed to {}",
                    if requested_port == 0 {
                        "RANDOM".to_string()
                    } else {
                        requested_port.to_string()
                    }
                );
                if matches!(*self.network_state_rx.borrow(), NetworkState::Blocked(_)) {
                    tracing::info!(
                        "Deferred listen port {} until networking recovers.",
                        if requested_port == 0 {
                            "RANDOM".to_string()
                        } else {
                            requested_port.to_string()
                        }
                    );
                    if bootstrap_changed {
                        tracing::info!("Config update: DHT bootstrap nodes changed.");
                        self.dht_service
                            .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));
                    }
                    if self.network_handle.retry_binding().await.is_err() {
                        config_error = Some(
                            "Could not retry networking with the updated listen port.".to_string(),
                        );
                    }
                } else if !self.rebind_listener(requested_port).await {
                    config_error = Some(format!(
                        "Could not activate listen port {}. Networking is blocked; configured port {} was restored.",
                        if requested_port == 0 {
                            "RANDOM".to_string()
                        } else {
                            requested_port.to_string()
                        },
                        old_settings.client_port
                    ));
                    self.client_configs.client_port = old_settings.client_port;
                    self.client_configs.randomize_client_port = old_settings.randomize_client_port;
                    let _ = self.rss_settings_tx.send(self.client_configs.clone());
                    if bootstrap_changed {
                        tracing::info!("Config update: DHT bootstrap nodes changed.");
                        self.dht_service
                            .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));
                    }
                    if self.network_handle.retry_binding().await.is_err() {
                        config_error = Some(
                            "Could not retry networking with the restored listen port.".to_string(),
                        );
                    }
                }
            } else if bootstrap_changed {
                tracing::info!("Config update: DHT bootstrap nodes changed.");
                self.dht_service
                    .reconfigure(DhtServiceConfig::from_settings(&self.client_configs));
            }
        }

        if new_settings.global_download_limit_bps != old_settings.global_download_limit_bps {
            self.disk_write_download_throttle
                .reset(new_settings.global_download_limit_bps);
            self.app_state.effective_download_limit_bps = new_settings.global_download_limit_bps;
            self.global_dl_bucket
                .set_rate(configured_download_bucket_rate(
                    new_settings.global_download_limit_bps,
                ));
        }
        if new_settings.global_upload_limit_bps != old_settings.global_upload_limit_bps {
            self.global_ul_bucket
                .set_rate(configured_upload_bucket_rate(
                    new_settings.global_upload_limit_bps,
                ));
        }

        if self.status_dump_interval_override_secs.is_none() {
            self.reschedule_status_dump_deadline();
        }

        if rss_changed {
            prune_rss_feed_errors(
                &mut self.app_state.rss_runtime.feed_errors,
                &self.client_configs,
            );
            self.refresh_rss_derived();
            let _ = self.rss_sync_tx.try_send(());
        }

        if persist {
            self.save_state_to_disk();
        }

        finish_settings_application(&mut self.app_state, settings_revision, config_error);
    }

    pub(crate) async fn apply_config_update_from_ui(&mut self, settings: Settings) {
        self.handle_app_command(AppCommand::UpdateConfig(settings))
            .await;
    }

    pub(crate) fn refresh_config_network_interfaces(&mut self) {
        let inventory = &mut self.app_state.ui.config.network_interface_inventory;
        let request_id = inventory.begin_refresh();
        self.app_state.ui.needs_redraw = true;

        let app_command_tx = self.app_command_tx.clone();
        self.background_tasks.spawn(async move {
            let result = tokio::task::spawn_blocking(|| {
                available_network_interfaces().map(|interfaces| {
                    interfaces
                        .into_iter()
                        .filter(NetworkInterfaceInfo::is_selectable)
                        .collect()
                })
            })
            .await
            .map_err(|error| format!("interface discovery task failed: {error}"))
            .and_then(|result| result.map_err(|error| error.to_string()));

            let _ = app_command_tx
                .send(AppCommand::ConfigNetworkInterfacesDiscovered { request_id, result })
                .await;
        });
    }

    pub(super) async fn handle_port_change(&mut self, path: PathBuf) {
        tracing_event!(Level::DEBUG, "Processing port file change...");
        let port_str = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing_event!(Level::ERROR, "Failed to read port file {:?}: {}", &path, e);
                return;
            }
        };

        match port_str.trim().parse::<u16>() {
            Ok(new_port) => {
                if new_port > 0 && new_port != self.client_configs.client_port {
                    tracing_event!(
                        Level::INFO,
                        "Port changed: {} -> {}. Attempting to re-bind listener.",
                        self.client_configs.client_port,
                        new_port
                    );

                    if matches!(*self.network_state_rx.borrow(), NetworkState::Blocked(_)) {
                        self.client_configs.client_port = new_port;
                        self.client_configs.randomize_client_port = false;
                        let _ = self.rss_settings_tx.send(self.client_configs.clone());
                        self.save_state_to_disk();
                        if self.network_handle.retry_binding().await.is_err() {
                            tracing_event!(
                                Level::ERROR,
                                "Could not retry networking with forwarded port {}.",
                                new_port
                            );
                        } else {
                            tracing_event!(
                                Level::INFO,
                                "Retrying networking with forwarded port {}.",
                                new_port
                            );
                        }
                    } else if self.rebind_listener(new_port).await {
                        self.client_configs.randomize_client_port = false;
                        self.save_state_to_disk();
                    } else {
                        tracing_event!(
                            Level::ERROR,
                            "Failed to bind to new port {}. Retaining old listener.",
                            new_port
                        );
                    }
                } else if new_port == self.client_configs.client_port {
                    if new_port > 0 && self.client_configs.randomize_client_port {
                        self.client_configs.randomize_client_port = false;
                        let _ = self.rss_settings_tx.send(self.client_configs.clone());
                        self.save_state_to_disk();
                        tracing_event!(
                            Level::INFO,
                            "Forwarded port {} matches the active listener; pinned it for future starts.",
                            new_port
                        );
                    } else {
                        tracing_event!(
                            Level::DEBUG,
                            "Port file updated, but port is unchanged ({}).",
                            new_port
                        );
                    }
                }
            }
            Err(e) => {
                tracing_event!(
                    Level::ERROR,
                    "Failed to parse new port from file {:?}: {}",
                    &path,
                    e
                );
            }
        }
    }
}

pub(super) fn preserve_bound_random_client_port(
    old_settings: &Settings,
    new_settings: &mut Settings,
) {
    if old_settings.randomize_client_port && new_settings.randomize_client_port {
        new_settings.client_port = old_settings.client_port;
    }
}

pub(super) fn requested_listener_port(settings: &Settings) -> u16 {
    if settings.randomize_client_port {
        0
    } else {
        settings.client_port
    }
}
