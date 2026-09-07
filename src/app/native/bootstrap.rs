// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native bootstrap execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    #[cfg(test)]
    pub async fn new(
        client_configs: Settings,
        runtime_mode: AppRuntimeMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_lock(client_configs, runtime_mode, None).await
    }

    #[cfg(test)]
    pub async fn new_with_lock(
        client_configs: Settings,
        runtime_mode: AppRuntimeMode,
        app_lock_handle: Option<File>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let app_persistence = AppPersistence::native();
        Self::new_with_lock_and_persistence(
            client_configs,
            runtime_mode,
            app_lock_handle,
            app_persistence,
        )
        .await
    }

    pub(crate) async fn new_with_lock_and_persistence(
        client_configs: Settings,
        runtime_mode: AppRuntimeMode,
        app_lock_handle: Option<File>,
        app_persistence: AppPersistence,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_lock_network_override_and_persistence(
            client_configs,
            runtime_mode,
            app_lock_handle,
            None,
            app_persistence,
        )
        .await
    }

    #[cfg(test)]
    pub async fn new_with_lock_and_network_persistence_override(
        client_configs: Settings,
        runtime_mode: AppRuntimeMode,
        app_lock_handle: Option<File>,
        persisted_network_binding_override: Option<NetworkBindingConfig>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let app_persistence = AppPersistence::native();
        Self::new_with_lock_network_override_and_persistence(
            client_configs,
            runtime_mode,
            app_lock_handle,
            persisted_network_binding_override,
            app_persistence,
        )
        .await
    }

    pub(crate) async fn new_with_lock_network_override_and_persistence(
        mut client_configs: Settings,
        runtime_mode: AppRuntimeMode,
        app_lock_handle: Option<File>,
        persisted_network_binding_override: Option<NetworkBindingConfig>,
        app_persistence: AppPersistence,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let requested_port = requested_listener_port(&client_configs);
        let (network_handle, _network_supervisor_task) =
            NetworkSupervisor::spawn_with_config(&client_configs.network_binding);
        let mut network_state_rx = network_handle.subscribe();
        let initial_network_state = network_state_rx.borrow().clone();
        let (mut network_activation_publisher, network_activation) =
            NetworkActivationPublisher::channel();
        let (listener, network_warning) = match initial_network_state {
            NetworkState::Ready(generation) => {
                let lease = network_handle
                    .try_lease_generation(generation.id())
                    .map_err(io::Error::other)?;
                let scope = network_activation_publisher
                    .prepare(lease)
                    .map_err(io::Error::other)?;
                match bind_peer_listener_with_lease(scope.lease(), requested_port).await {
                    Ok(listener) => {
                        let bound_port = listener
                            .as_ref()
                            .and_then(ListenerSet::local_port)
                            .unwrap_or(requested_port);
                        if requested_port == 0 {
                            client_configs.client_port = bound_port;
                        }
                        network_activation_publisher
                            .activate_prepared(scope, bound_port)
                            .map_err(io::Error::other)?;
                        (
                            listener,
                            network_policy_warning(&client_configs.network_binding),
                        )
                    }
                    Err(error) => {
                        let retry_binding = listener_bind_error_is_transient(&error);
                        let reason = format!("initial listener preflight failed: {error}");
                        network_activation_publisher.block(reason.clone());
                        network_handle
                            .block_generation_with_retry(
                                generation.id(),
                                reason.clone(),
                                retry_binding,
                            )
                            .await
                            .map_err(io::Error::other)?;
                        while network_state_rx
                            .borrow()
                            .generation_id()
                            .is_some_and(|generation_id| generation_id == generation.id())
                        {
                            network_state_rx.changed().await.map_err(io::Error::other)?;
                        }
                        (None, Some(format!("Networking blocked: {reason}")))
                    }
                }
            }
            NetworkState::Blocked(reason) => {
                network_activation_publisher.block(reason.to_string());
                (None, Some(format!("Networking blocked: {reason}")))
            }
        };
        let initial_network_runtime_status = network_state_rx
            .borrow()
            .runtime_status(&client_configs.network_binding);
        let initial_network_activation_status = network_activation.status();
        if requested_port == 0 {
            client_configs.client_port = listener
                .as_ref()
                .and_then(ListenerSet::local_port)
                .unwrap_or(0);
        }

        let (manager_event_tx, manager_event_rx) = mpsc::channel::<ManagerObservation>(1000);
        let (app_command_tx, app_command_rx) = mpsc::channel::<AppCommand>(10);
        let (incoming_peer_handshake_tx, incoming_peer_handshake_rx) =
            mpsc::channel::<IncomingPeerHandshake>(INCOMING_PEER_HANDSHAKE_QUEUE_SIZE);
        let (rss_sync_tx, rss_sync_rx) = mpsc::channel::<()>(8);
        let (rss_downloaded_entry_tx, rss_downloaded_entry_rx) =
            mpsc::channel::<RssHistoryEntry>(64);
        let (rss_settings_tx, rss_settings_rx) = watch::channel(client_configs.clone());
        let (tui_event_tx, tui_event_rx) = mpsc::channel::<CrosstermEvent>(100);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (tui_command_batch_tx, tui_command_batch_task) =
            crate::tui::runtime::spawn_serialized_app_command_sender(
                app_command_tx.clone(),
                shutdown_tx.subscribe(),
            );
        let (peer_manager_shutdown_tx, _) = broadcast::channel(1);
        let peer_manager = PeerManagerService::new(peer_manager_shutdown_tx.subscribe());
        let peer_policy_rx = peer_manager.handle().subscribe_policy();
        let peer_manager_view_rx = peer_manager.handle().subscribe_view();
        let shared_mode_enabled = runtime_mode.is_shared();
        let current_cluster_role = initial_cluster_role_for_runtime_mode(runtime_mode);
        // Only tests holding the shared-environment guard own an isolated persistence path.
        // Other App tests must not leave background writers targeting that process-global path.
        #[cfg(test)]
        let persistence_writer_enabled = test_persistence_writer_enabled();
        #[cfg(not(test))]
        let persistence_writer_enabled = true;
        let (persistence_tx, persistence_task) = if !persistence_writer_enabled
            || (shared_mode_enabled
                && matches!(current_cluster_role, Some(AppClusterRole::Follower)))
        {
            (None, None)
        } else {
            let (persistence_tx, persistence_task) =
                spawn_persistence_writer(app_command_tx.clone(), app_persistence.clone());
            (Some(persistence_tx), Some(persistence_task))
        };
        let (event_journal_persistence_tx, event_journal_persistence_task) =
            if persistence_writer_enabled {
                let (tx, task) = spawn_event_journal_persistence_writer(app_persistence.clone());
                (Some(tx), Some(task))
            } else {
                (None, None)
            };
        let (shared_recovery_backup_tx, shared_recovery_backup_task) = if shared_mode_enabled {
            let (tx, task) = spawn_shared_recovery_backup_worker();
            (Some(tx), Some(task))
        } else {
            (None, None)
        };

        let (limits, system_warning) = calculate_adaptive_limits(&client_configs);
        tracing_event!(
            Level::DEBUG,
            "Adaptive limits calculated: max_peers={}, disk_reads={}, disk_writes={}",
            limits.max_connected_peers,
            limits.disk_read_permits,
            limits.disk_write_permits
        );
        let mut rm_limits = HashMap::new();
        rm_limits.insert(ResourceType::Reserve, (limits.reserve_permits, 0));
        rm_limits.insert(
            ResourceType::PeerConnection,
            (limits.max_connected_peers, limits.max_connected_peers * 2),
        );
        rm_limits.insert(
            ResourceType::DiskRead,
            (limits.disk_read_permits, limits.disk_read_permits * 2),
        );
        rm_limits.insert(
            ResourceType::DiskWrite,
            (limits.disk_write_permits, limits.disk_write_permits * 2),
        );
        let (resource_manager, resource_manager_client) =
            ResourceManager::new(rm_limits, shutdown_tx.clone());
        let mut background_tasks = tokio::task::JoinSet::new();
        background_tasks.spawn(resource_manager.run());

        let dht_service = DhtService::new(
            network_activation.clone(),
            build_app_dht_service_config(&client_configs),
            shutdown_tx.subscribe(),
        )
        .await
        .map_err(io::Error::other)?;
        let dht_status_rx = dht_service.subscribe_status();

        let dl_limit = configured_download_bucket_rate(client_configs.global_download_limit_bps);
        let ul_limit = configured_upload_bucket_rate(client_configs.global_upload_limit_bps);
        let global_dl_bucket = Arc::new(TokenBucket::new(dl_limit, dl_limit));
        let global_ul_bucket = Arc::new(TokenBucket::new(ul_limit, ul_limit));
        let _ = crate::config::ensure_watch_directories(&client_configs);
        let persisted_rss_state = app_persistence.load_rss_state();
        let persisted_event_journal_state = app_persistence.load_event_journal_state();

        let tuning_controller = TuningController::new_adaptive(limits.clone());
        let tuning_state = tuning_controller.state().clone();
        let runtime_paths = RuntimePathView {
            shared_mode: shared_mode_enabled,
            settings_path: if shared_mode_enabled {
                shared_settings_path()
            } else {
                local_settings_path()
            },
            log_files_path: runtime_log_dir().map(|path| path.join("app*.log")),
            fallback_watch_path: get_watch_path().map(|(watch_path, _)| watch_path),
            shared_inbox_path: shared_mode_enabled.then(shared_inbox_path).flatten(),
        };
        let app_state = AppState {
            system_warning: None,
            system_error: None,
            network_runtime_status: Some(initial_network_runtime_status),
            network_activation_status: Some(initial_network_activation_status),
            limits: limits.clone(),
            last_tuning_score: tuning_state.last_tuning_score,
            current_tuning_score: tuning_state.current_tuning_score,
            tuning_countdown: tuning_controller.cadence_secs(),
            last_tuning_limits: tuning_state.last_tuning_limits,
            baseline_speed_ema: tuning_state.baseline_speed_ema,
            adaptive_max_scpb: 10.0,
            runtime_paths,
            capabilities: AppCapabilities::native(),
            ..initial_app_state(
                &client_configs,
                persisted_rss_state,
                persisted_event_journal_state,
            )
        };

        let watched_paths = runtime_watch_paths(
            &client_configs,
            shared_mode_enabled,
            matches!(current_cluster_role, Some(AppClusterRole::Leader)) || !shared_mode_enabled,
        );

        let (notify_tx, notify_rx) = mpsc::channel::<Result<Event, NotifyError>>(100);
        let watcher = watcher::create_watcher(&watched_paths, true, notify_tx)?;
        let initial_tuning_deadline =
            time::Instant::now() + Duration::from_secs(tuning_controller.cadence_secs());
        let persisted_torrent_metadata_cache = app_persistence
            .load_torrent_metadata()
            .map(|metadata| {
                metadata
                    .torrents
                    .into_iter()
                    .filter_map(|entry| {
                        hex::decode(&entry.info_hash_hex)
                            .ok()
                            .map(|info_hash| (info_hash, entry))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut app = Self {
            app_state,
            client_configs: client_configs.clone(),
            app_persistence,
            runtime_mode,
            shared_mode_enabled,
            current_cluster_role,
            watched_paths,
            base_system_warning: system_warning,
            network_warning,
            listener,
            network_handle,
            network_state_rx,
            network_activation,
            network_activation_publisher,
            torrent_manager_incoming_peer_txs: HashMap::new(),
            torrent_manager_command_txs: HashMap::new(),
            incoming_peer_handshake_tx,
            incoming_peer_handshake_rx,
            dht_service,
            dht_status_rx,
            peer_manager,
            peer_policy_rx,
            peer_policy_open: true,
            peer_manager_view_rx,
            peer_manager_view_open: true,
            resource_manager: resource_manager_client,
            wake_lag_peer_throttle: WakeLagPeerThrottle::default(),
            last_applied_resource_limits: Some(limits.clone()),
            last_applied_peer_queue_size: Some(limits.max_connected_peers.saturating_mul(2)),
            global_dl_bucket,
            global_ul_bucket,
            disk_write_download_throttle: DiskBackpressureDownloadThrottle::new(
                client_configs.global_download_limit_bps,
            ),
            torrent_metric_watch_rxs: HashMap::new(),
            manager_lifetimes: HashMap::new(),
            background_tasks,
            manager_tasks: tokio::task::JoinSet::new(),
            manager_event_tx,
            manager_event_rx,
            app_command_tx,
            app_command_rx,
            tui_command_batch_tx,
            tui_command_batch_task: Some(tui_command_batch_task),
            rss_sync_tx,
            rss_downloaded_entry_tx,
            rss_settings_tx,
            tui_event_tx,
            tui_event_rx,
            shutdown_tx,
            peer_manager_shutdown_tx,
            persistence_tx,
            persistence_task,
            event_journal_persistence_tx,
            event_journal_persistence_task,
            shared_recovery_backup_tx,
            shared_recovery_backup_task,
            rss_sync_rx: Some(rss_sync_rx),
            rss_downloaded_entry_rx: Some(rss_downloaded_entry_rx),
            rss_settings_rx: Some(rss_settings_rx),
            rss_service_task: None,
            tui_task: None,
            watcher,
            notify_rx,
            tuning_controller,
            next_tuning_at: initial_tuning_deadline,
            integrity_scheduler: IntegrityScheduler::new(Instant::now()),
            event_journal_host_id: shared_host_id(),
            status_dump_interval_override_secs: None,
            next_status_dump_at: None,
            status_dump_generation: Arc::new(AtomicU64::new(0)),
            app_lock_handle,
            persisted_network_binding_override,
            leader_status_snapshot: None,
            startup_completion_suppressed_hashes: HashSet::new(),
            startup_deferred_load_queue: VecDeque::new(),
            startup_loaded_torrent_count: 0,
            startup_load_summary_logged: false,
            next_startup_load_at: None,
            last_dht_peer_slot_usage: None,
            persisted_torrent_metadata_cache,
            data_availability_fault_log_cooldowns: HashMap::new(),
            probe_available_log_cooldowns: HashMap::new(),
        };
        sync_peer_policy_to_app_state(&mut app.app_state, &mut app.peer_policy_rx);
        sync_peer_manager_view_to_app_state(&mut app.app_state, &mut app.peer_manager_view_rx);
        app.sync_cluster_role_label();
        app.refresh_system_warning();

        app.ensure_leader_services_running();

        let mut torrents_to_load = app.client_configs.torrents.clone();
        torrents_to_load.sort_by_key(|t| !t.validation_status);
        let mut running_torrents_started = 0usize;
        for torrent_config in torrents_to_load {
            let is_running = matches!(
                torrent_config.torrent_control_state,
                TorrentControlState::Running
            );
            let should_roll_running_torrent =
                is_running && !app.should_suppress_follower_runtime_for_torrent(&torrent_config);
            let should_defer_running_torrent = should_roll_running_torrent
                && running_torrents_started >= STARTUP_ROLLING_LOADS_PER_INTERVAL;

            if should_defer_running_torrent {
                if let Some(info_hash) =
                    info_hash_from_torrent_source(&torrent_config.torrent_or_magnet)
                {
                    app.startup_deferred_load_queue.push_back(info_hash);
                } else {
                    tracing_event!(
                        Level::WARN,
                        torrent = %torrent_config.torrent_or_magnet,
                        "Could not derive info hash for deferred startup torrent; restoring immediately"
                    );
                    if app.load_runtime_torrent_from_settings(torrent_config).await {
                        app.startup_loaded_torrent_count =
                            app.startup_loaded_torrent_count.saturating_add(1);
                    }
                }
            } else {
                if app.load_runtime_torrent_from_settings(torrent_config).await {
                    if should_roll_running_torrent {
                        running_torrents_started = running_torrents_started.saturating_add(1);
                    }
                    app.startup_loaded_torrent_count =
                        app.startup_loaded_torrent_count.saturating_add(1);
                }
            }
        }
        app.reschedule_startup_load_deadline();
        app.maybe_log_startup_load_summary();

        if app.app_state.torrents.is_empty()
            && app.startup_deferred_load_queue.is_empty()
            && app.app_state.lifetime_downloaded_from_config == 0
        {
            app.app_state.mode = AppMode::Welcome;
        }

        let is_leeching = app.app_state.torrents.values().any(|t| {
            t.latest_state.number_of_pieces_completed < t.latest_state.number_of_pieces_total
        });
        app.app_state.is_seeding = !is_leeching;
        app.refresh_rss_derived();
        app.refresh_follower_read_model();
        if matches!(
            app.network_activation.status(),
            NetworkActivationStatus::Blocked { .. }
        ) {
            let status = app.network_activation.status();
            app.record_network_activation_status_in_journal(status);
        }

        Ok(app)
    }

    pub(super) async fn start_missing_runtime_torrents_for_current_role(&mut self) {
        let mut running_torrents_started = 0usize;
        let mut deferred_torrent_added = false;

        for torrent in self.client_configs.torrents.clone() {
            let Some(info_hash) = info_hash_from_torrent_source(&torrent.torrent_or_magnet) else {
                continue;
            };
            if self.has_live_runtime_for_torrent(&info_hash) {
                continue;
            }
            if self
                .startup_deferred_load_queue
                .iter()
                .any(|queued_hash| queued_hash == &info_hash)
            {
                continue;
            }
            if self.should_suppress_follower_runtime_for_torrent(&torrent) {
                self.ensure_display_only_torrent_from_settings(&torrent);
                continue;
            }
            let is_running = matches!(torrent.torrent_control_state, TorrentControlState::Running);
            if is_running
                && (running_torrents_started >= STARTUP_ROLLING_LOADS_PER_INTERVAL
                    || !self.startup_deferred_load_queue.is_empty())
            {
                self.startup_deferred_load_queue.push_back(info_hash);
                deferred_torrent_added = true;
                continue;
            }

            if self.load_runtime_torrent_from_settings(torrent).await {
                if is_running {
                    running_torrents_started = running_torrents_started.saturating_add(1);
                }
                self.startup_loaded_torrent_count =
                    self.startup_loaded_torrent_count.saturating_add(1);
            }
        }

        if deferred_torrent_added {
            self.reschedule_startup_load_deadline();
        }
        self.maybe_log_startup_load_summary();
    }

    pub(super) fn reschedule_startup_load_deadline(&mut self) {
        self.reschedule_startup_load_deadline_after(Duration::from_secs(
            STARTUP_ROLLING_BATCH_INTERVAL_SECS,
        ));
    }

    pub(super) fn reschedule_startup_load_deadline_after(&mut self, delay: Duration) {
        self.next_startup_load_at = if self.startup_deferred_load_queue.is_empty() {
            None
        } else {
            Some(time::Instant::now() + delay)
        };
    }

    pub(super) fn maybe_log_startup_load_summary(&mut self) {
        if self.startup_load_summary_logged || !self.startup_deferred_load_queue.is_empty() {
            return;
        }
        if self.startup_loaded_torrent_count == 0 && self.client_configs.torrents.is_empty() {
            return;
        }

        self.startup_load_summary_logged = true;
    }

    pub(super) async fn load_next_startup_batch(&mut self) {
        let mut loaded_count = 0usize;

        for _ in 0..STARTUP_ROLLING_LOADS_PER_INTERVAL {
            let Some(info_hash) = self.startup_deferred_load_queue.front().cloned() else {
                break;
            };

            if self.has_live_runtime_for_torrent(&info_hash) {
                self.startup_deferred_load_queue.pop_front();
                continue;
            }

            let Some(torrent_config) = self
                .client_configs
                .torrents
                .iter()
                .find(|torrent| {
                    info_hash_from_torrent_source(&torrent.torrent_or_magnet).as_deref()
                        == Some(info_hash.as_slice())
                })
                .cloned()
            else {
                tracing_event!(
                    Level::WARN,
                    info_hash = %hex::encode(&info_hash),
                    "Skipping deferred startup torrent because it is no longer configured"
                );
                self.startup_deferred_load_queue.pop_front();
                continue;
            };

            if !should_load_persisted_torrent(&torrent_config) {
                self.startup_deferred_load_queue.pop_front();
                continue;
            }

            if self
                .load_runtime_torrent_from_settings(torrent_config)
                .await
            {
                self.startup_deferred_load_queue.pop_front();
                loaded_count = loaded_count.saturating_add(1);
            } else {
                if let Some(failed_info_hash) = self.startup_deferred_load_queue.pop_front() {
                    self.startup_deferred_load_queue.push_back(failed_info_hash);
                }
                tracing_event!(
                    Level::WARN,
                    info_hash = %hex::encode(&info_hash),
                    "Deferred startup torrent restore failed; moving it to the back of the queue"
                );
                continue;
            }
        }

        self.startup_loaded_torrent_count = self
            .startup_loaded_torrent_count
            .saturating_add(loaded_count);
        self.reschedule_startup_load_deadline();

        if loaded_count > 0 {
            self.app_state.ui.needs_redraw = true;
            self.save_state_to_disk();
        }
        self.maybe_log_startup_load_summary();
    }
}

pub(super) fn should_load_persisted_torrent(torrent_settings: &TorrentSettings) -> bool {
    torrent_settings.torrent_control_state != TorrentControlState::Deleting
}

pub(super) fn preserve_restored_added_at(
    app_state: &mut AppState,
    torrent_config: &TorrentSettings,
) {
    let Some(added_at_unix_secs) = torrent_config.added_at_unix_secs else {
        return;
    };
    let Some(info_hash) = info_hash_from_torrent_source(&torrent_config.torrent_or_magnet) else {
        return;
    };
    if let Some(runtime) = app_state.torrents.get_mut(&info_hash) {
        runtime.added_at_unix_secs = Some(added_at_unix_secs);
    }
}

pub(super) fn initial_cluster_role_for_runtime_mode(
    runtime_mode: AppRuntimeMode,
) -> Option<AppClusterRole> {
    match runtime_mode {
        AppRuntimeMode::Normal => None,
        AppRuntimeMode::SharedLeader => Some(AppClusterRole::Leader),
        AppRuntimeMode::SharedFollower => Some(AppClusterRole::Follower),
    }
}
