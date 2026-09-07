// SPDX-License-Identifier: GPL-3.0-or-later
//! Native WebTorrent execution attached to the existing manager.
use super::*;
use crate::networking::activation::ActiveNetwork;
use crate::networking::webtorrent::rtc_trace;
use crate::networking::webtorrent::{
    tracker::{self, Observation, Parameters, Report, Request},
    transport::IceOptions,
    wire::{Counters, Event, Identity},
};

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
    tasks: crate::execution::JoinSet<()>,
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
            tasks: crate::execution::JoinSet::new(),
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
        let _ = crate::execution::time::timeout(Duration::from_secs(2), async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        for service in self.services.values() {
            service.stop.send_replace(true);
        }
        self.services.clear();
        if crate::execution::time::timeout(Duration::from_secs(5), async {
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
    pub(super) fn rtc_cleanup(&mut self) -> impl std::future::Future<Output = ()> + 'static {
        let counters = self.rtc_counters();
        let runtime = std::mem::take(&mut self.rtc);
        runtime.shutdown(counters)
    }
    pub(super) async fn rtc_shutdown(&mut self) {
        let _ = self.shutdown_tx.send(());
        self.rtc_cleanup().await;
    }
    fn rtc_network(&self) -> Option<Arc<ActiveNetwork>> {
        let active = self.network_activation.try_active().ok()?;
        #[cfg(not(target_arch = "wasm32"))]
        let platform_supported = active.scope().lease().permits_unrestricted_transport();
        #[cfg(target_arch = "wasm32")]
        let platform_supported = crate::networking::webtorrent::browser::available();
        (!self.settings.private_client
            && platform_supported
            && self.peer_policy_rx.borrow().restrictions.is_empty()
            && self.state.info_hash.len() == 20
            && self.settings.client_id.len() == 20
            && self.state.torrent.as_ref().is_none_or(|torrent| {
                torrent.info.private != Some(1) && !torrent.info.pieces.is_empty()
            }))
        .then_some(active)
    }
    pub(super) fn rtc_reconcile(&mut self) {
        let supported = self.rtc_network().is_some();
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
        self.rtc_reconcile();
        let Some(active) = self.rtc_network().filter(|_| !self.state.is_paused) else {
            self.pending_started_announces.remove(url);
            self.pending_completion_announces.remove(url);
            self.apply_action(Action::TrackerError { url: url.into() });
            return true;
        };
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
            let scope = active.scope().clone();
            self.rtc.serial = self.rtc.serial.wrapping_add(1);
            let incarnation = self.rtc.serial;
            let (requests, receive) = mpsc::channel(2);
            let (stop, cancel) = watch::channel(false);
            let ice = IceOptions::from_settings(&self.settings);
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
                rtc_trace!("manager_peer_received", {"hash":hex::encode(&self.state.info_hash), "tracker":report.url,
                    "peer":hex::encode(identity.0)});
                let Some(active) = self
                    .rtc_network()
                    .filter(|active| active.scope().id() == scope_id)
                else {
                    return;
                };
                if self.state.is_paused || !self.state.accepting_new_peers {
                    rtc_trace!("manager_peer_rejected", {"hash":hex::encode(&self.state.info_hash), "tracker":report.url,
                        "peer":hex::encode(identity.0), "reason":"state_gate"});
                    return;
                }
                let prefix = format!("webrtc://{}/", hex::encode(identity.0));
                if self.state.peers.keys().any(|key| key.starts_with(&prefix)) {
                    rtc_trace!("manager_peer_rejected", {"hash":hex::encode(&self.state.info_hash), "tracker":report.url,
                        "peer":hex::encode(identity.0), "reason":"duplicate"});
                    return;
                }
                self.rtc.serial = self.rtc.serial.wrapping_add(1);
                let key = format!("{prefix}{}", self.rtc.serial);
                let (send, receive) = mpsc::channel(256);
                let Some(cancel) = self.register_peer(key.clone(), None, send) else {
                    rtc_trace!("manager_peer_rejected", {"hash":hex::encode(&self.state.info_hash), "tracker":report.url,
                        "peer":hex::encode(identity.0), "reason":"register_denied"});
                    return;
                };
                rtc_trace!("manager_peer_admitted", {"hash":hex::encode(&self.state.info_hash), "tracker":report.url,
                    "peer":hex::encode(identity.0), "key":key});
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
                let scope = active.scope().clone();
                let mut capable = self.rtc.capable.subscribe();
                #[cfg(all(feature = "synthetic-load", not(target_arch = "wasm32")))]
                let trace_scope = (hex::encode(&self.state.info_hash), report.url.clone());
                self.rtc.tasks.spawn(async move {
                    let wire = async {
                        tokio::select! {
                            result = session.run(stream, Vec::new(), bitfield) => result,
                            _ = async {
                                while *capable.borrow_and_update() {
                                    if capable.changed().await.is_err() {
                                        break;
                                    }
                                }
                            } => Ok(()),
                        }
                    };
                    let (result, _transport_result) = driver.run_with(scope.run(wire)).await;
                    rtc_trace!("manager_session_ended", {"hash":trace_scope.0, "tracker":trace_scope.1,
                        "peer":hex::encode(identity.0), "key":key,
                        "wire_error":result.as_ref().err().map(ToString::to_string),
                        "transport_error":_transport_result.as_ref().err().map(ToString::to_string)});
                    if let Err(error) = result {
                        tracing::debug!(peer = %key, %error, "RTC peer session ended");
                    }
                    let _ = manager
                        .send(TorrentCommand::DisconnectGeneration {
                            peer_id: key,
                            scope_id,
                        })
                        .await;
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
    #[tokio::test]
    async fn default_webtorrent_retains_privacy_constraints() {
        let (mut manager, _directory) = manager();
        assert!(manager.rtc_network().is_some());
        Arc::make_mut(&mut manager.settings).private_client = true;
        assert!(manager.rtc_network().is_none());
        Arc::make_mut(&mut manager.settings).private_client = false;

        let _requests = service(&mut manager);
        let stopped = manager
            .rtc
            .services
            .values()
            .next()
            .unwrap()
            .stop
            .subscribe();
        manager.state.torrent.as_mut().unwrap().info.private = Some(1);
        manager.rtc_reconcile();
        assert!(manager.rtc_network().is_none());
        assert!(!*manager.rtc.capable.borrow());
        assert!(*stopped.borrow());
        assert!(manager.rtc.services.is_empty());
        manager.rtc_shutdown().await;
    }

    #[tokio::test]
    async fn live_binding_changes_gate_rtc_without_refreshing_manager_settings() {
        use crate::networking::{
            DnsPolicy, NetworkActivationPublisher, NetworkBindingConfig, NetworkBindingMode,
            NetworkSupervisor,
        };
        let (handle, supervisor) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = NetworkActivationPublisher::channel();
        let (mut manager, _directory) = manager();
        manager.network_activation_rx = activation.subscribe();
        manager.network_activation = activation;
        publisher
            .activate(handle.try_lease().unwrap(), 41000)
            .unwrap();
        assert!(manager.rtc_network().is_some());
        let startup_settings = manager.settings.network_binding.clone();

        let local = NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            enable_ipv6: false,
            ipv4_address: Some(std::net::Ipv4Addr::LOCALHOST),
            ..Default::default()
        };
        let policies = [
            local.clone(),
            NetworkBindingConfig {
                dns_policy: DnsPolicy::Bound,
                dns_servers: vec!["127.0.0.1:53".parse().unwrap()],
                ..local
            },
            NetworkBindingConfig {
                mode: NetworkBindingMode::LocalAddress,
                enable_ipv4: false,
                ipv6_address: Some(std::net::Ipv6Addr::LOCALHOST),
                ..Default::default()
            },
        ];
        for policy in policies {
            let _requests = service(&mut manager);
            let stopped = manager
                .rtc
                .services
                .values()
                .next()
                .unwrap()
                .stop
                .subscribe();
            handle.reconfigure(policy).await.unwrap();
            publisher
                .activate(handle.try_lease().unwrap(), 41000)
                .unwrap();
            manager.apply_latest_network_activation();
            manager.rtc_reconcile();
            assert_eq!(manager.settings.network_binding, startup_settings);
            assert!(manager.rtc_network().is_none());
            assert!(!*manager.rtc.capable.borrow());
            assert!(*stopped.borrow());
            assert!(manager.rtc.services.is_empty());
            assert!(manager.rtc_announce("ws://tracker.invalid/announce", Some(Event::Started)));
            assert!(manager.rtc.services.is_empty());

            handle
                .reconfigure(NetworkBindingConfig::default())
                .await
                .unwrap();
            publisher
                .activate(handle.try_lease().unwrap(), 41000)
                .unwrap();
            manager.apply_latest_network_activation();
            manager.rtc_reconcile();
            assert!(manager.rtc_network().is_some());
            assert!(*manager.rtc.capable.borrow());
        }
        manager.rtc_shutdown().await;
        handle.shutdown().await.unwrap();
        supervisor.await.unwrap();
    }

    #[tokio::test]
    async fn invalidated_rtc_snapshot_cannot_start_transport_or_panic_announce() {
        use crate::networking::{NetworkActivationPublisher, NetworkSupervisor};
        let (handle, supervisor) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = NetworkActivationPublisher::channel();
        let (mut manager, _directory) = manager();
        manager.network_activation_rx = activation.subscribe();
        manager.network_activation = activation;
        let url = "ws://tracker.invalid/announce";

        for blocked in [false, true] {
            publisher
                .activate(handle.try_lease().unwrap(), 41000)
                .unwrap();
            let active = manager.rtc_network().expect("eligible snapshot");
            let _requests = service(&mut manager);
            if blocked {
                publisher.block("test network unavailable");
            } else {
                publisher.pending(None);
            }
            let mut started = false;
            assert!(active.scope().run(async { started = true }).await.is_err());
            assert!(!started);
            manager.state.trackers.insert(
                url.into(),
                TrackerState {
                    next_announce_time: manager.state.now,
                    leeching_interval: Some(Duration::from_secs(60)),
                    seeding_interval: None,
                    has_responded: false,
                },
            );
            manager.pending_started_announces.insert(url.into());
            assert!(manager.rtc_announce(url, Some(Event::Started)));
            assert!(manager.state.trackers[url].next_announce_time > manager.state.now);
            assert!(manager.rtc.services.is_empty());
            assert!(!manager.pending_started_announces.contains(url));
            assert!(manager.rtc.tasks.is_empty());
        }
        manager.rtc_shutdown().await;
        handle.shutdown().await.unwrap();
        supervisor.await.unwrap();
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
