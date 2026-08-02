// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::env;

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

pub(in crate::dht::service) type DhtCommandSender = mpsc::UnboundedSender<DhtCommand>;
pub(in crate::dht::service) type DhtCommandReceiver = mpsc::UnboundedReceiver<DhtCommand>;

pub(in crate::dht::service) fn send_dht_command(
    command_tx: &DhtCommandSender,
    command: DhtCommand,
) -> Result<(), ()> {
    command_tx.send(command).map_err(|_| ())
}

#[derive(Debug)]
pub(in crate::dht::service) enum DhtDemandSubscriptionInner {
    Service {
        command_tx: DhtCommandSender,
        info_hash: InfoHash,
        subscriber_id: u64,
    },
    #[cfg(test)]
    Recorder,
    Disabled,
}

#[derive(Debug)]
pub struct DhtDemandSubscription {
    pub(in crate::dht::service) receiver: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
    pub(in crate::dht::service) inner: DhtDemandSubscriptionInner,
}

impl DhtDemandSubscription {
    fn empty() -> Self {
        let (_tx, receiver) = mpsc::unbounded_channel();
        Self {
            receiver,
            inner: DhtDemandSubscriptionInner::Disabled,
        }
    }

    pub async fn recv(&mut self) -> Option<Vec<SocketAddr>> {
        self.receiver.recv().await
    }
}

impl Drop for DhtDemandSubscription {
    fn drop(&mut self) {
        if let DhtDemandSubscriptionInner::Service {
            command_tx,
            info_hash,
            subscriber_id,
        } = &self.inner
        {
            let _ = send_dht_command(
                command_tx,
                DhtCommand::UnregisterDemand {
                    info_hash: *info_hash,
                    subscriber_id: *subscriber_id,
                },
            );
        }
    }
}

#[cfg(test)]
type RecordedAnnounces = Arc<StdMutex<Vec<(Vec<u8>, Option<u16>)>>>;
#[cfg(test)]
type RecordedReconfigures = Arc<StdMutex<Vec<DhtServiceConfig>>>;
#[cfg(test)]
type RecordedPeerSlotUsages = Arc<StdMutex<Vec<(usize, usize)>>>;

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct TestDhtRecorder {
    announce_requests: RecordedAnnounces,
    reconfigure_requests: RecordedReconfigures,
    peer_slot_usages: RecordedPeerSlotUsages,
}

