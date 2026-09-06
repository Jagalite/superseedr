// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser host bootstrap; shared app transitions retain application policy.

use super::*;

impl BrowserSession {
    pub fn from_fixture(width: u16, height: u16, fixture: PresentationFixture) -> Self {
        let presentation = PresentationState::from_fixture(width, height, fixture);
        let (mut app_state, dht_status, dht_wave_telemetry, mut settings) =
            presentation.into_parts();
        settings.ui_refresh_rate = DataRate::Rate60s;
        app_state.data_rate = DataRate::Rate60s;
        app_state.capabilities.demo = true;
        Self::from_parts(app_state, settings, dht_status, dht_wave_telemetry)
    }

    /// Constructs an empty application host without loading demo fixtures or simulated managers.
    /// Storage/network adapters must be installed by the browser composition root.
    pub fn from_settings(width: u16, height: u16, settings: Settings) -> Self {
        let mut state =
            crate::app::initial_app_state(&settings, Default::default(), Default::default());
        state.screen_area = ratatui::layout::Rect::new(0, 0, width.max(1), height.max(1));
        Self::from_parts(
            state,
            settings,
            DhtStatus::default(),
            DhtWaveTelemetry::default(),
        )
    }

    pub(super) fn from_parts(
        app_state: AppState,
        settings: Settings,
        dht_status: DhtStatus,
        dht_wave_telemetry: DhtWaveTelemetry,
    ) -> Self {
        let environment = BrowserRuntimeEnvironment::default();
        let app_persistence = AppPersistence::memory(settings.clone());
        let (manager_event_tx, manager_event_rx) = mpsc::channel(1_000);
        let (telemetry_batch_tx, telemetry_batch_rx) = mpsc::channel(1_000);
        let manager_data_rate_ms = settings.ui_refresh_rate.as_ms();
        let pending_catalog_restores = settings
            .torrents
            .iter()
            .filter(|torrent| torrent.torrent_control_state != TorrentControlState::Deleting)
            .filter_map(|torrent| {
                crate::torrent_identity::info_hash_from_torrent_source(&torrent.torrent_or_magnet)
            })
            .filter(|hash| !app_state.torrents.contains_key(hash))
            .collect();
        Self {
            app_state,
            client_configs: settings,
            app_persistence,
            dht_status,
            dht_wave_telemetry,
            pending_browser_commands: VecDeque::new(),
            pending_app_effects: VecDeque::new(),
            checkpoint_requested: false,
            pending_catalog_restores,
            unsent_shutdowns: HashSet::new(),
            pending_removals: HashSet::new(),
            #[cfg(feature = "webtorrent")]
            failed_managers: HashMap::new(),
            manager_data_rate_ms,
            torrent_manager_command_txs: HashMap::new(),
            torrent_metric_watch_rxs: HashMap::new(),
            manager_lifetimes: HashMap::new(),
            manager_event_tx,
            manager_event_rx,
            telemetry_batch_tx,
            telemetry_batch_rx,
            browser_tracked_peers: HashMap::new(),
            browser_peer_metrics_updates: 0,
            browser_selected_peer_rate_frame_updates: 0,
            browser_selected_peer_rate_frame_changes: 0,
            browser_network_interface_refreshes: 0,
            fps_sample_elapsed: 0.0,
            fps_sample_frames: 0,
            environment,
        }
    }

    pub fn configure_environment(&mut self, environment: BrowserRuntimeEnvironment) {
        let interfaces = environment
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
            .collect::<Vec<_>>();
        self.app_state
            .ui
            .config
            .network_interface_inventory
            .interfaces = interfaces;
        self.app_state.runtime_paths = crate::app::RuntimePathView {
            shared_mode: environment.shared_mode,
            settings_path: environment.settings_path.clone(),
            log_files_path: environment.log_files_path.clone(),
            fallback_watch_path: environment.fallback_watch_path.clone(),
            shared_inbox_path: environment.shared_inbox_path.clone(),
        };
        self.app_state.lifetime_downloaded_from_config = environment.lifetime_downloaded;
        self.app_state.lifetime_uploaded_from_config = environment.lifetime_uploaded;
        self.environment = environment;
        self.app_state.ui.needs_redraw = true;
    }
}
