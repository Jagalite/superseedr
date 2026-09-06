// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host settings; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    pub(crate) fn apply_adjacent_theme(&mut self, next: bool) {
        let themes = ThemeName::sorted_for_ui();
        let current = themes
            .iter()
            .position(|theme| *theme == self.client_configs.ui_theme)
            .unwrap_or_default();
        let selected = if next {
            (current + 1) % themes.len()
        } else if current == 0 {
            themes.len() - 1
        } else {
            current - 1
        };
        self.client_configs.ui_theme = themes[selected];
        self.app_state.theme = Theme::builtin(themes[selected]);
    }

    pub(crate) fn apply_browser_config_update(&mut self, mut settings: Settings) {
        let mode_changed = settings.download_mode != self.client_configs.download_mode;
        if mode_changed {
            settings.set_download_mode(settings.download_mode);
        }
        let revision = crate::app::begin_settings_application(&mut self.app_state, &settings);
        if let Err(error) = self.app_persistence.save_settings(&settings) {
            crate::app::finish_settings_application(
                &mut self.app_state,
                revision,
                Some(format!("Failed to save browser configuration: {error}")),
            );
            return;
        }
        crate::app::apply_settings_projection(&mut self.app_state, &self.client_configs, &settings);
        self.client_configs = settings;
        self.checkpoint_requested = true;
        self.broadcast_manager_data_rate(self.client_configs.ui_refresh_rate.as_ms());
        rss::recompute_rss_derived(&mut self.app_state, &self.client_configs);
        let mut command_error = None;
        if mode_changed {
            let hashes: Vec<_> = self.torrent_manager_command_txs.keys().cloned().collect();
            for hash in hashes {
                if !self.send_manager_command(
                    &hash,
                    ManagerCommand::SetDownloadMode(self.client_configs.download_mode),
                ) {
                    command_error = Some("Download order was saved, but a torrent manager could not accept the change. Retry the setting or restart that torrent.".to_string());
                }
            }
        }
        crate::app::finish_settings_application(&mut self.app_state, revision, command_error);
    }

    pub(crate) fn refresh_browser_network_interfaces(&mut self) {
        if !self.app_state.capabilities.demo && !self.app_state.capabilities.native_paths {
            self.app_state.ui.config.network_interface_inventory.error =
                Some("Network interface selection is unavailable in this browser host.".into());
            self.app_state.ui.config.network_interface_inventory.loading = false;
            self.app_state.ui.needs_redraw = true;
            return;
        }
        let inventory = &mut self.app_state.ui.config.network_interface_inventory;
        inventory.interfaces = self
            .environment
            .network_interfaces
            .iter()
            .map(|interface| NetworkInterfaceInfo {
                identity: interface.identity.clone(),
                display_name: interface.display_name.clone(),
                ipv4_index: interface.ipv4_index,
                ipv6_index: interface.ipv6_index,
                is_up: interface.is_up,
                is_loopback: interface.is_loopback,
                ipv4_addresses: interface.ipv4_addresses.clone(),
                ipv6_addresses: interface.ipv6_addresses.clone(),
            })
            .collect();
        inventory.loading = false;
        inventory.error = None;
        self.browser_network_interface_refreshes =
            self.browser_network_interface_refreshes.saturating_add(1);
        self.app_state.ui.needs_redraw = true;
    }

    pub fn set_browser_add_location_prompt(&mut self, enabled: bool) {
        self.client_configs.always_show_add_location_prompt = enabled;
    }

    pub fn set_browser_default_download_folder(&mut self, path: PathBuf) {
        self.client_configs.default_download_folder = Some(path);
    }

    pub fn apply_next_browser_theme_setting(&mut self) {
        let themes = ThemeName::sorted_for_ui();
        let current = themes
            .iter()
            .position(|theme| *theme == self.client_configs.ui_theme)
            .unwrap_or_default();
        let mut settings = self.client_configs.clone();
        settings.ui_theme = themes[(current + 1) % themes.len()];
        self.apply_browser_config_update(settings);
    }
}
