// SPDX-License-Identifier: GPL-3.0-or-later
//! Native WebTorrent execution attached to the existing manager.
use super::*;
use crate::networking::webtorrent::{
    native::IceOptions,
    tracker::{self, Observation, Parameters, Report, Request},
    wire::{Counters, Event, Identity},
};
use crate::networking::{DnsPolicy, NetworkBindingMode};

struct Service {
    incarnation: u64,
    scope: NetworkScopeId,
    requests: Sender<Request>,
    stop: watch::Sender<bool>,
    inflight: bool,
    event: Option<Event>,
}
pub(super) struct Runtime {
    services: HashMap<String, Service>,
    send: Sender<Report>,
    pub(super) reports: Receiver<Report>,
    tasks: tokio::task::JoinSet<()>,
    serial: u64,
    capable: watch::Sender<bool>,
}
impl Default for Runtime {
    fn default() -> Self {
        let (send, reports) = mpsc::channel(32);
        Self {
            services: HashMap::new(),
            send,
            reports,
            tasks: tokio::task::JoinSet::new(),
            serial: 0,
            capable: watch::channel(true).0,
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        for service in self.services.values() {
            service.stop.send_replace(true);
        }
    }
}

impl Runtime {
    async fn shutdown(mut self, counters: Counters) {
        for service in self.services.values() {
            let _ = service.requests.try_send(Request {
                counters,
                event: Some(Event::Stopped),
            });
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        for service in self.services.values() {
            service.stop.send_replace(true);
        }
        self.services.clear();
        if tokio::time::timeout(Duration::from_secs(5), async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            self.tasks.abort_all();
            while self.tasks.join_next().await.is_some() {}
        }
    }
}

impl TorrentManager {
    pub(super) fn rtc_cleanup(&mut self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let counters = self.rtc_counters();
        let runtime = std::mem::take(&mut self.rtc);
        runtime.shutdown(counters)
    }
    pub(super) async fn rtc_shutdown(&mut self) {
        let _ = self.shutdown_tx.send(());
        self.rtc_cleanup().await;
    }
    pub(super) fn rtc_metadata_ready(&mut self, length: usize) {
        for (key, peer) in &self.state.peers {
            if key.starts_with("webrtc://") {
                let tx = peer.peer_tx.clone();
                self.rtc.tasks.spawn(async move {
                    let _ = tx.send(TorrentCommand::MetadataAvailable { length }).await;
                });
            }
        }
    }
    pub(super) fn rtc_metadata_request(&mut self, key: String, piece: usize) {
        let Some(peer) = self.state.peers.get(&key) else {
            return;
        };
        let fragment = self.state.torrent.as_ref().and_then(|torrent| {
            let data = &torrent.info_dict_bencode;
            let start = piece.checked_mul(16 * 1024)?;
            (start < data.len()).then(|| {
                (
                    data.len(),
                    data[start..(start + 16 * 1024).min(data.len())].to_vec(),
                )
            })
        });
        let (total, bytes) =
            fragment.map_or((None, Vec::new()), |(length, bytes)| (Some(length), bytes));
        let tx = peer.peer_tx.clone();
        self.rtc.tasks.spawn(async move {
            let _ = tx
                .send(TorrentCommand::MetadataReply {
                    piece,
                    total,
                    bytes,
                })
                .await;
        });
    }
    fn rtc_supported(&self) -> bool {
        let binding = &self.settings.network_binding;
        self.settings.webtorrent.enabled
            && !self.settings.private_client
            && binding.mode == NetworkBindingMode::Any
            && binding.dns_policy == DnsPolicy::System
            && binding.enable_ipv4
            && binding.enable_ipv6
            && self.peer_policy_rx.borrow().restrictions.is_empty()
            && self.state.info_hash.len() == 20
            && self.settings.client_id.len() == 20
            && self.state.torrent.as_ref().is_none_or(|torrent| {
                torrent.info.private != Some(1) && !torrent.info.pieces.is_empty()
            })
            && self.network_activation.try_active().is_ok()
    }
    pub(super) fn rtc_reconcile(&mut self) {
        let supported = self.rtc_supported();
        self.rtc.capable.send_if_modified(|current| {
            let changed = *current != supported;
            *current = supported;
            changed
        });
        let allowed = supported && !self.state.is_paused;
        let activation = &self.network_activation;
        self.rtc.services.retain(|_, service| {
            let keep =
                allowed && activation.is_current(service.scope) && !service.requests.is_closed();
            if !keep {
                service.stop.send_replace(true);
            }
            keep
        });
        while let Some(result) = self.rtc.tasks.try_join_next() {
            if let Err(error) = result {
                tracing::debug!(%error, "WebTorrent execution task ended");
            }
        }
    }
    fn rtc_counters(&self) -> Counters {
        let left = self.state.multi_file_info.as_ref().map_or(1, |layout| {
            let piece_size = self
                .state
                .torrent
                .as_ref()
                .map_or(0, |torrent| torrent.info.piece_length.max(0) as u64);
            let verified = self
                .state
                .piece_manager
                .bitfield
                .iter()
                .enumerate()
                .filter(|(_, status)| **status == PieceStatus::Done)
                .map(|(index, _)| {
                    layout
                        .total_size
                        .saturating_sub(index as u64 * piece_size)
                        .min(piece_size)
                })
                .sum::<u64>();
            layout.total_size.saturating_sub(verified)
        });
        Counters {
            uploaded: self.state.session_total_uploaded,
            downloaded: self.state.session_total_downloaded,
            left,
        }
    }
    pub(super) fn rtc_announce(&mut self, url: &str, event: Option<Event>) -> bool {
        if !url.starts_with("wss://") && !url.starts_with("ws://") {
            return false;
        }
        if !self.rtc_supported() || self.state.is_paused {
            self.pending_started_announces.remove(url);
            self.pending_completion_announces.remove(url);
            self.apply_action(Action::TrackerError { url: url.into() });
            return true;
        }
        self.rtc_reconcile();
        let counters = self.rtc_counters();
        // Selected-file completion does not mean we possess every payload byte.
        if matches!(event, Some(Event::Completed)) && counters.left != 0 {
            self.pending_completion_announces.remove(url);
            return true;
        }
        if !self.rtc.services.contains_key(url) && self.rtc.services.len() >= 4 {
            self.pending_started_announces.remove(url);
            self.pending_completion_announces.remove(url);
            self.apply_action(Action::TrackerError { url: url.into() });
            return true;
        }
        if !self.rtc.services.contains_key(url) {
            let active = self
                .network_activation
                .try_active()
                .expect("support checked above");
            let scope = active.scope().clone();
            self.rtc.serial = self.rtc.serial.wrapping_add(1);
            let incarnation = self.rtc.serial;
            let (requests, receive) = mpsc::channel(2);
            let (stop, cancel) = watch::channel(false);
            let ice = IceOptions {
                servers: self
                    .settings
                    .webtorrent
                    .ice_servers
                    .iter()
                    .map(|server| webrtc::ice_transport::ice_server::RTCIceServer {
                        urls: server.urls.clone(),
                        username: server.username.clone(),
                        credential: server.credential.clone(),
                    })
                    .collect(),
                loopback: cfg!(test),
            };
            let parameters = Parameters {
                url: url.into(),
                incarnation,
                hash: Identity(
                    self.state
                        .info_hash
                        .as_slice()
                        .try_into()
                        .expect("validated hash"),
                ),
                local: Identity(
                    self.settings
                        .client_id
                        .as_bytes()
                        .try_into()
                        .expect("validated peer ID"),
                ),
                ice,
                response_timeout: Duration::from_secs(45),
                resources: self.resource_manager.clone(),
                reports: self.rtc.send.clone(),
            };
            self.rtc.tasks.spawn(async move {
                let _ = scope.run(tracker::run(parameters, receive, cancel)).await;
            });
            self.rtc.services.insert(
                url.into(),
                Service {
                    incarnation,
                    scope: active.scope().id(),
                    requests,
                    stop,
                    inflight: false,
                    event: None,
                },
            );
        }
        let service = self.rtc.services.get_mut(url).expect("created above");
        if service.inflight {
            return true;
        }
        match service.requests.try_send(Request { counters, event }) {
            Ok(()) => {
                service.inflight = true;
                service.event = event;
                if matches!(event, Some(Event::Started)) {
                    self.started_announce_scopes
                        .insert(url.into(), service.scope);
                }
                if matches!(event, Some(Event::Completed)) {
                    self.completion_announce_scopes
                        .insert(url.into(), service.scope);
                }
            }
            Err(error) => {
                tracing::debug!(%url, %error, "WebTorrent announce queue unavailable");
            }
        }
        true
    }
    pub(super) fn rtc_report(&mut self, report: Report) {
        let Some(service) = self.rtc.services.get(&report.url) else {
            return;
        };
        if service.incarnation != report.incarnation
            || !self.network_activation.is_current(service.scope)
        {
            return;
        }
        let scope_id = service.scope;
        match report.observation {
            Observation::Interval(seconds) => {
                let service = self
                    .rtc
                    .services
                    .get_mut(&report.url)
                    .expect("current service");
                service.inflight = false;
                if matches!(service.event, Some(Event::Started)) {
                    self.pending_started_announces.remove(&report.url);
                    self.started_announce_scopes.remove(&report.url);
                }
                if matches!(service.event, Some(Event::Completed)) {
                    self.pending_completion_announces.remove(&report.url);
                    self.completion_announce_scopes.remove(&report.url);
                }
                // State's downloading schedule reads min_interval. Preserve the WS interval there too.
                self.apply_action(Action::TrackerResponse {
                    url: report.url,
                    peers: Vec::new(),
                    interval: seconds,
                    min_interval: Some(seconds),
                });
                self.start_pending_completion_announces();
            }
            Observation::Failed(error) => {
                self.rtc.services.remove(&report.url);
                self.pending_started_announces.remove(&report.url);
                self.pending_completion_announces.remove(&report.url);
                tracing::debug!(url = %report.url, %error, "WebTorrent tracker failed");
                self.apply_action(Action::TrackerError { url: report.url });
            }
            Observation::Peer {
                identity,
                stream,
                driver,
            } => {
                if !self.rtc_supported() || self.state.is_paused || !self.state.accepting_new_peers
                {
                    return;
                }
                let prefix = format!("webrtc://{}/", hex::encode(identity.0));
                if self.state.peers.keys().any(|key| key.starts_with(&prefix)) {
                    return;
                }
                self.rtc.serial = self.rtc.serial.wrapping_add(1);
                let key = format!("{prefix}{}", self.rtc.serial);
                let (send, receive) = mpsc::channel(256);
                let Some(cancel) = self.register_peer(key.clone(), None, send) else {
                    return;
                };
                self.peer_network_scopes.insert(key.clone(), scope_id);
                self.apply_action(Action::PeerTransportSelected {
                    peer_id: key.clone(),
                    transport: PeerTransportKind::WebRtc,
                });
                let bitfield = if self.state.torrent.is_some() {
                    Some(self.generate_bitfield())
                } else {
                    None
                };
                let session = PeerSession::new(PeerSessionParameters {
                    info_hash: self.state.info_hash.clone(),
                    // BEP 9 transfers the info dictionary, not the enclosing .torrent file.
                    torrent_metadata_length: self
                        .state
                        .torrent
                        .as_ref()
                        .map(|torrent| torrent.info_dict_bencode.len() as i64),
                    connection_type: ConnectionType::Outgoing,
                    torrent_manager_rx: receive,
                    torrent_manager_tx: self.torrent_manager_tx.clone(),
                    peer_ip_port: key.clone(),
                    client_id: self.settings.client_id.as_bytes().to_vec(),
                    global_dl_bucket: self.global_dl_bucket.clone(),
                    global_ul_bucket: self.global_ul_bucket.clone(),
                    shutdown_tx: self.shutdown_tx.clone(),
                    network_scope_id: Some(scope_id),
                    session_cancel: cancel,
                })
                .expect_rtc_identity(identity.0);
                let manager = self.torrent_manager_tx.clone();
                let scope = self
                    .network_activation
                    .try_active()
                    .expect("supported execution")
                    .scope()
                    .clone();
                let mut capable = self.rtc.capable.subscribe();
                self.rtc.tasks.spawn(async move {
                    let wire = async {
                        tokio::select! {
                            result = session.run(stream, Vec::new(), bitfield) => result,
                            _ = async { loop { if !*capable.borrow_and_update() || capable.changed().await.is_err() { break; } } } => Ok(()),
                        }
                    };
                    let (result, _) = driver.run_with(scope.run(wire)).await;
                    if let Err(error) = result { tracing::debug!(peer = %key, %error, "RTC peer session ended"); }
                    let _ = manager.send(TorrentCommand::DisconnectGeneration { peer_id: key, scope_id }).await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::networking::webtorrent::native::Negotiation;
    fn manager() -> (TorrentManager, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let mut params = super::super::resource_tests::build_test_params();
        params.torrent_data_path = Some(directory.path().into());
        let settings = Arc::make_mut(&mut params.settings);
        settings.webtorrent.enabled = true;
        settings.client_id = "Q".repeat(20);
        (
            TorrentManager::from_torrent(
                params,
                super::super::resource_tests::create_dummy_torrent(2),
            )
            .unwrap(),
            directory,
        )
    }
    fn service(manager: &mut TorrentManager) -> Receiver<Request> {
        let (requests, receive) = mpsc::channel(2);
        manager.rtc.services.insert(
            "ws://tracker.invalid/announce".into(),
            Service {
                incarnation: 19,
                scope: manager
                    .network_activation
                    .try_active()
                    .unwrap()
                    .scope()
                    .id(),
                requests,
                stop: watch::channel(false).0,
                inflight: false,
                event: None,
            },
        );
        receive
    }
    async fn candidate() -> (
        Observation,
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let ice = IceOptions {
            loopback: true,
            ..Default::default()
        };
        let offer = Negotiation::create(&ice, true).await.unwrap();
        let answer = Negotiation::create(&ice, false).await.unwrap();
        offer
            .accept(answer.answer(offer.offer().await.unwrap()).await.unwrap())
            .await
            .unwrap();
        let ((stream, driver), (other, other_driver)) =
            tokio::try_join!(offer.connected(), answer.connected()).unwrap();
        (
            Observation::Peer {
                identity: Identity([107; 20]),
                stream,
                driver,
            },
            other,
            tokio::spawn(other_driver.run()),
        )
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn late_candidates_close_after_pause_admission_pressure_or_tracker_replacement() {
        for condition in 0..3 {
            let (mut manager, _directory) = manager();
            let _requests = service(&mut manager);
            let (observation, other, transport) = candidate().await;
            if condition == 0 {
                manager.state.is_paused = true;
            }
            if condition == 1 {
                manager.state.accepting_new_peers = false;
            }
            manager.rtc_report(Report {
                url: "ws://tracker.invalid/announce".into(),
                incarnation: if condition == 2 { 18 } else { 19 },
                observation,
            });
            assert!(manager.state.peers.is_empty());
            let _ = tokio::time::timeout(Duration::from_secs(4), transport)
                .await
                .expect("rejected candidate must close its connection")
                .unwrap();
            drop(other);
            manager.rtc_shutdown().await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn old_session_termination_cannot_remove_replacement() {
        let (mut manager, _directory) = manager();
        let _requests = service(&mut manager);
        let (observation, old_other, old_transport) = candidate().await;
        manager.rtc_report(Report {
            url: "ws://tracker.invalid/announce".into(),
            incarnation: 19,
            observation,
        });
        let old = manager
            .state
            .peers
            .keys()
            .next()
            .expect("state admitted peer")
            .clone();
        let scope_id = manager.peer_network_scopes[&old];
        manager.apply_action(Action::PeerDisconnected {
            peer_id: old.clone(),
            force: true,
        });
        let (observation, new_other, new_transport) = candidate().await;
        manager.rtc_report(Report {
            url: "ws://tracker.invalid/announce".into(),
            incarnation: 19,
            observation,
        });
        let replacement = manager
            .state
            .peers
            .keys()
            .next()
            .expect("replacement admitted")
            .clone();
        assert_ne!(old, replacement);
        manager.handle_generation_scoped_peer_disconnected(old, scope_id);
        assert!(manager.state.peers.contains_key(&replacement));
        manager.rtc_shutdown().await;
        drop((old_other, new_other));
        let _ = old_transport.await;
        let _ = new_transport.await;
    }
}
