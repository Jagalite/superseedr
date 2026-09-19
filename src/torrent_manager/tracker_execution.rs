// SPDX-License-Identifier: GPL-3.0-or-later
//! Platform-specific tracker execution; scheduling and results remain in TorrentManager.
use super::*;

impl TorrentManager {
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn start_socket_started_announce(
        &mut self,
        url: String,
        network_scope: NetworkScope,
        client_port: u16,
    ) {
        let scope_id = network_scope.id();
        let info_hash = self.state.info_hash.clone();
        let client_id = self.settings.client_id.clone();
        let torrent_size_left = self
            .state
            .multi_file_info
            .as_ref()
            .map_or(0, |mfi| mfi.total_size as usize);

        self.started_announce_scopes.insert(url.clone(), scope_id);
        let network_lease = network_scope.lease().clone();
        let manager_tx = self.torrent_manager_tx.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        crate::execution::spawn(async move {
            let announce = network_scope.run(announce_started(
                &network_lease,
                url.clone(),
                &info_hash,
                client_id,
                client_port,
                torrent_size_left,
            ));
            let result = tokio::select! {
                biased;
                _ = shutdown_rx.recv() => return,
                result = announce => match result {
                    Ok(Ok(response)) => Ok(response),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => return,
                },
            };
            send_generation_scoped_tracker_result(
                &manager_tx,
                NetworkResult::StartedAnnounceFinished { url, result },
                &network_scope,
                &mut shutdown_rx,
            )
            .await;
        });
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn start_socket_started_announce(
        &mut self,
        url: String,
        _scope: NetworkScope,
        _port: u16,
    ) {
        self.pending_started_announces.remove(&url);
        self.pending_completion_announces.remove(&url);
        self.apply_action(Action::TrackerError { url });
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn start_socket_completion_announce(
        &mut self,
        url: String,
        network_scope: NetworkScope,
        client_port: u16,
    ) {
        let scope_id = network_scope.id();
        let info_hash = self.state.info_hash.clone();
        let client_id = self.settings.client_id.clone();
        let uploaded = self.state.session_total_uploaded as usize;
        let downloaded = self.state.session_total_downloaded as usize;

        self.completion_announce_scopes
            .insert(url.clone(), scope_id);
        let network_lease = network_scope.lease().clone();
        let manager_tx = self.torrent_manager_tx.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        crate::execution::spawn(async move {
            let announce = network_scope.run(announce_completed(
                &network_lease,
                url.clone(),
                &info_hash,
                client_id,
                client_port,
                uploaded,
                downloaded,
            ));
            let error = tokio::select! {
                biased;
                _ = shutdown_rx.recv() => return,
                result = announce => match result {
                    Ok(Ok(_)) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(_) => return,
                },
            };
            let result = NetworkResult::CompletionAnnounceFinished { url, error };
            send_generation_scoped_tracker_result(
                &manager_tx,
                result,
                &network_scope,
                &mut shutdown_rx,
            )
            .await;
        });
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn start_socket_completion_announce(
        &mut self,
        url: String,
        _scope: NetworkScope,
        _port: u16,
    ) {
        self.pending_started_announces.remove(&url);
        self.pending_completion_announces.remove(&url);
        self.apply_action(Action::TrackerError { url });
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn announce_socket_tracker(&mut self, url: String) {
        if self.pending_started_announces.contains(&url) {
            self.start_pending_started_announces();
            return;
        }
        let Ok(active_network) = self.network_activation.try_active() else {
            return;
        };
        let network_scope = active_network.scope().clone();
        let network_lease = network_scope.lease().clone();
        let info_hash = self.state.info_hash.clone();
        let client_id = self.settings.client_id.clone();
        let port = active_network.listen_port();
        let ul = self.state.session_total_uploaded as usize;
        let dl = self.state.session_total_downloaded as usize;

        let torrent_size_left = if let Some(mfi) = &self.state.multi_file_info {
            let completed = self
                .state
                .piece_manager
                .bitfield
                .iter()
                .filter(|&&s| s == PieceStatus::Done)
                .count();
            let piece_len = self
                .state
                .torrent
                .as_ref()
                .map(|t| t.info.piece_length)
                .unwrap_or(0) as u64;
            let completed_bytes = (completed as u64) * piece_len;
            mfi.total_size.saturating_sub(completed_bytes) as usize
        } else {
            0
        };

        let tx = self.torrent_manager_tx.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        crate::execution::spawn(async move {
            let res = tokio::select! {
                biased;
                _ = shutdown_rx.recv() => return,
                r = announce_periodic(
                    &network_lease,
                    url.clone(),
                    &info_hash,
                    client_id,
                    port,
                    ul,
                    dl,
                    torrent_size_left
                ) => r
            };

            let result = match res {
                Ok(resp) => NetworkResult::AnnounceResponse {
                    url,
                    response: resp,
                },
                Err(e) => NetworkResult::AnnounceFailed {
                    url,
                    error: e.to_string(),
                },
            };
            send_generation_scoped_tracker_result(&tx, result, &network_scope, &mut shutdown_rx)
                .await;
        });
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn announce_socket_tracker(&mut self, url: String) {
        self.apply_action(Action::TrackerError { url });
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn stop_socket_announces(
        &self,
        tracker_urls: Vec<String>,
        left: usize,
        uploaded: usize,
        downloaded: usize,
    ) -> impl std::future::Future<Output = ()> + use<> {
        let private_client = self.settings.private_client;
        let tracker_urls =
            shutdown_tracker_urls(tracker_urls, &self.state.trackers, private_client);
        let tracker_network = self
            .network_activation
            .try_active()
            .ok()
            .map(|active| (active.scope().lease().clone(), active.listen_port()));
        let network_activation = self.network_activation.clone();
        let client_id = self.settings.client_id.clone();
        let info_hash = self.state.info_hash.clone();
        async move {
            let stop_announces = async {
                let tracker_network = match tracker_network {
                    Some(tracker_network) => Some(tracker_network),
                    None if private_client => {
                        wait_for_active_tracker_network(&network_activation).await
                    }
                    None => None,
                };
                let Some((network_lease, port)) = tracker_network else {
                    return;
                };
                let mut announce_set = JoinSet::new();
                for url in tracker_urls {
                    let network_lease = network_lease.clone();
                    let info_hash = info_hash.clone();
                    let client_id = client_id.clone();
                    let announce = async move {
                        announce_stopped(
                            &network_lease,
                            url,
                            &info_hash,
                            client_id,
                            port,
                            uploaded,
                            downloaded,
                            left,
                        )
                        .await;
                    };
                    if private_client {
                        announce_set.spawn(announce);
                    } else {
                        // Public stop announces remain best effort and do not delay storage close.
                        crate::execution::spawn(announce);
                    }
                }
                while announce_set.join_next().await.is_some() {}
            };
            if timeout(PRIVATE_TRACKER_STOP_ANNOUNCE_TIMEOUT, stop_announces)
                .await
                .is_err()
            {
                event!(Level::WARN, "Tracker stop announce timed out.");
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn stop_socket_announces(
        &self,
        _urls: Vec<String>,
        _left: usize,
        _uploaded: usize,
        _downloaded: usize,
    ) -> impl std::future::Future<Output = ()> + use<> {
        std::future::ready(())
    }
}
