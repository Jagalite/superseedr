// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod service {
    #![allow(dead_code)]

    use crate::config::Settings;
    #[allow(unused_imports)]
    pub use crate::dht_model::{
        DhtBackendKind, DhtHealthSnapshot, DhtSizeEstimate, DhtStatus, DhtWaveTelemetry,
    };
    use crate::networking::NetworkActivationHandle;
    use std::net::SocketAddr;
    #[cfg(test)]
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tokio::sync::broadcast;
    use tokio::sync::mpsc::Sender;
    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    const DHT_LOOKUP_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DhtServiceConfig {
        pub port: u16,
        pub bootstrap_nodes: Vec<String>,
        pub preferred_backend: DhtBackendKind,
        #[cfg(test)]
        pub force_internal_failure: bool,
    }

    impl DhtServiceConfig {
        pub fn from_settings(settings: &Settings) -> Self {
            Self {
                port: settings.client_port,
                bootstrap_nodes: settings.bootstrap_nodes.clone(),
                preferred_backend: DhtBackendKind::Disabled,
                #[cfg(test)]
                force_internal_failure: false,
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct DhtLookupRun {
        pub batch_count: usize,
        pub total_peers: usize,
        pub unique_peers: usize,
        pub unique_ipv4_peers: usize,
        pub unique_ipv6_peers: usize,
        pub first_batch_ms: Option<u64>,
        pub first_ipv4_batch_ms: Option<u64>,
        pub first_ipv6_batch_ms: Option<u64>,
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct DhtDemandState {
        pub awaiting_metadata: bool,
        pub connected_peers: usize,
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct DhtDemandMetrics {
        pub paused: bool,
        pub accepting_new_peers: bool,
        pub complete: bool,
        pub total_pieces: u32,
        pub completed_pieces: u32,
        pub connected_peers: usize,
        pub interested_peers: usize,
        pub peers_interested_in_us: usize,
        pub unchoked_download_peers: usize,
        pub unchoked_upload_peers: usize,
        pub downloading_peers: usize,
        pub uploading_peers: usize,
        pub download_speed_bps: u64,
        pub upload_speed_bps: u64,
        pub bytes_downloaded_this_tick: u64,
        pub bytes_uploaded_this_tick: u64,
    }

    #[derive(Debug)]
    pub struct DhtService {
        handle: DhtHandle,
        status_rx: watch::Receiver<DhtStatus>,
        task: Option<JoinHandle<()>>,
    }

    impl DhtService {
        pub async fn new(
            _network_activation: NetworkActivationHandle,
            config: DhtServiceConfig,
            mut shutdown_rx: broadcast::Receiver<()>,
        ) -> Result<Self, String> {
            let initial_status = configured_status_from_config(&config);
            let (_status_tx, status_rx) = watch::channel(initial_status);
            let handle = DhtHandle {
                status_rx: status_rx.clone(),
                #[cfg(test)]
                recorder: None,
            };
            let task = Some(tokio::spawn(async move {
                let _ = shutdown_rx.recv().await;
            }));
            Ok(Self {
                handle,
                status_rx,
                task,
            })
        }

        pub fn handle(&self) -> DhtHandle {
            self.handle.clone()
        }

        pub fn subscribe_status(&self) -> watch::Receiver<DhtStatus> {
            self.status_rx.clone()
        }

        pub fn current_status(&self) -> DhtStatus {
            self.status_rx.borrow().clone()
        }

        pub fn current_wave_telemetry(&self) -> DhtWaveTelemetry {
            DhtWaveTelemetry::default()
        }

        pub fn current_warning(&self) -> Option<String> {
            self.status_rx.borrow().warning.clone()
        }

        pub fn reconfigure(&self, config: DhtServiceConfig) {
            #[cfg(test)]
            if let Some(recorder) = &self.handle.recorder {
                recorder
                    .reconfigure_requests
                    .lock()
                    .expect("test dht reconfigure recorder lock")
                    .push(config);
            }
            #[cfg(not(test))]
            let _ = config;
        }

        pub async fn reconfigure_and_wait(&self, config: DhtServiceConfig) -> Result<(), String> {
            self.reconfigure(config);
            #[cfg(test)]
            if let Some(gate) = self
                .handle
                .recorder
                .as_ref()
                .and_then(|recorder| recorder.reconfigure_completion_gate.as_ref())
            {
                gate.notified().await;
            }
            Ok(())
        }

        pub fn update_peer_slot_usage(&self, total_peers: usize, max_connected_peers: usize) {
            #[cfg(test)]
            if let Some(recorder) = &self.handle.recorder {
                recorder
                    .peer_slot_usages
                    .lock()
                    .expect("test dht peer slot recorder lock")
                    .push((total_peers, max_connected_peers));
            }
            #[cfg(not(test))]
            let _ = (total_peers, max_connected_peers);
        }
    }

    #[cfg(test)]
    impl DhtService {
        pub(crate) fn from_test_recorder(recorder: TestDhtRecorder) -> Self {
            let handle = DhtHandle::from_test_recorder(recorder);
            let status_rx = handle.status_rx().clone();
            Self {
                handle,
                status_rx,
                task: None,
            }
        }
    }

    pub fn configured_status_from_settings(settings: &Settings) -> DhtStatus {
        configured_status_from_config(&DhtServiceConfig::from_settings(settings))
    }

    fn configured_status_from_config(config: &DhtServiceConfig) -> DhtStatus {
        let bootstrap = literal_bootstrap_summary(&config.bootstrap_nodes);
        DhtStatus {
            generation: 0,
            warning: None,
            health: DhtHealthSnapshot {
                backend: DhtBackendKind::Disabled,
                preferred_backend: Some(DhtBackendKind::Disabled),
                enabled: false,
                exported_bootstrap_nodes: bootstrap.total,
                ipv4_bootstrap_nodes: bootstrap.ipv4,
                ipv6_bootstrap_nodes: bootstrap.ipv6,
                ..Default::default()
            },
        }
    }

    #[derive(Debug, Clone, Copy, Default)]
    struct BootstrapSummary {
        total: usize,
        ipv4: usize,
        ipv6: usize,
    }

    fn literal_bootstrap_summary(bootstrap_nodes: &[String]) -> BootstrapSummary {
        let mut summary = BootstrapSummary {
            total: bootstrap_nodes.len(),
            ..Default::default()
        };
        for value in bootstrap_nodes {
            if let Ok(addr) = value.parse::<SocketAddr>() {
                if addr.is_ipv4() {
                    summary.ipv4 += 1;
                } else {
                    summary.ipv6 += 1;
                }
            }
        }
        summary
    }

    #[cfg(test)]
    type AnnounceRequests = Arc<StdMutex<Vec<(Vec<u8>, Option<u16>)>>>;

    #[cfg(test)]
    type ReconfigureRequests = Arc<StdMutex<Vec<DhtServiceConfig>>>;

    #[cfg(test)]
    type PeerSlotUsages = Arc<StdMutex<Vec<(usize, usize)>>>;

    #[cfg(test)]
    #[derive(Debug, Clone, Default)]
    pub(crate) struct TestDhtRecorder {
        announce_requests: AnnounceRequests,
        reconfigure_requests: ReconfigureRequests,
        peer_slot_usages: PeerSlotUsages,
        reconfigure_completion_gate: Option<Arc<tokio::sync::Notify>>,
    }

    #[cfg(test)]
    impl TestDhtRecorder {
        pub(crate) fn with_blocked_reconfigure() -> Self {
            Self {
                reconfigure_completion_gate: Some(Arc::new(tokio::sync::Notify::new())),
                ..Self::default()
            }
        }

        pub(crate) fn release_reconfigure(&self) {
            if let Some(gate) = &self.reconfigure_completion_gate {
                gate.notify_one();
            }
        }

        pub(crate) fn recorded_announces(&self) -> Vec<(Vec<u8>, Option<u16>)> {
            self.announce_requests
                .lock()
                .expect("test dht recorder lock")
                .clone()
        }

        pub(crate) fn recorded_reconfigures(&self) -> Vec<DhtServiceConfig> {
            self.reconfigure_requests
                .lock()
                .expect("test dht reconfigure recorder lock")
                .clone()
        }

        pub(crate) fn recorded_peer_slot_usages(&self) -> Vec<(usize, usize)> {
            self.peer_slot_usages
                .lock()
                .expect("test dht peer slot recorder lock")
                .clone()
        }
    }

    #[derive(Clone)]
    pub struct DhtHandle {
        status_rx: watch::Receiver<DhtStatus>,
        #[cfg(test)]
        recorder: Option<TestDhtRecorder>,
    }

    impl std::fmt::Debug for DhtHandle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let status = self.status_rx.borrow().clone();
            f.debug_struct("DhtHandle")
                .field("generation", &status.generation)
                .field("backend", &status.health.backend)
                .finish()
        }
    }

    impl Default for DhtHandle {
        fn default() -> Self {
            Self::disabled()
        }
    }

    impl DhtHandle {
        pub fn disabled() -> Self {
            let (_status_tx, status_rx) = watch::channel(DhtStatus {
                generation: 0,
                warning: None,
                health: DhtHealthSnapshot {
                    backend: DhtBackendKind::Disabled,
                    preferred_backend: Some(DhtBackendKind::Disabled),
                    enabled: false,
                    ..Default::default()
                },
            });
            Self {
                status_rx,
                #[cfg(test)]
                recorder: None,
            }
        }

        #[cfg(test)]
        fn from_test_recorder(recorder: TestDhtRecorder) -> Self {
            let (_status_tx, status_rx) = watch::channel(DhtStatus {
                generation: 0,
                warning: None,
                health: DhtHealthSnapshot {
                    backend: DhtBackendKind::Disabled,
                    preferred_backend: Some(DhtBackendKind::Disabled),
                    enabled: false,
                    ..Default::default()
                },
            });
            Self {
                status_rx,
                recorder: Some(recorder),
            }
        }

        pub async fn status_snapshot(&self) -> DhtStatus {
            self.status_rx.borrow().clone()
        }

        pub fn spawn_lookup_task(
            &self,
            _info_hash: Vec<u8>,
            _initial_demand: DhtDemandState,
            _initial_metrics: DhtDemandMetrics,
            _dht_tx: Sender<Vec<SocketAddr>>,
            mut shutdown_rx: broadcast::Receiver<()>,
        ) -> Option<JoinHandle<()>> {
            Some(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        _ = tokio::time::sleep(DHT_LOOKUP_REFRESH_INTERVAL) => {}
                    }
                }
            }))
        }

        pub fn update_demand(&self, _info_hash: Vec<u8>, _demand: DhtDemandState) -> bool {
            true
        }

        pub fn update_demand_metrics(
            &self,
            _info_hash: Vec<u8>,
            _metrics: DhtDemandMetrics,
        ) -> bool {
            true
        }

        pub async fn lookup_once(
            &self,
            _info_hash: Vec<u8>,
            _idle_timeout: Duration,
            _overall_timeout: Duration,
        ) -> Option<DhtLookupRun> {
            Some(DhtLookupRun::default())
        }

        pub async fn announce_peer(&self, info_hash: Vec<u8>, port: Option<u16>) -> bool {
            #[cfg(test)]
            if let Some(recorder) = &self.recorder {
                recorder
                    .announce_requests
                    .lock()
                    .expect("test dht recorder lock")
                    .push((info_hash, port));
                return true;
            }

            let _ = (info_hash, port);
            false
        }

        fn status_rx(&self) -> &watch::Receiver<DhtStatus> {
            &self.status_rx
        }
    }
}
