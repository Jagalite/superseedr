// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Execution ownership for WebTorrent. All torrent decisions still enter TorrentState as actions.
use super::*;
use crate::networking::webtorrent::{
    rtc::WebRtcSessionConfig,
    stream::WebRtcStream,
    tracker_worker::{
        webtorrent_tracker_worker, WebTorrentAnnounceStats, WebTorrentTrackerConfig,
        WebTorrentTrackerEvent,
    },
};
use crate::networking::NetworkBindingMode;
use crate::resource::PermitGuard;

const MAX_TRACKERS: usize = 4;
const MAX_PENDING_SESSIONS: usize = 8;

pub(super) enum Event {
    Tracker {
        url: String,
        id: u64,
        scope: NetworkScopeId,
        event: WebTorrentTrackerEvent,
    },
    TrackerClosed {
        url: String,
        id: u64,
    },
    Admit {
        remote: [u8; 20],
        key: String,
        scope: NetworkScopeId,
        epoch: u64,
        connection: Option<(WebRtcStream, PermitGuard)>,
    },
    SessionClosed {
        remote: [u8; 20],
        key: String,
    },
}
struct Worker {
    id: u64,
    scope: NetworkScopeId,
    cancel: watch::Sender<bool>,
    stats: watch::Sender<WebTorrentAnnounceStats>,
}
pub(super) struct Runtime {
    workers: HashMap<String, Worker>,
    tasks: JoinSet<Event>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    remotes: HashMap<[u8; 20], String>,
    next_id: u64,
    pending: usize,
    active_scope: Option<NetworkScopeId>,
    epoch: watch::Sender<u64>,
}
impl Default for Runtime {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(64);
        Self {
            workers: HashMap::new(),
            tasks: JoinSet::new(),
            tx,
            rx,
            remotes: HashMap::new(),
            next_id: 0,
            pending: 0,
            active_scope: None,
            epoch: watch::channel(0).0,
        }
    }
}
impl Runtime {
    pub(super) async fn next(&mut self) -> Event {
        loop {
            tokio::select! {
                biased;
                Some(event) = self.rx.recv() => return event,
                Some(result) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    match result {
                        Ok(event) => return event,
                        Err(error) => tracing::error!(%error, "WebTorrent task failed"),
                    }
                }
            }
        }
    }
    pub(super) async fn shutdown(&mut self) {
        self.stop_trackers();
        // Drain both the result queue and owned tasks so a full result queue cannot
        // prevent cancellation from completing.
        let drain = async {
            while !self.tasks.is_empty() {
                tokio::select! {
                    _ = self.rx.recv() => {},
                    _ = self.tasks.join_next() => {},
                }
            }
            while self.rx.try_recv().is_ok() {}
        };
        if timeout(Duration::from_secs(5), drain).await.is_err() {
            self.tasks.shutdown().await;
            while self.rx.try_recv().is_ok() {}
        }
    }
    fn id(&mut self) -> u64 {
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("WebTorrent incarnation exhausted");
        self.next_id
    }
    pub(super) fn stop_trackers(&mut self) {
        self.remotes.clear();
        for (_, worker) in self.workers.drain() {
            let _ = worker.cancel.send(true);
        }
        self.epoch
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop_trackers();
    }
}
impl TorrentManager {
    fn webtorrent_scope(&self) -> Result<NetworkScope, &'static str> {
        if self.settings.client_id.len() != 20 {
            return Err("WebTorrent requires a 20-byte client ID");
        }
        if self.state.is_paused {
            return Err("torrent is paused");
        }
        if !self.peer_policy_rx.borrow().restrictions.is_empty() {
            return Err(
                "WebRTC cannot enforce an IP restriction without a verified remote address",
            );
        }
        if self.state.torrent.as_ref().is_some_and(|torrent| {
            torrent.info.meta_version == Some(2) && torrent.info.pieces.is_empty()
        }) {
            return Err("the WebTorrent peer protocol requires a v1 or hybrid torrent");
        }
        let active = self
            .network_activation
            .try_active()
            .map_err(|_| "networking is unavailable")?;
        let scope = active.scope().clone();
        let lease = scope.lease();
        if lease.binding_mode() != NetworkBindingMode::Any || !lease.uses_system_dns() {
            return Err("the RTC library cannot enforce interface binding or bound DNS");
        }
        if !lease.ipv4_enabled() || !lease.ipv6_enabled() {
            return Err("the RTC library requires unrestricted dual-stack networking");
        }
        Ok(scope)
    }
    pub(super) fn sync_webtorrent(&mut self) {
        let scope = self.webtorrent_scope().ok().map(|scope| scope.id());
        let previous = self.webtorrent.active_scope;
        self.webtorrent.active_scope = scope;
        let tracker_scope_changed = scope.is_none()
            || self
                .webtorrent
                .workers
                .values()
                .any(|worker| Some(worker.scope) != scope);
        if tracker_scope_changed
            && (!self.webtorrent.workers.is_empty()
                || self.webtorrent.pending > 0
                || previous.is_some() && previous != scope)
        {
            self.webtorrent.stop_trackers();
        }
        if previous.is_some() && previous != scope {
            // Report loss of transport capability through the same state action used
            // for a failed socket. Admission pressure alone never enters this path.
            let peers = self
                .state
                .peers
                .iter()
                .filter(|(_, peer)| peer.transport_kind == PeerTransportKind::WebRtc)
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for peer_id in peers {
                self.apply_action(Action::PeerDisconnected {
                    peer_id,
                    force: true,
                });
            }
        }
    }
    pub(super) fn announce_webtorrent(&mut self, url: String, completed: bool) {
        let scope = match self.webtorrent_scope() {
            Ok(scope) => scope,
            Err(reason) => {
                tracing::warn!(tracker = %url, %reason, "WebTorrent tracker unavailable");
                self.apply_action(Action::TrackerError { url });
                return;
            }
        };
        let Ok(info_hash) = <[u8; 20]>::try_from(self.state.info_hash.as_slice()) else {
            return;
        };
        let Ok(peer_id) = <[u8; 20]>::try_from(self.settings.client_id.as_bytes()) else {
            return;
        };
        let left = self.state.multi_file_info.as_ref().map_or(1, |mfi| {
            let piece_length = self
                .state
                .torrent
                .as_ref()
                .map_or(0, |t| t.info.piece_length as u64);
            self.state
                .piece_manager
                .bitfield
                .iter()
                .enumerate()
                .filter(|(_, status)| **status != PieceStatus::Done)
                .map(|(index, _)| {
                    piece_length.min(mfi.total_size.saturating_sub(index as u64 * piece_length))
                })
                .sum()
        });
        let stats = WebTorrentAnnounceStats {
            uploaded: self.state.session_total_uploaded,
            downloaded: self.state.session_total_downloaded,
            left,
            completed: completed || self.pending_completion_announces.contains(&url),
        };
        if let Some(worker) = self.webtorrent.workers.get(&url) {
            if worker.scope == scope.id() && worker.stats.send(stats).is_ok() {
                return;
            }
        }
        if self.webtorrent.workers.len() >= MAX_TRACKERS {
            self.apply_action(Action::TrackerError { url });
            return;
        }
        let id = self.webtorrent.id();
        let scope_id = scope.id();
        let (stats_tx, stats_rx) = watch::channel(stats);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let config = WebTorrentTrackerConfig {
            url: url.clone(),
            info_hash,
            peer_id,
            key: rand::random(),
            num_offers: 2,
            max_incoming_negotiations: 2,
            rtc: WebRtcSessionConfig {
                bind_addrs: vec!["0.0.0.0:0".into(), "[::]:0".into()],
                ice_servers: self.settings.webtorrent_ice_servers.clone(),
            },
        };
        self.webtorrent.workers.insert(
            url.clone(),
            Worker {
                id,
                scope: scope_id,
                cancel: cancel_tx,
                stats: stats_tx,
            },
        );
        let tx = self.webtorrent.tx.clone();
        self.webtorrent.tasks.spawn(async move {
            let mut invalidated = scope.subscribe_invalidation();
            let worker = webtorrent_tracker_worker(config, stats_rx, cancel_rx, event_tx);
            tokio::pin!(worker);
            loop {
                tokio::select! {
                    biased;
                    _ = invalidated.changed() => break,
                    Some(event) = event_rx.recv() => {
                        let event = Event::Tracker { url: url.clone(), id, scope: scope_id, event };
                        tokio::select! {
                            _ = invalidated.changed() => break,
                            result = tx.send(event) => if result.is_err() { break; },
                        }
                    }
                    _ = &mut worker => {
                        // The worker may have queued its final error immediately before returning.
                        while let Ok(event) = event_rx.try_recv() {
                            if tx.send(Event::Tracker { url: url.clone(), id, scope: scope_id, event }).await.is_err() { break; }
                        }
                        break;
                    }
                }
            }
            Event::TrackerClosed { url, id }
        });
    }
    pub(super) fn handle_webtorrent_event(&mut self, event: Event) {
        match event {
            Event::Tracker {
                url,
                id,
                scope,
                event,
            } => {
                if !self.network_activation.is_current(scope)
                    || self.webtorrent.workers.get(&url).is_none_or(|w| w.id != id)
                {
                    return;
                }
                match event {
                    WebTorrentTrackerEvent::Connected => {}
                    WebTorrentTrackerEvent::Interval(interval) => {
                        self.pending_started_announces.remove(&url);
                        self.pending_completion_announces.remove(&url);
                        self.apply_action(Action::TrackerResponse {
                            url,
                            peers: Vec::new(),
                            interval: interval.interval_secs,
                            min_interval: interval.min_interval_secs,
                        });
                    }
                    WebTorrentTrackerEvent::NegotiationFailed(error) => {
                        tracing::debug!(%url, %error, "WebRTC negotiation failed");
                    }
                    WebTorrentTrackerEvent::Failed(error) => {
                        tracing::debug!(%url, %error, "WebTorrent tracker error");
                        self.apply_action(Action::TrackerError { url });
                    }
                    WebTorrentTrackerEvent::PeerReady {
                        peer_id,
                        offer_id,
                        stream,
                    } => {
                        tracing::trace!(offer = %hex::encode(offer_id), "WebRTC negotiation ready for admission");
                        self.prepare_webtorrent_session(scope, peer_id, stream);
                    }
                }
            }
            Event::TrackerClosed { url, id } => {
                if self
                    .webtorrent
                    .workers
                    .get(&url)
                    .is_some_and(|w| w.id == id)
                {
                    self.webtorrent.workers.remove(&url);
                }
            }
            Event::Admit {
                remote,
                key,
                scope,
                epoch,
                connection,
            } => {
                self.webtorrent.pending = self.webtorrent.pending.saturating_sub(1);
                if self.webtorrent.remotes.get(&remote) != Some(&key) {
                    return;
                }
                let Some((stream, permit)) = connection else {
                    self.webtorrent.remotes.remove(&remote);
                    return;
                };
                if *self.webtorrent.epoch.borrow() != epoch
                    || !self.should_accept_new_peers()
                    || !self.webtorrent_scope().is_ok_and(|s| s.id() == scope)
                {
                    self.webtorrent.remotes.remove(&remote);
                    return;
                }
                let (tx, rx) = mpsc::channel(256);
                let Some(session_cancel) = self.register_peer(key.clone(), None, tx) else {
                    self.webtorrent.remotes.remove(&remote);
                    return;
                };
                self.peer_network_scopes.insert(key.clone(), scope);
                self.apply_action(Action::PeerTransportSelected {
                    peer_id: key.clone(),
                    transport: PeerTransportKind::WebRtc,
                });
                let session = PeerSession::new(PeerSessionParameters {
                    info_hash: self.state.info_hash.clone(),
                    torrent_metadata_length: self.webtorrent_advertised_metadata_length(),
                    connection_type: ConnectionType::Outgoing,
                    torrent_manager_rx: rx,
                    torrent_manager_tx: self.torrent_manager_tx.clone(),
                    peer_ip_port: key.clone(),
                    client_id: self.settings.client_id.as_bytes().to_vec(),
                    global_dl_bucket: self.global_dl_bucket.clone(),
                    global_ul_bucket: self.global_ul_bucket.clone(),
                    shutdown_tx: self.shutdown_tx.clone(),
                    network_scope_id: Some(scope),
                    session_cancel,
                })
                .with_expected_peer_id(remote);
                let bitfield = if self.state.torrent.is_some() {
                    Some(self.generate_bitfield())
                } else {
                    None
                };
                let mut closed = stream.closed();
                self.webtorrent.tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = session.run(stream, Vec::new(), bitfield).await {
                        tracing::debug!(peer = %key, %error, "WebRTC session ended");
                    }
                    while !*closed.borrow_and_update() {
                        if closed.changed().await.is_err() {
                            break;
                        }
                    }
                    Event::SessionClosed { remote, key }
                });
            }
            Event::SessionClosed { remote, key } => {
                if self.webtorrent.remotes.get(&remote) == Some(&key) {
                    self.webtorrent.remotes.remove(&remote);
                }
                self.apply_action(Action::PeerDisconnected {
                    peer_id: key,
                    force: true,
                });
            }
        }
    }
    fn prepare_webtorrent_session(
        &mut self,
        scope_id: NetworkScopeId,
        remote: [u8; 20],
        stream: WebRtcStream,
    ) {
        if !self.should_accept_new_peers()
            || self.webtorrent.pending >= MAX_PENDING_SESSIONS
            || remote.as_slice() == self.settings.client_id.as_bytes()
            || self.webtorrent.remotes.contains_key(&remote)
        {
            return;
        }
        let Ok(scope) = self.webtorrent_scope() else {
            return;
        };
        if scope.id() != scope_id {
            return;
        }
        let id = self.webtorrent.id();
        // The wire identity and this connection incarnation are distinct. Late commands cannot
        // affect a replacement session because all peer-wire commands use this unique key.
        let key = format!("webrtc://{}/{id}", hex::encode(remote));
        self.webtorrent.remotes.insert(remote, key.clone());
        self.webtorrent.pending += 1;
        let resources = self.resource_manager.clone();
        let mut epoch_rx = self.webtorrent.epoch.subscribe();
        let epoch = *epoch_rx.borrow_and_update();
        self.webtorrent.tasks.spawn(async move {
            let mut invalidation = scope.subscribe_invalidation();
            let permit = tokio::select! {
                biased;
                _ = invalidation.changed() => None,
                _ = epoch_rx.changed() => None,
                result = tokio::time::timeout(Duration::from_secs(30), resources.acquire_peer_connection()) => result.ok().and_then(Result::ok),
            };
            Event::Admit { remote, key, scope: scope_id, epoch, connection: permit.map(|permit| (stream, permit)) }
        });
    }
    fn webtorrent_advertised_metadata_length(&self) -> Option<i64> {
        let len = self.state.torrent.as_ref()?.info_dict_bencode.len();
        (len > 0).then(|| i64::try_from(len).ok()).flatten()
    }
    pub(super) fn send_webtorrent_metadata_piece(&mut self, peer_id: &str, piece: usize) {
        let (Some(torrent), Some(peer)) =
            (self.state.torrent.as_ref(), self.state.peers.get(peer_id))
        else {
            return;
        };
        if peer.transport_kind != PeerTransportKind::WebRtc {
            return;
        }
        let total_size = torrent.info_dict_bencode.len();
        let start = piece.saturating_mul(crate::networking::protocol::METADATA_PIECE_SIZE);
        let command = if start >= total_size {
            TorrentCommand::RejectMetadata { piece }
        } else {
            let end = start
                .saturating_add(crate::networking::protocol::METADATA_PIECE_SIZE)
                .min(total_size);
            TorrentCommand::UploadMetadata {
                piece,
                total_size,
                data: torrent.info_dict_bencode[start..end].to_vec(),
            }
        };
        if peer.peer_tx.try_send(command).is_err() {
            // Do not silently discard protocol control traffic on a wedged peer queue.
            self.apply_action(Action::PeerDisconnected {
                peer_id: peer_id.to_string(),
                force: true,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::networking::webtorrent::browser_interop_tests::relay_browser_negotiation;
    use crate::resource::{ResourceManager, ResourceType};
    use tokio_tungstenite::accept_async;

    fn params(
        path: &std::path::Path,
        peer: &str,
    ) -> (
        TorrentParameters,
        Sender<ManagerCommand>,
        watch::Receiver<TorrentMetrics>,
        JoinSet<()>,
    ) {
        let mut params = super::super::resource_tests::build_test_params();
        assert_eq!(peer.len(), 20);
        params.torrent_data_path = Some(path.to_path_buf());
        Arc::make_mut(&mut params.settings).client_id = peer.into();
        let (control, rx) = mpsc::channel(16);
        params.manager_command_rx = rx;
        let (metrics, metrics_rx) = watch::channel(TorrentMetrics::default());
        params.metrics_tx = metrics;
        let (events, mut events_rx) = mpsc::channel(128);
        params.manager_event_tx = events;
        let mut tasks = JoinSet::new();
        tasks.spawn(async move { while events_rx.recv().await.is_some() {} });
        let mut limits = HashMap::new();
        for kind in [
            ResourceType::PeerConnection,
            ResourceType::DiskRead,
            ResourceType::DiskWrite,
        ] {
            limits.insert(kind, (16, 16));
        }
        limits.insert(ResourceType::Reserve, (0, 0));
        let (shutdown, _) = broadcast::channel(1);
        let (resources, client) = ResourceManager::new(limits, shutdown.clone());
        params.resource_manager = client;
        tasks.spawn(async move {
            let _shutdown = shutdown;
            resources.run().await;
        });
        (params, control, metrics_rx, tasks)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn magnet_downloads_metadata_and_verified_payload_through_real_managers() {
        crate::install_webtorrent_crypto_provider().unwrap();
        if std::env::var_os("SUPERSEEDR_TEST_TRACE").is_some() {
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .try_init();
        }
        let seed_dir = tempfile::tempdir().unwrap();
        let download_dir = tempfile::tempdir().unwrap();
        let payload = (0..49_321).map(|n| (n % 251) as u8).collect::<Vec<_>>();
        let piece_length = 16_384;
        let info = crate::torrent_file::Info {
            name: "orbital-sample.bin".into(),
            piece_length,
            length: payload.len() as i64,
            pieces: payload
                .chunks(piece_length as usize)
                .flat_map(|p| sha1::Sha1::digest(p).to_vec())
                .collect(),
            files: Vec::new(),
            private: None,
            md5sum: None,
            meta_version: None,
            file_tree: None,
        };
        let metadata = serde_bencode::to_bytes(&info).unwrap();
        let hash = sha1::Sha1::digest(&metadata);
        tokio::fs::write(seed_dir.path().join(&info.name), &payload)
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/announce", listener.local_addr().unwrap());
        let mut relay_tasks = JoinSet::new();
        relay_tasks.spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            let mut first = accept_async(first).await.unwrap();
            let (second, _) = listener.accept().await.unwrap();
            let mut second = accept_async(second).await.unwrap();
            relay_browser_negotiation(&mut first, &mut second)
                .await
                .unwrap();
            // Keep both signaling connections alive for the full payload transfer.
            std::future::pending::<()>().await;
        });
        let torrent = Torrent {
            announce: Some(url.clone()),
            announce_list: None,
            url_list: None,
            info,
            info_dict_bencode: metadata,
            created_by: None,
            creation_date: None,
            encoding: None,
            comment: None,
            piece_layers: None,
        };
        let (seed_params, seed_control, mut seed_metrics, _seed_resources) =
            params(seed_dir.path(), "-SS1000-SEED00000001");
        let (download_params, download_control, mut download_metrics, _download_resources) =
            params(download_dir.path(), "-SS1000-LOAD00000001");
        let seed = TorrentManager::from_torrent(seed_params, torrent).unwrap();
        let magnet = format!(
            "magnet:?xt=urn:btih:{}&tr={}",
            hex::encode(hash),
            urlencoding::encode(&url)
        );
        let download = TorrentManager::from_magnet(
            download_params,
            magnet_url::Magnet::new(&magnet).unwrap(),
            &magnet,
        )
        .unwrap();
        assert!(download.state.torrent.is_none());
        assert_eq!(download.state.trackers.len(), 1);
        assert!(download
            .state
            .trackers
            .keys()
            .all(|url| url.starts_with("ws://")));
        let mut managers = JoinSet::new();
        managers.spawn(async move { seed.run(false).await.map_err(|e| e.to_string()) });
        managers.spawn(async move { download.run(false).await.map_err(|e| e.to_string()) });
        let transferred = timeout(Duration::from_secs(60), async {
            loop {
                download_metrics.changed().await.unwrap();
                let metrics = download_metrics.borrow().clone();
                if metrics.is_complete {
                    break metrics;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "download timed out: {:?}; seed: {:?}",
                *download_metrics.borrow(),
                *seed_metrics.borrow()
            )
        });
        assert_eq!(transferred.number_of_pieces_completed, 4);
        assert!(transferred.session_total_downloaded >= payload.len() as u64);
        assert!(transferred
            .peers
            .iter()
            .any(|p| p.address.starts_with("webrtc://") && p.total_downloaded > 0));
        assert!(transferred
            .peers
            .iter()
            .all(|p| p.address.starts_with("webrtc://")));
        let retained = tokio::fs::read(download_dir.path().join("orbital-sample.bin"))
            .await
            .unwrap();
        assert_eq!(retained, payload);
        timeout(Duration::from_secs(3), async {
            while seed_metrics.borrow().session_total_uploaded < payload.len() as u64 {
                seed_metrics.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        seed_control.send(ManagerCommand::Shutdown).await.unwrap();
        download_control
            .send(ManagerCommand::Shutdown)
            .await
            .unwrap();
        timeout(Duration::from_secs(5), async {
            while let Some(result) = managers.join_next().await {
                result.unwrap().unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn magnets_are_eligible_and_pause_closes_tracker_execution() {
        let (mut manager, _, _, _, _) = super::super::resource_tests::setup_test_harness();
        Arc::make_mut(&mut manager.settings).client_id = "-SS1000-TEST00000001".into();
        assert!(manager.state.torrent.is_none());
        assert!(manager.webtorrent_scope().is_ok());
        manager.apply_action(Action::Pause);
        assert!(manager.webtorrent_scope().is_err());
        assert!(manager.webtorrent.workers.is_empty());
    }
    #[tokio::test]
    async fn old_admission_and_disconnect_cannot_remove_a_replacement() {
        let (mut manager, _, _, _, _) = super::super::resource_tests::setup_test_harness();
        let remote = [5; 20];
        let replacement = "webrtc://remote/2".to_string();
        let (tx, _rx) = mpsc::channel(16);
        assert!(manager
            .register_peer(replacement.clone(), None, tx)
            .is_some());
        manager
            .webtorrent
            .remotes
            .insert(remote, replacement.clone());
        manager.webtorrent.pending = 1;
        let scope = manager
            .network_activation
            .try_active()
            .unwrap()
            .scope()
            .id();
        manager.handle_webtorrent_event(Event::Admit {
            remote,
            key: "webrtc://remote/1".into(),
            scope,
            epoch: 0,
            connection: None,
        });
        manager.handle_webtorrent_event(Event::SessionClosed {
            remote,
            key: "webrtc://remote/1".into(),
        });
        assert_eq!(manager.webtorrent.remotes.get(&remote), Some(&replacement));
        assert!(manager.state.peers.contains_key(&replacement));
    }

    #[tokio::test]
    async fn native_tracker_retry_does_not_start_a_websocket_announce() {
        let (mut manager, _, _, _, _) = super::super::resource_tests::setup_test_harness();
        Arc::make_mut(&mut manager.settings).client_id = "-SS1000-TEST00000001".into();
        let url = "ws://127.0.0.1:1/announce".to_string();
        manager.pending_started_announces.insert(url.clone());
        manager.pending_completion_announces.insert(url);
        manager.start_pending_started_announces();
        manager.start_pending_completion_announces();
        assert!(manager.webtorrent.workers.is_empty());
    }

    #[tokio::test]
    async fn admission_pressure_keeps_rtc_peers_but_lost_ip_policy_capability_removes_them() {
        use crate::peer_manager::{PeerPolicy, PeerRestriction, PeerRestrictionReason};
        let (mut manager, _, _, _, _) = super::super::resource_tests::setup_test_harness();
        Arc::make_mut(&mut manager.settings).client_id = "-SS1000-TEST00000001".into();
        manager.sync_webtorrent();
        let key = "webrtc://policy-peer/1".to_string();
        let (tx, _rx) = mpsc::channel(16);
        assert!(manager.register_peer(key.clone(), None, tx).is_some());
        manager.apply_action(Action::PeerTransportSelected {
            peer_id: key.clone(),
            transport: PeerTransportKind::WebRtc,
        });
        manager.state.accepting_new_peers = false;
        manager.sync_webtorrent();
        assert!(manager.state.peers.contains_key(&key));
        let now = std::time::SystemTime::now();
        let policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                "192.0.2.8".parse().unwrap(),
                PeerRestriction {
                    detected_at: now,
                    blocked_until: now + Duration::from_secs(60),
                    torrent_info_hash: None,
                    reason: PeerRestrictionReason::Manual,
                },
            )]),
        });
        manager.peer_policy_rx = watch::channel(policy.clone()).1;
        manager.apply_action(Action::PeerPolicyUpdated { policy });
        assert!(!manager.state.peers.contains_key(&key));
    }
}