#[cfg(test)]
impl TestDhtRecorder {
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

#[derive(Debug)]
pub(in crate::dht::service) enum DhtCommand {
    Reconfigure(DhtServiceConfig),
    ReconfigureAndWait {
        config: DhtServiceConfig,
        completion_tx: oneshot::Sender<()>,
    },
    RegisterDemand {
        info_hash: InfoHash,
        demand: DhtDemandState,
        subscriber_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
        response_tx: oneshot::Sender<Option<u64>>,
    },
    UpdateDemand {
        info_hash: InfoHash,
        demand: DhtDemandState,
    },
    UpdateDemandMetrics {
        info_hash: InfoHash,
        metrics: DhtDemandMetrics,
    },
    UpdatePeerSlotUsage {
        total_peers: usize,
        max_connected_peers: usize,
    },
    UnregisterDemand {
        info_hash: InfoHash,
        subscriber_id: u64,
    },
    DemandPeers {
        info_hash: InfoHash,
        peers: Vec<SocketAddr>,
    },
    DemandLookupFinished {
        info_hash: InfoHash,
        slice_class: DemandSliceClass,
        total_peers: usize,
        unique_peers: usize,
    },
    StartGetPeers {
        info_hash: InfoHash,
        response_tx: oneshot::Sender<Result<StartedLookup, String>>,
    },
    StartGetPeersFamily {
        info_hash: InfoHash,
        family: AddressFamily,
        slice_class: DemandSliceClass,
        record_metrics: bool,
        merged_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
        lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
        first_batch_seen: Arc<AtomicBool>,
        accepting_families: Arc<AtomicBool>,
    },
    CancelLookups {
        lookup_ids: Vec<LookupId>,
    },
    ParkDemandLookups {
        info_hash: InfoHash,
        slice_class: DemandSliceClass,
        stop_reason: DemandSliceStopReason,
        total_peers: usize,
        unique_peers: HashSet<SocketAddr>,
        lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    },
    FinalizeDrainedDemandLookups {
        info_hash: InfoHash,
    },
    AnnouncePeer {
        info_hash: InfoHash,
        port: Option<u16>,
        response_tx: oneshot::Sender<bool>,
    },
}

#[derive(Debug)]
pub struct DhtService {
    handle: DhtHandle,
    status_rx: watch::Receiver<DhtStatus>,
    wave_telemetry_rx: watch::Receiver<DhtWaveTelemetry>,
    command_tx: DhtCommandSender,
    #[allow(dead_code)]
    task: Option<JoinHandle<()>>,
}

impl DhtService {
    pub async fn new(
        network_handle: NetworkHandle,
        config: DhtServiceConfig,
        shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<Self, String> {
        let local_node_id = configured_or_persisted_local_node_id();
        let initial = match build_runtime(&network_handle, &config, local_node_id).await {
            Ok(initial) => initial,
            Err(error) => BuiltRuntime {
                active_runtime: None,
                backend: DhtBackendKind::Disabled,
                warning: Some(format!("DHT startup failed: {error}")),
                bootstrap: literal_bootstrap_summary(&config.bootstrap_nodes),
            },
        };
        let initial_status = build_status(
            initial.active_runtime.as_ref(),
            initial.backend,
            config.preferred_backend,
            initial.warning.clone(),
            0,
            initial.bootstrap,
        );
        let initial_wave_telemetry = build_wave_telemetry(
            initial.active_runtime.as_ref(),
            0,
            DHT_DEMAND_POWER_BASE_SCALE_HALVES,
        );

        let (status_tx, status_rx) = watch::channel(initial_status);
        let (wave_telemetry_tx, wave_telemetry_rx) = watch::channel(initial_wave_telemetry);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let handle = DhtHandle {
            inner: DhtHandleInner::Service {
                command_tx: command_tx.clone(),
                status_rx: status_rx.clone(),
            },
        };
        let task = Some(tokio::spawn(run_service(
            network_handle,
            config,
            local_node_id,
            initial.active_runtime,
            initial.warning,
            status_tx,
            wave_telemetry_tx,
            command_tx.clone(),
            command_rx,
            shutdown_rx,
        )));

        Ok(Self {
            handle,
            status_rx,
            wave_telemetry_rx,
            command_tx,
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
        self.wave_telemetry_rx.borrow().clone()
    }

    pub fn current_warning(&self) -> Option<String> {
        self.status_rx.borrow().warning.clone()
    }

    pub fn reconfigure(&self, config: DhtServiceConfig) {
        let _ = send_dht_command(&self.command_tx, DhtCommand::Reconfigure(config));
    }

    pub async fn reconfigure_and_wait(&self, config: DhtServiceConfig) -> Result<(), String> {
        let (completion_tx, completion_rx) = oneshot::channel();
        send_dht_command(
            &self.command_tx,
            DhtCommand::ReconfigureAndWait {
                config,
                completion_tx,
            },
        )
        .map_err(|()| "DHT service is unavailable".to_string())?;
        completion_rx
            .await
            .map_err(|_| "DHT service stopped before reconfiguration completed".to_string())
    }

    pub fn update_peer_slot_usage(&self, total_peers: usize, max_connected_peers: usize) {
        let _ = send_dht_command(
            &self.command_tx,
            DhtCommand::UpdatePeerSlotUsage {
                total_peers,
                max_connected_peers,
            },
        );
    }
}

fn configured_or_persisted_local_node_id() -> NodeId {
    if let Some(configured) = env::var("SUPERSEEDR_DHT_NODE_ID_HEX")
        .ok()
        .and_then(|value| hex::decode(value).ok())
        .and_then(|bytes| NodeId::try_from(bytes.as_slice()).ok())
    {
        return configured;
    }

    if let Some(persistence) = persistence_config() {
        let manager = PersistenceManager::new(persistence);
        if let Ok(Some(snapshot)) = manager.load_snapshot(std::time::SystemTime::now()) {
            return snapshot.node_id;
        }
    }

    NodeId::from(random::<[u8; 20]>())
}

#[cfg(test)]
impl DhtService {
    pub(crate) fn from_test_recorder(recorder: TestDhtRecorder) -> Self {
        let handle = DhtHandle::from_test_recorder(recorder);
        let status_rx = handle.status_rx().clone();
        let (_wave_telemetry_tx, wave_telemetry_rx) = watch::channel(DhtWaveTelemetry::default());
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let recorder = match &handle.inner {
            DhtHandleInner::Recorder { recorder, .. } => recorder.clone(),
            _ => unreachable!("test recorder handle must use recorder inner"),
        };
        let task = Some(tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    DhtCommand::Reconfigure(config) => {
                        recorder
                            .reconfigure_requests
                            .lock()
                            .expect("test dht reconfigure recorder lock")
                            .push(config);
                    }
                    DhtCommand::ReconfigureAndWait {
                        config,
                        completion_tx,
                    } => {
                        recorder
                            .reconfigure_requests
                            .lock()
                            .expect("test dht reconfigure recorder lock")
                            .push(config);
                        let _ = completion_tx.send(());
                    }
                    DhtCommand::UpdatePeerSlotUsage {
                        total_peers,
                        max_connected_peers,
                    } => {
                        recorder
                            .peer_slot_usages
                            .lock()
                            .expect("test dht peer slot recorder lock")
                            .push((total_peers, max_connected_peers));
                    }
                    _ => {}
                }
            }
        }));
        Self {
            handle,
            status_rx,
            wave_telemetry_rx,
            command_tx,
            task,
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
            backend: config.preferred_backend,
            preferred_backend: Some(config.preferred_backend),
            enabled: !matches!(config.preferred_backend, DhtBackendKind::Disabled),
            exported_bootstrap_nodes: bootstrap.total,
            ipv4_bootstrap_nodes: bootstrap.ipv4,
            ipv6_bootstrap_nodes: bootstrap.ipv6,
            ..Default::default()
        },
    }
}

