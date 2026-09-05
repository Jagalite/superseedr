// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native runtime execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let result = self.run_event_loop(terminal).await;
        self.app_state.should_quit = true;
        self.save_state_to_disk();

        self.shutdown_sequence(terminal).await;
        self.flush_shared_recovery_backup_worker().await;
        self.flush_persistence_writer().await;
        self.app_state
            .lifecycle
            .finish(!self.app_state.checkpoint.is_dirty());

        result
    }

    async fn run_event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Ok(size) = terminal.size() {
            self.app_state.screen_area = Rect::new(0, 0, size.width, size.height);
        }

        self.process_pending_commands().await;

        self.startup_crossterm_event_listener();
        self.startup_network_history_restore();
        self.startup_activity_history_restore();
        self.startup_version_checker();

        let mut sys = System::new();

        let mut stats_interval = time::interval(Duration::from_secs(1));
        let mut network_history_persist_interval =
            time::interval(Duration::from_secs(NETWORK_HISTORY_PERSIST_INTERVAL_SECS));
        let shared_recovery_backup_period =
            Duration::from_secs(SHARED_RECOVERY_BACKUP_REFRESH_INTERVAL_SECS);
        let mut shared_recovery_backup_interval = time::interval_at(
            time::Instant::now() + shared_recovery_backup_period,
            shared_recovery_backup_period,
        );
        let mut watch_folder_rescan_interval =
            time::interval(Duration::from_secs(WATCH_FOLDER_RESCAN_INTERVAL_SECS));
        let mut shared_role_retry_interval =
            time::interval(Duration::from_secs(SHARED_ROLE_RETRY_INTERVAL_SECS));
        let mut integrity_scheduler_interval = time::interval(INTEGRITY_SCHEDULER_TICK_INTERVAL);
        self.reschedule_tuning_deadline();
        network_history_persist_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        shared_recovery_backup_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        watch_folder_rescan_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        shared_role_retry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        integrity_scheduler_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        self.save_state_to_disk();
        self.dump_status_to_file();
        self.reschedule_status_dump_deadline();

        let mut next_draw_time = Instant::now();
        while !self.app_state.should_quit {
            while let Some(result) = self.background_tasks.try_join_next() {
                if let Err(error) = result {
                    self.app_state.system_error = Some(format!("Application task failed: {error}"));
                }
            }
            while let Some(result) = self.manager_tasks.try_join_next() {
                if let Err(error) = result {
                    self.app_state.system_error =
                        Some(format!("Torrent manager task failed: {error}"));
                }
            }
            self.flush_pending_watch_commands();

            let current_target_framerate = match self.app_state.mode {
                AppMode::Welcome => DataRate::Rate60s.frame_interval(), // Force 60 FPS for animation
                AppMode::PowerSaving => Duration::from_secs(1),         // Force 1 FPS for Zen mode
                _ => self.app_state.data_rate.frame_interval(),         // User-defined FPS
            };
            let next_tuning_at = self.next_tuning_at;
            let next_paste_flush_at = self.app_state.ui.normal_paste_burst.next_deadline();
            let next_status_dump_at = self.next_status_dump_at;
            let next_startup_load_at = self.next_startup_load_at;

            tokio::select! {
                _ = signal::ctrl_c() => {
                    self.app_state.should_quit = true;
                }
                Ok(Ok(connection)) = async {
                    match &self.listener {
                        Some(listener) => tokio::time::timeout(Duration::from_secs(2), listener.accept()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Ok(active_network) = self.network_activation.try_active() {
                        self.handle_incoming_peer(
                            connection.with_network_lease(active_network.scope().lease()),
                        ).await;
                    }

                }
                Some(incoming) = self.incoming_peer_handshake_rx.recv() => {
                    self.route_incoming_peer_handshake(incoming);
                }
                Some(observation) = self.manager_event_rx.recv() => {
                    if !observation.source.is_current() { continue; }
                    let event = observation.event;
                    self.handle_manager_event(event);
                    self.app_state.ui.needs_redraw = true;
                }
                status_changed = self.dht_status_rx.changed() => {
                    if status_changed.is_ok() {
                        self.handle_dht_status_changed();
                    }
                }
                network_changed = self.network_state_rx.changed() => {
                    if network_changed.is_ok() {
                        self.handle_network_state_changed().await;
                    }
                }
                policy_changed = self.peer_policy_rx.changed(), if self.peer_policy_open => {
                    if policy_changed.is_ok() {
                        let blocked_ips = sync_peer_policy_to_app_state(
                            &mut self.app_state,
                            &mut self.peer_policy_rx,
                        );
                        tracing::debug!(
                            blocked_ips,
                            "App received peer policy"
                        );
                        if matches!(self.app_state.mode, AppMode::PeerManagement) {
                            self.refresh_peer_management_derived(SystemTime::now());
                        }
                    } else {
                        self.peer_policy_open = false;
                    }
                }
                view_changed = self.peer_manager_view_rx.changed(), if self.peer_manager_view_open && should_sync_peer_manager_view(&self.app_state.mode) => {
                    if view_changed.is_ok() {
                        let tracked_peers = sync_peer_manager_view_to_app_state(
                            &mut self.app_state,
                            &mut self.peer_manager_view_rx,
                        );
                        tracing::debug!(
                            tracked_peers,
                            "App received peer manager view"
                        );
                        self.refresh_peer_management_derived(SystemTime::now());
                    } else {
                        self.peer_manager_view_open = false;
                    }
                }

                Some(command) = self.app_command_rx.recv() => {
                    self.handle_app_command(command).await;
                },

                Some(event) = self.tui_event_rx.recv() => {
                    self.clamp_selected_indices();
                    crate::tui::runtime::handle_event(self, event).await;
                    next_draw_time = Instant::now();
                }

                Some(result) = self.notify_rx.recv() => {
                    self.handle_file_event(result).await;
                }

                _ = watch_folder_rescan_interval.tick() => {
                    self.process_pending_commands().await;
                }
                _ = shared_role_retry_interval.tick() => {
                    self.maybe_promote_to_shared_leader().await;
                    self.refresh_follower_read_model();
                }

                _ = async {
                    if let Some(deadline) = next_paste_flush_at {
                        time::sleep_until(deadline.into()).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.clamp_selected_indices();
                    crate::tui::runtime::flush_pending_paste_burst(self).await;
                    next_draw_time = Instant::now();
                }

                _ = stats_interval.tick() => {
                    self.calculate_stats(&mut sys).await;
                    if matches!(self.app_state.mode, AppMode::PeerManagement) {
                        crate::tui::screens::peers::refresh_peer_management_expiries(
                            &mut self.app_state,
                            SystemTime::now(),
                        );
                    }
                    self.app_state.ui.needs_redraw = true;
                }

                _ = time::sleep_until(next_tuning_at) => {
                    self.tuning_resource_limits().await;
                    self.reschedule_tuning_deadline();
                }

                _ = async {
                    if let Some(deadline) = next_status_dump_at {
                        time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.trigger_status_dump_now();
                }
                _ = async {
                    if let Some(deadline) = next_startup_load_at {
                        time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.load_next_startup_batch().await;
                }
                _ = network_history_persist_interval.tick() => {
                    if should_persist_network_history_on_interval(&self.app_state) {
                        self.save_state_to_disk();
                    }
                }
                _ = shared_recovery_backup_interval.tick() => {
                    self.refresh_shared_recovery_backup_on_interval();
                }
                _ = integrity_scheduler_interval.tick() => {
                    self.advance_integrity_scheduler(INTEGRITY_SCHEDULER_TICK_INTERVAL);
                }
                _ = time::sleep_until(next_draw_time.into()) => {
                    let scheduled_frame_time = next_draw_time;
                    let frame_started_at = Instant::now();
                    self.app_state.ui.record_frame_wake(
                        scheduled_frame_time,
                        frame_started_at,
                        current_target_framerate,
                    );
                    Self::advance_next_draw_time(
                        &mut next_draw_time,
                        frame_started_at,
                        current_target_framerate,
                    );
                    self.drain_latest_torrent_metrics();
                    self.sync_dht_peer_slot_usage();
                    self.sync_network_activation_status_to_journal();
                    let normal_animation_active = if matches!(self.app_state.mode, AppMode::Normal)
                    {
                        let dht_wave_telemetry = self.dht_service.current_wave_telemetry();
                        Self::normal_mode_animation_active(
                            &self.app_state,
                            self.client_configs.ui_layout_mode,
                            Some(&dht_wave_telemetry),
                            frame_started_at,
                        )
                    } else {
                        false
                    };
                    let should_draw = Self::should_draw_this_frame(
                        &self.app_state.mode,
                        self.app_state.ui.needs_redraw,
                        normal_animation_active,
                    );
                    if should_draw {
                        self.app_state.ui.record_drawn_frame(frame_started_at);
                        self.tick_ui_effects_clock();
                        let dht_status = self.dht_service.current_status();
                        let dht_wave_telemetry = self.dht_service.current_wave_telemetry();
                        let draw_started_at = Instant::now();
                        terminal.draw(|f| {
                            draw(
                                f,
                                &self.app_state,
                                &dht_status,
                                &dht_wave_telemetry,
                                &self.client_configs,
                            );
                        })?;
                        self.app_state.ui.record_draw_duration(
                            draw_started_at.elapsed(),
                            current_target_framerate,
                        );
                        self.app_state.ui.needs_redraw = false;
                    } else if matches!(self.app_state.mode, AppMode::Normal) {
                        next_draw_time = frame_started_at
                            + Self::normal_idle_frame_check_interval(current_target_framerate);
                    }
                }
            }
        }

        Ok(())
    }

    pub(super) fn startup_crossterm_event_listener(&mut self) {
        let tui_event_tx_clone = self.tui_event_tx.clone();
        let mut tui_shutdown_rx = self.shutdown_tx.subscribe();

        self.tui_task = Some(tokio::spawn(async move {
            loop {
                if tui_shutdown_rx.try_recv().is_ok() {
                    break;
                }

                // Run blocking poll to completion (do NOT use tokio::select!)
                // This ensures we never abandon a thread that is reading from stdin.
                // Keep the timeout relatively short (250ms) so the app remains responsive to shutdown.
                let event =
                    tokio::task::spawn_blocking(|| -> std::io::Result<Option<CrosstermEvent>> {
                        if event::poll(Duration::from_millis(250))? {
                            return Ok(Some(crate::tui::runtime::adapt_terminal_event(
                                event::read()?,
                            )));
                        }
                        Ok(None)
                    })
                    .await;

                match event {
                    Ok(Ok(Some(e))) => {
                        tokio::select! {
                            send_result = tui_event_tx_clone.send(e) => {
                                if send_result.is_err() {
                                    break;
                                }
                            }
                            _ = tui_shutdown_rx.recv() => break,
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => {
                        tracing::error!("Crossterm event error: {}", e);
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Blocking task join error: {}", e);
                        break;
                    }
                }

                if tui_shutdown_rx.try_recv().is_ok() {
                    break;
                }
            }
        }));
    }

    pub(super) async fn shutdown_sequence<B: Backend>(&mut self, terminal: &mut Terminal<B>) {
        self.shutdown_runtime(|app| {
            let dht_status = app.dht_service.current_status();
            let dht_wave_telemetry = app.dht_service.current_wave_telemetry();
            let _ = terminal.draw(|frame| {
                draw(
                    frame,
                    &app.app_state,
                    &dht_status,
                    &dht_wave_telemetry,
                    &app.client_configs,
                );
            });
        })
        .await;
    }

    /// Host teardown can run without a terminal; progress presentation is supplied by the caller.
    pub(super) async fn shutdown_runtime(&mut self, mut present: impl FnMut(&Self)) {
        self.app_state
            .lifecycle
            .begin_shutdown(self.torrent_manager_command_txs.keys().cloned());
        // Stop producers that feed the app loop before it stops draining their channels.
        // The peer manager has a dedicated signal so it can remain alive for the final
        // torrent-manager metrics flush below.
        let _ = self.shutdown_tx.send(());

        if let Some(handle) = self.tui_task.take() {
            tracing::info!("Waiting for TUI event listener to finish...");
            if let Err(error) = handle.await {
                tracing::error!(%error, "Error joining TUI task");
            }
        }

        if let Some(task) = self.tui_command_batch_task.take() {
            if let Err(error) = task.await {
                self.app_state.lifecycle.host_cleanup_failed = true;
                self.app_state.system_error = Some(format!(
                    "Application command sender failed during shutdown: {error}"
                ));
            }
        }

        let total_managers_to_shut_down = self.torrent_manager_command_txs.len();
        let mut unsent_shutdowns: HashSet<Vec<u8>> =
            self.torrent_manager_command_txs.keys().cloned().collect();

        if total_managers_to_shut_down > 0 {
            let shutdown_timeout = time::sleep(Duration::from_secs(SHUTDOWN_TIMEOUT_SECS));
            let mut draw_interval = time::interval(Duration::from_millis(100));
            tokio::pin!(shutdown_timeout);

            tracing_event!(
                Level::INFO,
                "Waiting for {} torrents to shut down...",
                total_managers_to_shut_down
            );

            loop {
                unsent_shutdowns.retain(|hash| {
                    let Some(sender) = self.torrent_manager_command_txs.get(hash) else {
                        return false;
                    };
                    match sender.try_send(ManagerCommand::Shutdown) {
                        Ok(()) => false,
                        Err(mpsc::error::TrySendError::Full(_)) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            self.app_state.lifecycle.manager_stopped(
                                hash,
                                Err("Manager command channel closed during shutdown".into()),
                            );
                            false
                        }
                    }
                });
                self.app_state.shutdown_progress = self.app_state.lifecycle.progress();
                self.tick_ui_effects_clock();
                present(self);
                if self.app_state.lifecycle.pending_count() == 0 {
                    break;
                }

                tokio::select! {
                    Some(observation) = self.manager_event_rx.recv() => {
                    if !observation.source.is_current() { continue; }
                    let event = observation.event;
                        match event {
                            ManagerEvent::DeletionComplete(info_hash, result) => {
                                unsent_shutdowns.remove(&info_hash);
                                self.app_state.lifecycle.manager_stopped(&info_hash, result);
                            }
                            _ => {
                                // CRITICAL: We must aggressively drain other events (Stats, BlockReceived, etc.)
                                // so the managers don't get blocked on a full channel while trying to die.
                            }
                        }
                    }

                    _ = draw_interval.tick() => {
                    }

                    _ = &mut shutdown_timeout => {
                        tracing_event!(Level::WARN, "Shutdown timed out. {}/{} managers did not reply. Forcing exit.",
                            self.app_state.lifecycle.pending_count(),
                            total_managers_to_shut_down
                        );
                        break;
                    }
                }
            }
        }

        // Join manager execution before stopping the service that consumes their final metrics.
        let manager_tasks = std::mem::take(&mut self.manager_tasks);
        self.drain_owned_tasks(manager_tasks).await;
        let background_tasks = std::mem::take(&mut self.background_tasks);
        self.drain_owned_tasks(background_tasks).await;

        if !self.peer_manager.handle().flush().await {
            tracing::warn!("Peer manager stopped before final torrent metrics were flushed");
        }

        let _ = self.peer_manager_shutdown_tx.send(());
        self.peer_manager.wait_for_shutdown().await;
    }

    async fn drain_owned_tasks(&mut self, mut tasks: tokio::task::JoinSet<()>) {
        let deadline = time::sleep(Duration::from_secs(SHUTDOWN_TIMEOUT_SECS));
        tokio::pin!(deadline);
        while !tasks.is_empty() {
            tokio::select! {
                result = tasks.join_next() => {
                    if let Some(Err(error)) = result {
                        self.app_state.lifecycle.host_cleanup_failed = true;
                        self.app_state.system_error = Some(format!("Application task failed during shutdown: {error}"));
                    }
                }
                Some(command) = self.app_command_rx.recv() => self.accept_checkpoint_command(command),
                Some(_) = self.manager_event_rx.recv() => {},
                _ = &mut deadline => {
                    self.app_state.lifecycle.host_cleanup_failed = true;
                    self.app_state.system_error = Some("Application tasks did not finish before the shutdown deadline.".into());
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
    }

    pub(super) async fn handle_app_command(&mut self, command: AppCommand) {
        match command {
            AppCommand::CheckpointPersisted { revision, result } => {
                reduce_app_action(
                    &mut self.app_state,
                    AppAction::CheckpointCompleted { revision, result },
                );
            }
            AppCommand::AddTorrentFromFile(path) => {
                let action = self.resolve_add_ingress_action(IngestSource::TorrentFile, &path);
                self.execute_add_ingress_action(IngestSource::TorrentFile, path, action)
                    .await;
            }
            AppCommand::AddTorrentFromPathFile(path) => {
                let action = self.resolve_add_ingress_action(IngestSource::TorrentPathFile, &path);
                self.execute_add_ingress_action(IngestSource::TorrentPathFile, path, action)
                    .await;
            }
            AppCommand::AddMagnetFromFile(path) => {
                let action = self.resolve_add_ingress_action(IngestSource::MagnetFile, &path);
                self.execute_add_ingress_action(IngestSource::MagnetFile, path, action)
                    .await;
            }
            AppCommand::MarkPortOpen {
                peer_addr,
                transport,
                scope_id,
            } => {
                if self.network_activation.is_current(scope_id) {
                    self.mark_peer_port_open(peer_addr, transport);
                } else {
                    let active_scope_id = self
                        .network_activation
                        .try_active()
                        .ok()
                        .map(|active| active.scope().id());
                    tracing::trace!(
                        ?scope_id,
                        ?active_scope_id,
                        "ignoring reachability observed on an inactive network activation"
                    );
                }
            }
            AppCommand::SubmitControlRequest(request) => {
                self.handle_submit_control_request(request, None).await;
            }
            AppCommand::SubmitManualAddRequest {
                request,
                pending_ingest,
            } => {
                self.handle_submit_control_request(request, pending_ingest)
                    .await;
            }
            AppCommand::ControlRequest { path, request } => {
                if self.is_current_shared_follower() && self.is_host_watch_path(&path) {
                    self.app_state.pending_control_by_path.remove(&path);
                    self.relay_local_watch_file(&path, "control.forwarded");
                    self.save_state_to_disk();
                    return;
                }

                let result = self.apply_control_request(&request).await;
                self.record_control_result(&path, &request, result);
                self.save_state_to_disk();

                if let Err(error) = archive_watch_file(&path, "control.done") {
                    tracing_event!(
                        Level::WARN,
                        "Failed to archive processed control file {:?}: {}",
                        &path,
                        error
                    );
                }
            }
            AppCommand::ClientShutdown(path) => {
                tracing_event!(Level::INFO, "Shutdown command received via command file.");
                self.app_state.should_quit = true;
                if let Err(e) = fs::remove_file(&path) {
                    tracing_event!(
                        Level::WARN,
                        "Failed to remove command file {:?}: {}",
                        &path,
                        e
                    );
                }
            }
            AppCommand::PortFileChanged(path) => {
                self.handle_port_change(path).await;
            }

            AppCommand::FetchFileTree {
                browser_generation,
                path,
                browser_mode,
                preserve_browser_mode,
                highlight_path,
            } => {
                self.start_file_browser_fetch(
                    browser_generation,
                    path,
                    browser_mode,
                    preserve_browser_mode,
                    highlight_path,
                );
            }

            AppCommand::UpdateFileBrowserData {
                request_id,
                path,
                data,
                highlight_path,
            } => {
                if crate::app::reducer::preview::apply_file_tree_result(
                    &mut self.app_state,
                    request_id,
                    path,
                    data,
                    highlight_path,
                ) {
                    self.sync_torrent_file_preview();
                }
            }
            AppCommand::FileBrowserFetchFailed {
                request_id,
                path,
                message,
            } => {
                crate::app::reducer::preview::apply_file_tree_failure(
                    &mut self.app_state,
                    request_id,
                    path,
                    message,
                );
            }
            AppCommand::UpdateTorrentFilePreview {
                browser_generation,
                request_id,
                path,
                result,
            } => {
                crate::app::reducer::preview::apply_torrent_preview_result(
                    &mut self.app_state,
                    browser_generation,
                    request_id,
                    path,
                    result,
                );
            }
            AppCommand::RssSyncNow => {
                let _ = self.rss_sync_tx.try_send(());
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::RssPreviewUpdated(preview_items) => {
                self.observe_service(crate::app::reducer::ServiceObservation::RssPreview(
                    preview_items,
                ));
            }
            AppCommand::RssSyncStatusUpdated {
                last_sync_at,
                next_sync_at,
            } => {
                self.observe_service(crate::app::reducer::ServiceObservation::RssSync {
                    last_sync_at,
                    next_sync_at,
                });
            }
            AppCommand::RssFeedErrorUpdated { feed_url, error } => {
                self.observe_service(crate::app::reducer::ServiceObservation::RssFeedError {
                    feed_url,
                    error,
                });
            }
            AppCommand::RssDownloadSelected {
                entry,
                command_path,
            } => {
                if let Some(command_path) = command_path {
                    let ingest_kind = ingest_kind_from_path(&command_path).unwrap_or_default();
                    let origin = match entry.added_via {
                        crate::config::RssAddedVia::Auto => IngestOrigin::RssAuto,
                        crate::config::RssAddedVia::Manual => IngestOrigin::RssManual,
                    };
                    self.record_rss_queued(command_path, origin, ingest_kind);
                }
                let existing_idx = self
                    .app_state
                    .rss_runtime
                    .history
                    .iter()
                    .position(|existing| existing.dedupe_key == entry.dedupe_key);
                if let Some(idx) = existing_idx {
                    if self.app_state.rss_runtime.history[idx].info_hash.is_none()
                        && entry.info_hash.is_some()
                    {
                        self.app_state.rss_runtime.history[idx].info_hash = entry.info_hash.clone();
                        self.save_state_to_disk();
                    }
                } else {
                    self.app_state.rss_runtime.history.push(entry);
                    self.save_state_to_disk();
                }
                self.refresh_rss_derived();
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::RssDownloadPreview(item) => {
                self.download_rss_preview_item(item).await;
                self.refresh_rss_derived();
                self.app_state.ui.needs_redraw = true;
            }
            AppCommand::NetworkHistoryLoaded(state) => {
                self.observe_service(
                    crate::app::reducer::ServiceObservation::NetworkHistoryLoaded(state),
                );
            }
            AppCommand::ActivityHistoryLoaded(state) => {
                self.observe_service(
                    crate::app::reducer::ServiceObservation::ActivityHistoryLoaded(state),
                );
            }
            AppCommand::NetworkHistoryPersisted {
                request_id,
                success,
            } => {
                apply_network_history_persist_result(&mut self.app_state, request_id, success);
            }
            AppCommand::ActivityHistoryPersisted {
                request_id,
                success,
            } => {
                apply_activity_history_persist_result(&mut self.app_state, request_id, success);
            }
            AppCommand::ConfigNetworkInterfacesDiscovered { request_id, result } => {
                let inventory = &mut self.app_state.ui.config.network_interface_inventory;
                let error = result.as_ref().err().cloned();
                if inventory.finish_refresh(request_id, result) {
                    if let Some(error) = error {
                        tracing_event!(
                            Level::WARN,
                            %error,
                            "Config interface discovery failed"
                        );
                    }
                    self.app_state.ui.needs_redraw = true;
                }
            }
            AppCommand::RefreshConfigNetworkInterfaces => {
                self.refresh_config_network_interfaces();
            }
            AppCommand::UpdateConfig(new_settings) => {
                let capabilities = self.cluster_capabilities();
                if capabilities.can_edit_host_local_config && self.is_current_shared_follower() {
                    match classify_shared_mode_settings_change(&self.client_configs, &new_settings)
                    {
                        SettingsChangeScope::NoChange => {}
                        SettingsChangeScope::HostOnly => {
                            match self.app_persistence.save_settings(&new_settings) {
                                Ok(()) => self.apply_settings_update(new_settings, false).await,
                                Err(error) => {
                                    self.app_state.system_error = Some(format!(
                                        "Failed to save follower host-local settings: {}",
                                        error
                                    ));
                                    self.app_state.ui.needs_redraw = true;
                                }
                            }
                        }
                        SettingsChangeScope::SharedOrMixed => {
                            self.app_state.system_error = Some(
                                "Shared configuration and RSS edits are leader-only while this node is a follower. Only host-local client ID, port, and watch-folder changes are allowed."
                                    .to_string(),
                            );
                            self.app_state.ui.needs_redraw = true;
                        }
                    }
                } else {
                    self.apply_settings_update(new_settings, true).await;
                }
            }
            AppCommand::ReloadClusterState(_path) => {
                if self.is_current_shared_leader() {
                    return;
                }
                match self.app_persistence.load_settings() {
                    Ok(new_settings) => {
                        self.apply_reloaded_settings(new_settings).await;
                    }
                    Err(error) => {
                        tracing_event!(
                            Level::ERROR,
                            "Failed to reload shared cluster state: {}",
                            error
                        );
                    }
                }
            }
            AppCommand::UpdateVersionAvailable(latest_version) => {
                self.app_state.update_available = Some(latest_version);
            }
        }
    }
}
