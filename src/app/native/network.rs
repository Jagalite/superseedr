// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native network execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    #[cfg(test)]
    pub(super) fn active_network_generation_id(&self) -> Option<u64> {
        self.network_activation
            .try_active()
            .ok()
            .map(|active| active.scope().id().generation_id())
    }

    pub(super) async fn handle_network_state_changed(&mut self) {
        let state = self.network_state_rx.borrow().clone();
        self.app_state.network_runtime_status =
            Some(state.runtime_status(&self.client_configs.network_binding));
        let valid_generation_id = self
            .network_activation
            .try_active()
            .ok()
            .map(|active| active.scope().id().generation_id());
        if matches!(&state, NetworkState::Ready(generation) if valid_generation_id == Some(generation.id()))
        {
            return;
        }
        let previous_generation_id = self
            .network_activation_publisher
            .active_scope_id()
            .map(NetworkScopeId::generation_id);
        self.publish_network_activation_pending(state.generation_id());
        match state {
            NetworkState::Blocked(reason) => {
                if let Some(listener) = self.listener.take() {
                    listener.shutdown().await;
                }
                self.clear_network_generation_reachability();
                let mut suspended_dht = build_app_dht_service_config(&self.client_configs);
                suspended_dht.preferred_backend = crate::dht::service::DhtBackendKind::Disabled;
                if let Some(previous_generation_id) = previous_generation_id {
                    if let Err(error) = self.dht_service.reconfigure_and_wait(suspended_dht).await {
                        tracing_event!(
                            Level::WARN,
                            %error,
                            "DHT teardown did not acknowledge blocked network generation"
                        );
                    }
                    crate::networking::utp::shutdown_udp_generation(previous_generation_id).await;
                }
                self.block_network_activation(reason.to_string());
                self.network_warning = Some(format!("Networking blocked: {reason}"));
                tracing_event!(Level::WARN, %reason, "network generation blocked");
                self.refresh_system_warning();
                self.app_state.ui.needs_redraw = true;
            }
            NetworkState::Ready(generation) => {
                if let Some(listener) = self.listener.take() {
                    listener.shutdown().await;
                }
                self.clear_network_generation_reachability();
                if let Some(previous_generation_id) = previous_generation_id {
                    let mut suspended_dht = build_app_dht_service_config(&self.client_configs);
                    suspended_dht.preferred_backend = crate::dht::service::DhtBackendKind::Disabled;
                    let dht_teardown = self.dht_service.reconfigure_and_wait(suspended_dht).await;
                    crate::networking::utp::shutdown_udp_generation(previous_generation_id).await;
                    if let Err(error) = dht_teardown {
                        self.block_network_activation(format!(
                            "old DHT transport did not stop before rebind: {error}"
                        ));
                        self.network_warning = Some(format!(
                            "Networking blocked: old DHT transport did not stop before rebind: {error}"
                        ));
                        let _ = self
                            .network_handle
                            .block_generation(
                                generation.id(),
                                format!("old DHT transport did not stop before rebind: {error}"),
                            )
                            .await;
                        self.refresh_system_warning();
                        self.app_state.ui.needs_redraw = true;
                        return;
                    }
                }
                let requested_port = requested_listener_port(&self.client_configs);
                let lease = match self.network_handle.try_lease_generation(generation.id()) {
                    Ok(lease) => lease,
                    Err(error) => {
                        self.block_network_activation(error.to_string());
                        return;
                    }
                };
                let scope = match self.prepare_network_activation(lease) {
                    Ok(scope) => scope,
                    Err(error) => {
                        self.block_network_activation(error.to_string());
                        return;
                    }
                };
                match bind_peer_listener_with_lease(scope.lease(), requested_port).await {
                    Ok(listener) => {
                        if requested_port == 0 {
                            if let Some(bound_port) =
                                listener.as_ref().and_then(ListenerSet::local_port)
                            {
                                self.client_configs.client_port = bound_port;
                            }
                        }
                        self.listener = listener;
                        self.network_warning =
                            network_policy_warning(&self.client_configs.network_binding);
                        let bound_port = self
                            .listener
                            .as_ref()
                            .and_then(ListenerSet::local_port)
                            .unwrap_or(self.client_configs.client_port);
                        if let Err(error) = self.activate_network_scope(scope, bound_port) {
                            self.block_network_activation(error.to_string());
                            return;
                        }
                        if let Err(error) = self
                            .dht_service
                            .reconfigure_and_wait(build_app_dht_service_config(
                                &self.client_configs,
                            ))
                            .await
                        {
                            tracing_event!(
                                Level::WARN,
                                %error,
                                "DHT did not acknowledge the replacement network generation"
                            );
                        }
                        tracing_event!(
                            Level::INFO,
                            generation_id = generation.id(),
                            config_epoch = generation.config_epoch(),
                            "network generation activated"
                        );
                    }
                    Err(error) => {
                        let retry_binding = listener_bind_error_is_transient(&error);
                        self.block_network_activation(format!(
                            "replacement listener preflight failed: {error}"
                        ));
                        self.network_warning = Some(format!(
                            "Networking blocked: replacement listener preflight failed: {error}"
                        ));
                        let _ = self
                            .network_handle
                            .block_generation_with_retry(
                                generation.id(),
                                format!("replacement listener preflight failed: {error}"),
                                retry_binding,
                            )
                            .await;
                    }
                }
                self.refresh_system_warning();
                self.app_state.ui.needs_redraw = true;
            }
        }
    }

    pub(super) fn clear_network_generation_reachability(&mut self) {
        self.app_state.externally_accessable_port_v4 = false;
        self.app_state.externally_accessable_port_v6 = false;
        self.app_state.inbound_peer_transports = InboundPeerTransportStatus::default();
        self.app_state.externally_accessable_port_v4_highlight_until = None;
        self.app_state.externally_accessable_port_v6_highlight_until = None;
    }

    pub(super) fn route_incoming_peer_handshake(&mut self, incoming: IncomingPeerHandshake) {
        let IncomingPeerHandshake {
            connection,
            buffer,
            permit,
        } = incoming;
        if buffer.len() < 48 {
            return;
        }

        let peer_addr = connection.remote_addr;
        let transport = connection.endpoint.kind;
        let network_scope_id = connection.network_scope_id();
        let peer_info_hash = buffer[28..48].to_vec();
        let peer_info_hash_hex = hex::encode(&peer_info_hash);

        if self
            .peer_policy_rx
            .borrow()
            .blocks_ip(peer_addr.ip(), SystemTime::now())
        {
            tracing::trace!(
                peer_ip = %peer_addr,
                "Rejected inbound connection from blocked peer"
            );
            return;
        }

        let Some(torrent_manager_tx) = self.torrent_manager_incoming_peer_txs.get(&peer_info_hash)
        else {
            tracing::trace!(
                "ROUTING FAIL: No manager registered for hash: {}",
                peer_info_hash_hex
            );
            return;
        };

        let torrent_manager_tx = torrent_manager_tx.clone();
        let app_command_tx = self.app_command_tx.clone();
        self.background_tasks.spawn(async move {
            let mut network_invalidation_rx = connection.subscribe_network_invalidation();
            let send_result = tokio::select! {
                biased;
                _ = wait_for_peer_network_invalidation(&mut network_invalidation_rx) => return,
                result = torrent_manager_tx.send((connection, buffer, permit)) => result,
            };
            match send_result {
                Ok(()) => {
                    if let Some(scope_id) = network_scope_id {
                        let _ = app_command_tx.try_send(AppCommand::MarkPortOpen {
                            peer_addr,
                            transport,
                            scope_id,
                        });
                    }
                }
                Err(_) => {
                    tracing::trace!(
                        "ROUTING FAIL: Manager channel closed for hash: {}",
                        peer_info_hash_hex
                    );
                }
            }
        });
    }

    pub(super) async fn handle_incoming_peer(&mut self, mut connection: PeerConnection) {
        if self
            .peer_policy_rx
            .borrow()
            .blocks_ip(connection.remote_addr.ip(), SystemTime::now())
        {
            tracing::trace!(
                peer_ip = %connection.remote_addr,
                "Rejected inbound connection from blocked peer before handshake"
            );
            return;
        }

        let resource_manager_clone = self.resource_manager.clone();
        let incoming_peer_handshake_tx = self.incoming_peer_handshake_tx.clone();
        let mut permit_shutdown_rx = self.shutdown_tx.subscribe();
        self.background_tasks.spawn(async move {
            let mut network_invalidation_rx = connection.subscribe_network_invalidation();
            let session_permit = tokio::select! {
                biased;
                _ = wait_for_peer_network_invalidation(&mut network_invalidation_rx) => None,
                permit_result = resource_manager_clone.acquire_peer_connection() => {
                    match permit_result {
                        Ok(permit) => Some(permit),
                        Err(ResourceManagerError::QueueFull) => {
                            tracing_event!(
                                Level::DEBUG,
                                peer_ip = %connection.remote_addr,
                                "Incoming peer dropped because peer permit capacity is saturated."
                            );
                            None
                        }
                        Err(ResourceManagerError::ManagerShutdown) => {
                            tracing_event!(Level::DEBUG, "Failed to acquire permit. Manager shut down?");
                            None
                        }
                    }
                }
                _ = permit_shutdown_rx.recv() => None
            };
            let Some(permit) = session_permit else {
                return;
            };
            let peer_addr = connection.remote_addr;
            let mut buffer = vec![0u8; 68];
            let read_ok = tokio::select! {
                biased;
                _ = wait_for_peer_network_invalidation(&mut network_invalidation_rx) => false,
                result = time::timeout(
                    Duration::from_secs(INCOMING_HANDSHAKE_TIMEOUT_SECS),
                    connection.stream.read_exact(&mut buffer)
                ) => matches!(result, Ok(Ok(_))),
            };
            if !read_ok {
                return;
            }

            if !is_valid_incoming_bittorrent_handshake(&buffer) {
                tracing::trace!(
                    "Rejected inbound TCP connection with invalid BitTorrent handshake."
                );
                return;
            }

            let incoming = IncomingPeerHandshake {
                connection,
                buffer,
                permit,
            };
            let send_result = tokio::select! {
                biased;
                _ = wait_for_peer_network_invalidation(&mut network_invalidation_rx) => return,
                result = incoming_peer_handshake_tx.send(incoming) => result,
            };
            if send_result.is_err() {
                tracing_event!(
                    Level::DEBUG,
                    peer_ip = %peer_addr,
                    "Incoming peer routing queue closed; dropping connection."
                );
            }
        });
    }

    pub(super) fn active_running_torrents_for_dht_announce(&self) -> Vec<Vec<u8>> {
        self.app_state
            .torrents
            .iter()
            .filter(|(info_hash, display)| {
                display.latest_state.torrent_control_state == TorrentControlState::Running
                    && display.latest_state.number_of_pieces_total > 0
                    && self.torrent_manager_command_txs.contains_key(*info_hash)
            })
            .map(|(info_hash, _)| info_hash.clone())
            .collect()
    }

    pub(super) fn announce_torrents_to_dht<I>(&mut self, info_hashes: I)
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        let Some(port) =
            (self.client_configs.client_port > 0).then_some(self.client_configs.client_port)
        else {
            return;
        };

        let dht_handle = self.dht_service.handle();
        for info_hash in info_hashes {
            let should_announce = self
                .app_state
                .torrents
                .get(&info_hash)
                .is_some_and(|display| display.latest_state.number_of_pieces_total > 0);
            if !should_announce {
                continue;
            }
            let dht_handle = dht_handle.clone();
            self.background_tasks.spawn(async move {
                let _ = dht_handle.announce_peer(info_hash, Some(port)).await;
            });
        }
    }

    pub(super) fn mark_peer_port_open(
        &mut self,
        peer_addr: SocketAddr,
        transport: PeerTransportKind,
    ) {
        let highlight_until = Some(Instant::now() + PORT_FAMILY_HIGHLIGHT_DURATION);
        let ipv4 = match peer_addr {
            SocketAddr::V4(_) => {
                self.app_state.externally_accessable_port_v4_highlight_until = highlight_until;
                true
            }
            SocketAddr::V6(addr) if addr.ip().to_ipv4_mapped().is_some() => {
                self.app_state.externally_accessable_port_v4_highlight_until = highlight_until;
                true
            }
            SocketAddr::V6(_) => {
                self.app_state.externally_accessable_port_v6_highlight_until = highlight_until;
                false
            }
        };
        self.app_state
            .inbound_peer_transports
            .mark_seen(transport, ipv4);
        let open_flag = if ipv4 {
            &mut self.app_state.externally_accessable_port_v4
        } else {
            &mut self.app_state.externally_accessable_port_v6
        };
        let just_opened = !*open_flag;
        if just_opened {
            *open_flag = true;
            let info_hashes = self.active_running_torrents_for_dht_announce();
            self.announce_torrents_to_dht(info_hashes);
        }
        self.app_state.ui.needs_redraw = true;
    }

    pub(super) fn total_successfully_connected_peers(&self) -> usize {
        self.app_state
            .torrents
            .values()
            .map(|torrent| torrent.latest_state.number_of_successfully_connected_peers)
            .sum()
    }

    pub(super) fn sync_dht_peer_slot_usage(&mut self) {
        let total_peers = self.total_successfully_connected_peers();
        let max_connected_peers = self.effective_resource_limits().max_connected_peers;
        let usage = (total_peers, max_connected_peers);
        if self.last_dht_peer_slot_usage == Some(usage) {
            return;
        }

        self.last_dht_peer_slot_usage = Some(usage);
        self.dht_service
            .update_peer_slot_usage(total_peers, max_connected_peers);
    }

    pub(super) fn handle_dht_status_changed(&mut self) {
        self.refresh_system_warning();
        // ResetDemandPlanner is followed by a DHT status publish; resend peer pressure
        // because the planner-side cap may have been reset while usage stayed unchanged.
        self.last_dht_peer_slot_usage = None;
        self.sync_dht_peer_slot_usage();
        self.app_state.ui.needs_redraw = true;
    }

    pub(super) fn network_journal_interface(&self) -> Option<String> {
        self.app_state
            .network_runtime_status
            .as_ref()
            .and_then(|status| {
                status
                    .interface_display_name
                    .as_ref()
                    .or(status.interface.as_ref())
            })
            .cloned()
    }

    pub(super) fn sync_network_activation_status_to_journal(&mut self) {
        let status = self.network_activation.status();
        if self.app_state.network_activation_status.as_ref() == Some(&status) {
            return;
        }

        self.record_network_activation_status_in_journal(status);
    }

    pub(super) fn record_network_activation_status_in_journal(
        &mut self,
        status: NetworkActivationStatus,
    ) {
        let interface = self.network_journal_interface();
        let (event_type, generation_id, listen_port, message) = match &status {
            NetworkActivationStatus::Pending { generation_id } => (
                EventType::NetworkRebinding,
                *generation_id,
                None,
                interface.as_deref().map_or_else(
                    || "Rebinding network".to_string(),
                    |interface| format!("Rebinding network to {interface}"),
                ),
            ),
            NetworkActivationStatus::Blocked { reason } => {
                (EventType::NetworkBlocked, None, None, reason.to_string())
            }
            NetworkActivationStatus::Active {
                generation_id,
                listen_port,
            } => (
                EventType::NetworkRestored,
                Some(*generation_id),
                Some(*listen_port),
                interface.as_deref().map_or_else(
                    || format!("Network restored on port {listen_port}"),
                    |interface| {
                        format!("Network restored on {interface}, listening on port {listen_port}")
                    },
                ),
            ),
        };

        self.app_state.network_activation_status = Some(status);
        self.append_event_journal_entry(EventJournalEntry {
            host_id: self.event_journal_host_id.clone(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Network,
            event_type,
            message: Some(message),
            details: EventDetails::Network {
                interface,
                generation_id,
                listen_port,
            },
            ..Default::default()
        });
        self.app_state.ui.needs_redraw = true;
    }

    pub(super) fn publish_network_activation_pending(&mut self, generation_id: Option<u64>) {
        self.network_activation_publisher.pending(generation_id);
        self.sync_network_activation_status_to_journal();
    }

    pub(super) fn prepare_network_activation(
        &mut self,
        lease: NetworkLease,
    ) -> Result<NetworkScope, crate::networking::runtime::NetworkLeaseError> {
        let result = self.network_activation_publisher.prepare(lease);
        self.sync_network_activation_status_to_journal();
        result
    }

    pub(super) fn activate_network_scope(
        &mut self,
        scope: NetworkScope,
        listen_port: u16,
    ) -> Result<
        Arc<crate::networking::activation::ActiveNetwork>,
        crate::networking::runtime::NetworkLeaseError,
    > {
        let result = self
            .network_activation_publisher
            .activate_prepared(scope, listen_port);
        self.sync_network_activation_status_to_journal();
        result
    }

    pub(super) fn block_network_activation(&mut self, reason: impl Into<Arc<str>>) {
        self.network_activation_publisher.block(reason);
        self.sync_network_activation_status_to_journal();
    }

    pub(super) async fn rebind_listener(&mut self, new_port: u16) -> bool {
        self.rebind_listener_with_dht_timeout(new_port, PORT_REBIND_DHT_TIMEOUT)
            .await
    }

    pub(super) async fn rebind_listener_with_dht_timeout(
        &mut self,
        new_port: u16,
        dht_timeout: Duration,
    ) -> bool {
        let previous_bound_port = self.listener.as_ref().and_then(ListenerSet::local_port);
        let active_network = match self.network_activation.try_active() {
            Ok(active) => active,
            Err(error) => {
                tracing_event!(Level::WARN, %error, "Cannot rebind listener without an active network scope");
                return false;
            }
        };
        let generation_id = active_network.scope().id().generation_id();
        let lease = match self.network_handle.try_lease_generation(generation_id) {
            Ok(lease) => lease,
            Err(error) => {
                self.block_network_activation(error.to_string());
                return false;
            }
        };
        let scope = match self.prepare_network_activation(lease) {
            Ok(scope) => scope,
            Err(error) => {
                self.block_network_activation(error.to_string());
                return false;
            }
        };
        if let Some(old_listener) = self.listener.take() {
            old_listener.shutdown().await;
        }
        if let Some(old_port) = previous_bound_port {
            crate::networking::utp::shutdown_udp_port(generation_id, old_port).await;
        }
        let first_attempt = bind_peer_listener_with_lease(scope.lease(), new_port).await;
        let bind_result = match first_attempt {
            Err(_) => {
                // A listener driver aborted by its owner is released on the next
                // Tokio scheduling turn. Give that cleanup one bounded chance
                // before treating the requested port as externally occupied.
                tokio::task::yield_now().await;
                bind_peer_listener_with_lease(scope.lease(), new_port).await
            }
            result => result,
        };
        match bind_result {
            Ok(new_listener) => {
                self.listener = new_listener;
                // Note: client_configs.client_port is likely already updated by the caller (UpdateConfig)
                // but we ensure consistency here just in case.
                let bound_port = self
                    .listener
                    .as_ref()
                    .and_then(ListenerSet::local_port)
                    .unwrap_or(new_port);
                self.client_configs.client_port = bound_port;
                let bound_port_changed = previous_bound_port != Some(bound_port);

                if let Err(error) = self.activate_network_scope(scope, bound_port) {
                    self.block_network_activation(error.to_string());
                    return false;
                }

                tracing_event!(
                    Level::INFO,
                    "Successfully rebound listener to port {}",
                    bound_port
                );

                // Listener bind is the success criterion: a failed or wedged DHT
                // must not roll back the replacement listener or port persistence.
                match time::timeout(
                    dht_timeout,
                    self.dht_service
                        .reconfigure_and_wait(DhtServiceConfig::from_settings(&self.client_configs)),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing_event!(
                        Level::WARN,
                        %error,
                        "DHT rejected the replacement listen port; TCP/uTP remain on the replacement port"
                    ),
                    Err(_) => tracing_event!(
                        Level::WARN,
                        timeout_ms = dht_timeout.as_millis(),
                        "DHT timed out on the replacement listen port; TCP/uTP remain on the replacement port"
                    ),
                }

                if self.app_state.externally_accessable_port_v4
                    || self.app_state.externally_accessable_port_v6
                {
                    let info_hashes = self.active_running_torrents_for_dht_announce();
                    self.announce_torrents_to_dht(info_hashes);
                }

                if bound_port_changed {
                    self.app_state.externally_accessable_port_v4 = false;
                    self.app_state.externally_accessable_port_v6 = false;
                    self.app_state.externally_accessable_port_v4_highlight_until = None;
                    self.app_state.externally_accessable_port_v6_highlight_until = None;
                    self.app_state.inbound_peer_transports = InboundPeerTransportStatus::default();
                }

                true
            }
            Err(e) => {
                let retry_binding = listener_bind_error_is_transient(&e);
                self.block_network_activation(format!(
                    "replacement listener preflight failed: {e}"
                ));
                let _ = self
                    .network_handle
                    .block_generation_with_retry(
                        generation_id,
                        format!("replacement listener preflight failed: {e}"),
                        retry_binding,
                    )
                    .await;
                tracing_event!(
                    Level::ERROR,
                    "Failed to bind to new port {}: {}. Listener not updated.",
                    new_port,
                    e
                );

                false
            }
        }
    }
}

pub(super) fn is_valid_incoming_bittorrent_handshake(buffer: &[u8]) -> bool {
    buffer.len() >= 48
        && buffer[0] as usize == BITTORRENT_PROTOCOL_STR.len()
        && buffer[1..(1 + BITTORRENT_PROTOCOL_STR.len())] == *BITTORRENT_PROTOCOL_STR
}

pub(super) fn build_app_dht_service_config(client_configs: &Settings) -> DhtServiceConfig {
    let config = DhtServiceConfig::from_settings(client_configs);
    #[cfg(test)]
    {
        let mut config = config;
        if client_configs.client_port == 0 {
            config.preferred_backend = crate::dht::service::DhtBackendKind::Disabled;
        }
        config
    }
    #[cfg(not(test))]
    {
        config
    }
}

pub(super) fn listener_bind_error_is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrInUse
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
    )
}