#[derive(Clone)]
pub struct DhtHandle {
    inner: DhtHandleInner,
}

#[derive(Clone)]
enum DhtHandleInner {
    Service {
        command_tx: DhtCommandSender,
        status_rx: watch::Receiver<DhtStatus>,
    },
    #[cfg(test)]
    Recorder {
        recorder: TestDhtRecorder,
        status_rx: watch::Receiver<DhtStatus>,
    },
    Disabled {
        status_rx: watch::Receiver<DhtStatus>,
    },
}

impl std::fmt::Debug for DhtHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = self.status_rx().borrow().clone();
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
            inner: DhtHandleInner::Disabled { status_rx },
        }
    }

    #[cfg(test)]
    fn from_test_recorder(recorder: TestDhtRecorder) -> Self {
        let (_status_tx, status_rx) = watch::channel(DhtStatus {
            generation: 0,
            warning: None,
            health: DhtHealthSnapshot {
                backend: DhtBackendKind::InternalPrototype,
                preferred_backend: Some(DhtBackendKind::InternalPrototype),
                enabled: true,
                ..Default::default()
            },
        });
        Self {
            inner: DhtHandleInner::Recorder {
                recorder,
                status_rx,
            },
        }
    }

    pub async fn status_snapshot(&self) -> DhtStatus {
        match &self.inner {
            DhtHandleInner::Service { status_rx, .. } => status_rx.borrow().clone(),
            #[cfg(test)]
            DhtHandleInner::Recorder { status_rx, .. } => status_rx.borrow().clone(),
            DhtHandleInner::Disabled { status_rx } => status_rx.borrow().clone(),
        }
    }

    pub fn spawn_lookup_task(
        &self,
        info_hash: Vec<u8>,
        initial_demand: DhtDemandState,
        initial_metrics: DhtDemandMetrics,
        dht_tx: Sender<Vec<SocketAddr>>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> Option<JoinHandle<()>> {
        let info_hash = InfoHash::from(<[u8; 20]>::try_from(info_hash).ok()?);
        match &self.inner {
            DhtHandleInner::Service { .. } => {
                let handle = self.clone();
                Some(tokio::spawn(async move {
                    let metrics_info_hash = info_hash.as_ref().to_vec();
                    let mut subscription = match handle
                        .register_demand(metrics_info_hash.clone(), initial_demand)
                        .await
                    {
                        Some(subscription) => subscription,
                        None => return,
                    };
                    handle.update_demand_metrics(metrics_info_hash, initial_metrics);

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            maybe_peers = subscription.recv() => {
                                let Some(peers) = maybe_peers else {
                                    break;
                                };
                                if dht_tx.send(peers).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }))
            }
            #[cfg(test)]
            DhtHandleInner::Recorder { .. } | DhtHandleInner::Disabled { .. } => {
                Some(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            _ = std::future::pending::<()>() => {}
                        }
                    }
                }))
            }
            #[cfg(not(test))]
            DhtHandleInner::Disabled { .. } => Some(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        _ = std::future::pending::<()>() => {}
                    }
                }
            })),
        }
    }

    pub async fn lookup_once(
        &self,
        info_hash: Vec<u8>,
        idle_timeout: Duration,
        overall_timeout: Duration,
    ) -> Option<DhtLookupRun> {
        let info_hash = InfoHash::from(<[u8; 20]>::try_from(info_hash).ok()?);
        match &self.inner {
            DhtHandleInner::Service { .. } => {
                let mut peers_rx = self.start_lookup_receiver(info_hash).await?;
                summarize_lookup_receiver(&mut peers_rx, idle_timeout, overall_timeout).await
            }
            #[cfg(test)]
            DhtHandleInner::Recorder { .. } | DhtHandleInner::Disabled { .. } => {
                Some(DhtLookupRun::default())
            }
            #[cfg(not(test))]
            DhtHandleInner::Disabled { .. } => Some(DhtLookupRun::default()),
        }
    }

    pub async fn announce_peer(&self, info_hash: Vec<u8>, port: Option<u16>) -> bool {
        let Ok(info_hash) = <[u8; 20]>::try_from(info_hash) else {
            return false;
        };
        match &self.inner {
            DhtHandleInner::Service { command_tx, .. } => {
                if command_tx.is_closed() {
                    return false;
                }

                let (response_tx, response_rx) = oneshot::channel();
                let command = DhtCommand::AnnouncePeer {
                    info_hash: InfoHash::from(info_hash),
                    port,
                    response_tx,
                };
                if send_dht_command(command_tx, command).is_err() {
                    return false;
                }
                response_rx.await.unwrap_or(false)
            }
            #[cfg(test)]
            DhtHandleInner::Recorder { recorder, .. } => {
                recorder
                    .announce_requests
                    .lock()
                    .expect("test dht recorder lock")
                    .push((info_hash.to_vec(), port));
                true
            }
            DhtHandleInner::Disabled { .. } => false,
        }
    }

    pub async fn register_demand(
        &self,
        info_hash: Vec<u8>,
        demand: DhtDemandState,
    ) -> Option<DhtDemandSubscription> {
        let Ok(info_hash) = <[u8; 20]>::try_from(info_hash) else {
            return None;
        };

        match &self.inner {
            DhtHandleInner::Service { command_tx, .. } => {
                let (subscriber_tx, receiver) = mpsc::unbounded_channel();
                let (response_tx, response_rx) = oneshot::channel();
                let command = DhtCommand::RegisterDemand {
                    info_hash: InfoHash::from(info_hash),
                    demand,
                    subscriber_tx,
                    response_tx,
                };
                if send_dht_command(command_tx, command).is_err() {
                    return None;
                }

                let subscriber_id = response_rx.await.ok().flatten()?;
                Some(DhtDemandSubscription {
                    receiver,
                    inner: DhtDemandSubscriptionInner::Service {
                        command_tx: command_tx.clone(),
                        info_hash: InfoHash::from(info_hash),
                        subscriber_id,
                    },
                })
            }
            #[cfg(test)]
            DhtHandleInner::Recorder { .. } => Some(DhtDemandSubscription {
                receiver: mpsc::unbounded_channel().1,
                inner: DhtDemandSubscriptionInner::Recorder,
            }),
            DhtHandleInner::Disabled { .. } => Some(DhtDemandSubscription::empty()),
        }
    }

    pub fn update_demand(&self, info_hash: Vec<u8>, demand: DhtDemandState) -> bool {
        let Ok(info_hash) = <[u8; 20]>::try_from(info_hash) else {
            return false;
        };

        match &self.inner {
            DhtHandleInner::Service { command_tx, .. } => send_dht_command(
                command_tx,
                DhtCommand::UpdateDemand {
                    info_hash: InfoHash::from(info_hash),
                    demand,
                },
            )
            .is_ok(),
            #[cfg(test)]
            DhtHandleInner::Recorder { .. } => true,
            DhtHandleInner::Disabled { .. } => true,
        }
    }

    pub fn update_demand_metrics(&self, info_hash: Vec<u8>, metrics: DhtDemandMetrics) -> bool {
        let Ok(info_hash) = <[u8; 20]>::try_from(info_hash) else {
            return false;
        };

        match &self.inner {
            DhtHandleInner::Service { command_tx, .. } => send_dht_command(
                command_tx,
                DhtCommand::UpdateDemandMetrics {
                    info_hash: InfoHash::from(info_hash),
                    metrics,
                },
            )
            .is_ok(),
            #[cfg(test)]
            DhtHandleInner::Recorder { .. } => true,
            DhtHandleInner::Disabled { .. } => true,
        }
    }

    async fn start_lookup_receiver(&self, info_hash: InfoHash) -> Option<ManagedLookupReceiver> {
        let status_rx = self.status_rx();
        match &self.inner {
            DhtHandleInner::Service { command_tx, .. } => {
                if command_tx.is_closed()
                    && matches!(status_rx.borrow().health.backend, DhtBackendKind::Disabled)
                {
                    return Some(ManagedLookupReceiver::empty());
                }

                let (response_tx, response_rx) = oneshot::channel();
                let command = DhtCommand::StartGetPeers {
                    info_hash,
                    response_tx,
                };
                if send_dht_command(command_tx, command).is_err() {
                    return if matches!(status_rx.borrow().health.backend, DhtBackendKind::Disabled)
                    {
                        Some(ManagedLookupReceiver::empty())
                    } else {
                        None
                    };
                }

                match response_rx.await.ok()? {
                    Ok(started) => Some(ManagedLookupReceiver::new(
                        started.receiver,
                        command_tx.clone(),
                        started.lookup_ids,
                    )),
                    Err(_) => Some(ManagedLookupReceiver::empty()),
                }
            }
            _ => Some(ManagedLookupReceiver::empty()),
        }
    }

    fn status_rx(&self) -> &watch::Receiver<DhtStatus> {
        match &self.inner {
            DhtHandleInner::Service { status_rx, .. } => status_rx,
            #[cfg(test)]
            DhtHandleInner::Recorder { status_rx, .. } => status_rx,
            DhtHandleInner::Disabled { status_rx } => status_rx,
        }
    }
}
