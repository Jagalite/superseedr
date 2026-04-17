// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::persist::{PersistenceConfig, PersistenceManager};
use super::types::{AddressFamily, InfoHash, LookupId, NodeId};
use super::{Runtime, RuntimeConfig};
use crate::config::{self, Settings};
use rand::random;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
#[cfg(feature = "dht")]
use mainline::{async_dht::AsyncDht, Id};
use tokio::net::lookup_host;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
#[cfg(feature = "dht")]
use tokio_stream::StreamExt;

const DHT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
const DHT_LOOKUP_REFRESH_INTERVAL: Duration = DHT_MAINTENANCE_INTERVAL;
const DHT_HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const DHT_PERSISTENCE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const DHT_STARTUP_BOOTSTRAP_DELAY: Duration = Duration::from_secs(5);
const DHT_IPV6_HEDGE_DELAY: Duration = Duration::from_millis(750);
const DHT_LOOKUP_BOOTSTRAP_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DhtBackendKind {
    #[default]
    Disabled,
    Mainline,
    InternalPrototype,
}

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
            preferred_backend: std::env::var("SUPERSEEDR_DHT_BACKEND")
                .ok()
                .as_deref()
                .and_then(DhtBackendKind::from_override)
                .unwrap_or(DhtBackendKind::InternalPrototype),
            #[cfg(test)]
            force_internal_failure: false,
        }
    }
}

impl DhtBackendKind {
    fn from_override(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" => Some(Self::Disabled),
            "mainline" | "compat" => Some(Self::Mainline),
            "internal" | "internal-prototype" | "builtin" => Some(Self::InternalPrototype),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DhtHealthSnapshot {
    pub backend: DhtBackendKind,
    pub preferred_backend: Option<DhtBackendKind>,
    pub recovery_pending: bool,
    pub enabled: bool,
    pub local_addr: Option<SocketAddr>,
    pub ipv4_local_addr: Option<SocketAddr>,
    pub ipv6_local_addr: Option<SocketAddr>,
    pub bound_family_count: usize,
    pub cached_ipv4_routes: usize,
    pub cached_ipv6_routes: usize,
    pub active_ipv4_routes: usize,
    pub active_ipv6_routes: usize,
    pub cached_ipv4_announce_tokens: usize,
    pub cached_ipv6_announce_tokens: usize,
    pub cached_lookup_results: usize,
    pub inflight_lookups: usize,
    pub inflight_ipv4_queries: usize,
    pub inflight_ipv6_queries: usize,
    pub public_addr: Option<SocketAddr>,
    pub firewalled: Option<bool>,
    pub server_mode: Option<bool>,
    pub exported_bootstrap_nodes: usize,
    pub dht_size_estimate: Option<DhtSizeEstimate>,
    pub ipv4_bootstrap_nodes: usize,
    pub ipv6_bootstrap_nodes: usize,
    pub responsive_ipv4_bootstrap_nodes: usize,
    pub responsive_ipv6_bootstrap_nodes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DhtSizeEstimate {
    pub node_count: usize,
    pub std_dev: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DhtStatus {
    pub generation: u64,
    pub warning: Option<String>,
    pub health: DhtHealthSnapshot,
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

#[derive(Debug)]
struct StartedLookup {
    lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    receiver: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
}

struct LookupCancelGuard {
    command_tx: mpsc::UnboundedSender<DhtCommand>,
    lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
}

impl Drop for LookupCancelGuard {
    fn drop(&mut self) {
        let mut lookup_ids = self
            .lookup_ids
            .lock()
            .expect("managed dht lookup ids lock");
        if lookup_ids.is_empty() {
            return;
        }
        let _ = self.command_tx.send(DhtCommand::CancelLookups {
            lookup_ids: std::mem::take(&mut *lookup_ids),
        });
    }
}

struct ManagedLookupReceiver {
    receiver: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
    cancel_guard: Option<LookupCancelGuard>,
}

impl ManagedLookupReceiver {
    fn new(
        receiver: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
        command_tx: mpsc::UnboundedSender<DhtCommand>,
        lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    ) -> Self {
        let has_lookup_ids = !lookup_ids
            .lock()
            .expect("managed dht lookup ids lock")
            .is_empty();
        let cancel_guard = has_lookup_ids.then_some(LookupCancelGuard {
            command_tx,
            lookup_ids,
        });
        Self {
            receiver,
            cancel_guard,
        }
    }

    fn empty() -> Self {
        let (_tx, receiver) = mpsc::unbounded_channel();
        Self {
            receiver,
            cancel_guard: None,
        }
    }

    async fn recv(&mut self) -> Option<Vec<SocketAddr>> {
        self.receiver.recv().await
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct TestDhtRecorder {
    announce_requests: Arc<StdMutex<Vec<(Vec<u8>, Option<u16>)>>>,
}

#[cfg(test)]
impl TestDhtRecorder {
    pub(crate) fn recorded_announces(&self) -> Vec<(Vec<u8>, Option<u16>)> {
        self.announce_requests
            .lock()
            .expect("test dht recorder lock")
            .clone()
    }
}

#[derive(Debug)]
enum DhtCommand {
    Reconfigure(DhtServiceConfig),
    StartGetPeers {
        info_hash: InfoHash,
        response_tx: oneshot::Sender<Result<StartedLookup, String>>,
    },
    StartGetPeersFamily {
        info_hash: InfoHash,
        family: AddressFamily,
        merged_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
        lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
        first_batch_seen: Arc<AtomicBool>,
    },
    CancelLookups {
        lookup_ids: Vec<LookupId>,
    },
    AnnouncePeer {
        info_hash: InfoHash,
        port: Option<u16>,
        response_tx: oneshot::Sender<bool>,
    },
}

#[derive(Debug)]
enum LoopEvent {
    Shutdown,
    Command(DhtCommand),
    MaintenanceTick,
    HealthTick,
    RuntimeStep(Result<bool, String>),
    CommandClosed,
}

#[derive(Debug, Clone, Copy, Default)]
struct BootstrapSummary {
    total: usize,
    ipv4: usize,
    ipv6: usize,
}

#[derive(Debug)]
struct ActiveRuntime {
    runtime: Runtime,
    backend: DhtBackendKind,
    bootstrap: BootstrapSummary,
    startup_bootstrap_due: Option<Instant>,
}

#[derive(Debug)]
struct BuiltRuntime {
    active_runtime: Option<ActiveRuntime>,
    backend: DhtBackendKind,
    warning: Option<String>,
    bootstrap: BootstrapSummary,
}

#[derive(Debug)]
pub struct DhtService {
    handle: DhtHandle,
    status_rx: watch::Receiver<DhtStatus>,
    command_tx: mpsc::UnboundedSender<DhtCommand>,
    #[allow(dead_code)]
    task: Option<JoinHandle<()>>,
}

impl DhtService {
    pub async fn new(
        config: DhtServiceConfig,
        shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<Self, String> {
        let local_node_id = configured_or_persisted_local_node_id();
        let initial = build_runtime(&config, local_node_id).await?;
        let initial_status = build_status(
            initial.active_runtime.as_ref(),
            initial.backend,
            config.preferred_backend,
            initial.warning.clone(),
            0,
            initial.bootstrap,
        );

        let (status_tx, status_rx) = watch::channel(initial_status);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let handle = DhtHandle {
            inner: DhtHandleInner::Service {
                command_tx: command_tx.clone(),
                status_rx: status_rx.clone(),
            },
        };
        let task = Some(tokio::spawn(run_service(
            config,
            local_node_id,
            initial.active_runtime,
            initial.warning,
            status_tx,
            command_tx.clone(),
            command_rx,
            shutdown_rx,
        )));

        Ok(Self {
            handle,
            status_rx,
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

    pub fn current_warning(&self) -> Option<String> {
        self.status_rx.borrow().warning.clone()
    }

    pub fn reconfigure(&self, config: DhtServiceConfig) {
        let _ = self.command_tx.send(DhtCommand::Reconfigure(config));
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
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        Self {
            handle,
            status_rx,
            command_tx,
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
        command_tx: mpsc::UnboundedSender<DhtCommand>,
        status_rx: watch::Receiver<DhtStatus>,
    },
    #[cfg(feature = "dht")]
    Mainline {
        inner: AsyncDht,
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
    #[cfg(feature = "dht")]
    #[allow(dead_code)]
    pub fn from_async(inner: AsyncDht) -> Self {
        let (_status_tx, status_rx) = watch::channel(DhtStatus {
            generation: 0,
            warning: None,
            health: DhtHealthSnapshot {
                backend: DhtBackendKind::Mainline,
                preferred_backend: Some(DhtBackendKind::Mainline),
                enabled: true,
                bound_family_count: 1,
                ..Default::default()
            },
        });

        Self {
            inner: DhtHandleInner::Mainline { inner, status_rx },
        }
    }

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
            inner: DhtHandleInner::Recorder { recorder, status_rx },
        }
    }

    pub async fn status_snapshot(&self) -> DhtStatus {
        match &self.inner {
            DhtHandleInner::Service { status_rx, .. } => status_rx.borrow().clone(),
            #[cfg(feature = "dht")]
            DhtHandleInner::Mainline { inner, status_rx } => {
                let mut status = status_rx.borrow().clone();
                let info = inner.info().await;
                let (node_count, std_dev) = info.dht_size_estimate();
                status.health = DhtHealthSnapshot {
                    backend: DhtBackendKind::Mainline,
                    preferred_backend: Some(DhtBackendKind::Mainline),
                    enabled: true,
                    local_addr: Some(SocketAddr::V4(info.local_addr())),
                    ipv4_local_addr: Some(SocketAddr::V4(info.local_addr())),
                    ipv6_local_addr: None,
                    bound_family_count: 1,
                    public_addr: info.public_address().map(SocketAddr::V4),
                    firewalled: Some(info.firewalled()),
                    server_mode: Some(info.server_mode()),
                    dht_size_estimate: Some(DhtSizeEstimate {
                        node_count,
                        std_dev: Some(std_dev),
                    }),
                    ..status.health
                };
                status
            }
            #[cfg(test)]
            DhtHandleInner::Recorder { status_rx, .. } => status_rx.borrow().clone(),
            DhtHandleInner::Disabled { status_rx } => status_rx.borrow().clone(),
        }
    }

    pub fn spawn_lookup_task(
        &self,
        info_hash: Vec<u8>,
        dht_tx: Sender<Vec<SocketAddr>>,
        mut shutdown_rx: broadcast::Receiver<()>,
        mut dht_trigger_rx: watch::Receiver<()>,
    ) -> Option<JoinHandle<()>> {
        let info_hash = InfoHash::from(<[u8; 20]>::try_from(info_hash).ok()?);
        match &self.inner {
            DhtHandleInner::Service { .. } => {
                let handle = self.clone();
                let mut status_rx = self.status_rx().clone();
                Some(tokio::spawn(async move {
                    loop {
                        let mut peers_rx = match handle.start_lookup_receiver(info_hash).await {
                            Some(peers_rx) => peers_rx,
                            None => break,
                        };

                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            _ = async {
                                while let Some(peers) = peers_rx.recv().await {
                                    if dht_tx.send(peers).await.is_err() {
                                        return;
                                    }
                                }
                            } => {}
                        }

                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            changed = status_rx.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                            }
                            changed = dht_trigger_rx.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                            }
                            _ = tokio::time::sleep(DHT_LOOKUP_REFRESH_INTERVAL) => {}
                        }
                    }
                }))
            }
            #[cfg(feature = "dht")]
            DhtHandleInner::Mainline { inner, .. } => {
                let inner = inner.clone();
                let Ok(info_hash_id) = Id::from_bytes(info_hash.into_bytes()) else {
                    return None;
                };
                Some(tokio::spawn(async move {
                    loop {
                        let mut peers_stream = inner.get_peers(info_hash_id).map(|peers| {
                            peers
                                .into_iter()
                                .map(SocketAddr::V4)
                                .collect::<Vec<SocketAddr>>()
                        });

                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            _ = async {
                                while let Some(peers) = peers_stream.next().await {
                                    if dht_tx.send(peers).await.is_err() {
                                        return;
                                    }
                                }
                            } => {}
                        }

                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            changed = dht_trigger_rx.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                            }
                            _ = tokio::time::sleep(DHT_LOOKUP_REFRESH_INTERVAL) => {}
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
                            changed = dht_trigger_rx.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                            }
                            _ = tokio::time::sleep(DHT_LOOKUP_REFRESH_INTERVAL) => {}
                        }
                    }
                }))
            }
            #[cfg(not(test))]
            DhtHandleInner::Disabled { .. } => {
                Some(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            changed = dht_trigger_rx.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                            }
                            _ = tokio::time::sleep(DHT_LOOKUP_REFRESH_INTERVAL) => {}
                        }
                    }
                }))
            }
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
            #[cfg(feature = "dht")]
            DhtHandleInner::Mainline { inner, .. } => {
                let Ok(info_hash_id) = Id::from_bytes(info_hash.into_bytes()) else {
                    return None;
                };
                let mut peers_stream = inner.get_peers(info_hash_id).map(|peers| {
                    peers
                        .into_iter()
                        .map(SocketAddr::V4)
                        .collect::<Vec<SocketAddr>>()
                });
                summarize_lookup_stream(&mut peers_stream, idle_timeout, overall_timeout).await
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
                if command_tx.send(command).is_err() {
                    return false;
                }
                response_rx.await.unwrap_or(false)
            }
            #[cfg(feature = "dht")]
            DhtHandleInner::Mainline { inner, .. } => {
                let Ok(info_hash_id) = Id::from_bytes(info_hash) else {
                    return false;
                };
                inner.clone().announce_peer(info_hash_id, port).await.is_ok()
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

    async fn start_lookup_receiver(
        &self,
        info_hash: InfoHash,
    ) -> Option<ManagedLookupReceiver> {
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
                if command_tx.send(command).is_err() {
                    return if matches!(status_rx.borrow().health.backend, DhtBackendKind::Disabled) {
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
            #[cfg(feature = "dht")]
            DhtHandleInner::Mainline { status_rx, .. } => status_rx,
            #[cfg(test)]
            DhtHandleInner::Recorder { status_rx, .. } => status_rx,
            DhtHandleInner::Disabled { status_rx } => status_rx,
        }
    }
}

async fn run_service(
    mut config: DhtServiceConfig,
    local_node_id: NodeId,
    mut active_runtime: Option<ActiveRuntime>,
    mut warning: Option<String>,
    status_tx: watch::Sender<DhtStatus>,
    command_tx: mpsc::UnboundedSender<DhtCommand>,
    mut command_rx: mpsc::UnboundedReceiver<DhtCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut maintenance_interval = tokio::time::interval(DHT_MAINTENANCE_INTERVAL);
    let mut health_interval = tokio::time::interval(DHT_HEALTH_REFRESH_INTERVAL);
    let mut generation = status_tx.borrow().generation;

    loop {
        if let Some(active) = active_runtime.as_mut() {
            if let Some(startup_due) = active.startup_bootstrap_due {
                if Instant::now() >= startup_due && active.runtime.active_user_lookup_count() == 0 {
                    match active.runtime.bootstrap_startup().await {
                        Ok(()) => active.startup_bootstrap_due = None,
                        Err(error) => {
                            warning = Some(format!("DHT startup bootstrap failed: {error}"));
                            active.startup_bootstrap_due =
                                Some(Instant::now() + DHT_STARTUP_BOOTSTRAP_DELAY);
                        }
                    }
                }
            }
        }

        let event = if let Some(active) = active_runtime.as_mut() {
            tokio::select! {
                _ = shutdown_rx.recv() => LoopEvent::Shutdown,
                maybe_command = command_rx.recv() => maybe_command.map_or(LoopEvent::CommandClosed, LoopEvent::Command),
                _ = maintenance_interval.tick() => LoopEvent::MaintenanceTick,
                _ = health_interval.tick() => LoopEvent::HealthTick,
                step_result = active.runtime.step() => LoopEvent::RuntimeStep(step_result.map_err(|error| error.to_string())),
            }
        } else {
            tokio::select! {
                _ = shutdown_rx.recv() => LoopEvent::Shutdown,
                maybe_command = command_rx.recv() => maybe_command.map_or(LoopEvent::CommandClosed, LoopEvent::Command),
                _ = maintenance_interval.tick() => LoopEvent::MaintenanceTick,
                _ = health_interval.tick() => LoopEvent::HealthTick,
            }
        };

        match event {
            LoopEvent::Shutdown | LoopEvent::CommandClosed => {
                if let Some(active) = active_runtime.as_ref() {
                    let _ = active.runtime.save_state().await;
                }
                break;
            }
            LoopEvent::Command(DhtCommand::Reconfigure(new_config)) => {
                match build_runtime(&new_config, local_node_id).await {
                    Ok(built) => {
                        if let Some(previous) = active_runtime.as_ref() {
                            let _ = previous.runtime.save_state().await;
                        }
                        config = new_config;
                        generation = generation.saturating_add(1);
                        warning = built.warning;
                        active_runtime = built.active_runtime;
                    }
                    Err(error) => {
                        warning = Some(error);
                    }
                }
                publish_status(
                    &status_tx,
                    active_runtime.as_ref(),
                    warning.clone(),
                    generation,
                    config.preferred_backend,
                );
            }
            LoopEvent::Command(DhtCommand::StartGetPeers {
                info_hash,
                response_tx,
            }) => {
                let result =
                    start_get_peers_lookup(active_runtime.as_mut(), &command_tx, info_hash).await;
                let _ = response_tx.send(result);
            }
            LoopEvent::Command(DhtCommand::StartGetPeersFamily {
                info_hash,
                family,
                merged_tx,
                lookup_ids,
                first_batch_seen,
            }) => {
                let _ = attach_lookup_family(
                    active_runtime.as_mut(),
                    info_hash,
                    family,
                    merged_tx,
                    lookup_ids,
                    first_batch_seen,
                )
                .await;
            }
            LoopEvent::Command(DhtCommand::CancelLookups { lookup_ids }) => {
                if let Some(active_runtime) = active_runtime.as_mut() {
                    for lookup_id in lookup_ids {
                        active_runtime.runtime.cancel_lookup(lookup_id);
                    }
                }
            }
            LoopEvent::Command(DhtCommand::AnnouncePeer {
                info_hash,
                port,
                response_tx,
            }) => {
                let success = announce_peer(active_runtime.as_mut(), info_hash, port).await;
                let _ = response_tx.send(success);
            }
            LoopEvent::MaintenanceTick => {
                if let Some(active) = active_runtime.as_mut() {
                    if active.runtime.active_user_lookup_count() > 0 {
                        continue;
                    }
                    if let Err(error) = active.runtime.run_maintenance().await {
                        warning = Some(format!("DHT maintenance failed: {error}"));
                        publish_status(
                            &status_tx,
                            active_runtime.as_ref(),
                            warning.clone(),
                            generation,
                            config.preferred_backend,
                        );
                    }
                }
            }
            LoopEvent::HealthTick => {
                publish_status(
                    &status_tx,
                    active_runtime.as_ref(),
                    warning.clone(),
                    generation,
                    config.preferred_backend,
                );
                if let Some(active) = active_runtime.as_ref() {
                    let _ = active.runtime.save_state().await;
                }
            }
            LoopEvent::RuntimeStep(Ok(_)) => {}
            LoopEvent::RuntimeStep(Err(error)) => {
                warning = Some(format!("DHT runtime step failed: {error}"));
                publish_status(
                    &status_tx,
                    active_runtime.as_ref(),
                    warning.clone(),
                    generation,
                    config.preferred_backend,
                );
            }
        }
    }
}

async fn start_get_peers_lookup(
    active_runtime: Option<&mut ActiveRuntime>,
    command_tx: &mpsc::UnboundedSender<DhtCommand>,
    info_hash: InfoHash,
) -> Result<StartedLookup, String> {
    let Some(active_runtime) = active_runtime else {
        return Ok(StartedLookup {
            lookup_ids: Arc::new(StdMutex::new(Vec::new())),
            receiver: ManagedLookupReceiver::empty().receiver,
        });
    };

    let lookup_ids = Arc::new(StdMutex::new(Vec::new()));
    let (merged_tx, merged_rx) = mpsc::unbounded_channel();
    let first_batch_seen = Arc::new(AtomicBool::new(false));

    let primary_family = if active_runtime.runtime.family_bound(AddressFamily::Ipv4) {
        Some(AddressFamily::Ipv4)
    } else if active_runtime.runtime.family_bound(AddressFamily::Ipv6) {
        Some(AddressFamily::Ipv6)
    } else {
        None
    };

    if let Some(family) = primary_family {
        ensure_lookup_routes(active_runtime, family).await?;
        active_runtime.runtime.cancel_maintenance_lookups();
        attach_lookup_family(
            Some(active_runtime),
            info_hash,
            family,
            merged_tx.clone(),
            lookup_ids.clone(),
            first_batch_seen.clone(),
        )
        .await?;
    }

    if primary_family == Some(AddressFamily::Ipv4)
        && active_runtime.runtime.family_bound(AddressFamily::Ipv6)
    {
        let command_tx = command_tx.clone();
        let merged_tx = merged_tx.clone();
        let lookup_ids = lookup_ids.clone();
        let first_batch_seen = first_batch_seen.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DHT_IPV6_HEDGE_DELAY).await;
            if merged_tx.is_closed() {
                return;
            }
            let _ = command_tx.send(DhtCommand::StartGetPeersFamily {
                info_hash,
                family: AddressFamily::Ipv6,
                merged_tx,
                lookup_ids,
                first_batch_seen,
            });
        });
    }

    if lookup_ids
        .lock()
        .expect("managed dht lookup ids lock")
        .is_empty()
    {
        return Ok(StartedLookup {
            lookup_ids: Arc::new(StdMutex::new(Vec::new())),
            receiver: ManagedLookupReceiver::empty().receiver,
        });
    }

    drop(merged_tx);

    Ok(StartedLookup {
        lookup_ids,
        receiver: merged_rx,
    })
}

async fn ensure_lookup_routes(
    active_runtime: &mut ActiveRuntime,
    family: AddressFamily,
) -> Result<(), String> {
    if active_runtime.runtime.active_route_count(family) > 0 {
        return Ok(());
    }

    active_runtime
        .runtime
        .bootstrap_startup()
        .await
        .map_err(|error| error.to_string())?;
    active_runtime.startup_bootstrap_due = None;

    let deadline = Instant::now() + DHT_LOOKUP_BOOTSTRAP_WAIT;
    while Instant::now() < deadline && active_runtime.runtime.active_route_count(family) == 0 {
        match tokio::time::timeout(Duration::from_millis(200), active_runtime.runtime.step()).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => break,
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => {}
        }
    }

    Ok(())
}

async fn attach_lookup_family(
    active_runtime: Option<&mut ActiveRuntime>,
    info_hash: InfoHash,
    family: AddressFamily,
    merged_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
    lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    first_batch_seen: Arc<AtomicBool>,
) -> Result<(), String> {
    let Some(active_runtime) = active_runtime else {
        return Ok(());
    };
    if !active_runtime.runtime.family_bound(family) {
        return Ok(());
    }

    let (lookup_id, mut family_rx) = active_runtime
        .runtime
        .start_get_peers(family, info_hash)
        .await
        .map_err(|error| error.to_string())?;
    lookup_ids
        .lock()
        .expect("managed dht lookup ids lock")
        .push(lookup_id);

    tokio::spawn(async move {
        while let Some(batch) = family_rx.recv().await {
            first_batch_seen.store(true, Ordering::Release);
            if merged_tx.send(batch).is_err() {
                break;
            }
        }
    });

    Ok(())
}

async fn announce_peer(
    active_runtime: Option<&mut ActiveRuntime>,
    info_hash: InfoHash,
    port: Option<u16>,
) -> bool {
    let Some(active_runtime) = active_runtime else {
        return false;
    };

    let mut announced = false;
    for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
        if !active_runtime.runtime.family_bound(family) {
            continue;
        }
        match active_runtime.runtime.announce_peer(family, info_hash, port).await {
            Ok(success) => announced |= success,
            Err(_) => {}
        }
    }

    announced
}

async fn build_runtime(
    config: &DhtServiceConfig,
    local_node_id: NodeId,
) -> Result<BuiltRuntime, String> {
    if let Some(error) = forced_internal_backend_error(config) {
        return Err(error);
    }

    let disable_ipv4 = std::env::var_os("SUPERSEEDR_DHT_DISABLE_IPV4").is_some();
    let disable_ipv6 = std::env::var_os("SUPERSEEDR_DHT_DISABLE_IPV6").is_some();

    if matches!(config.preferred_backend, DhtBackendKind::Disabled) {
        let bootstrap = literal_bootstrap_summary(&config.bootstrap_nodes);
        return Ok(BuiltRuntime {
            active_runtime: None,
            backend: DhtBackendKind::Disabled,
            warning: None,
            bootstrap,
        });
    }

    let bootstrap_nodes = resolve_bootstrap_nodes(&config.bootstrap_nodes).await;
    let bootstrap = BootstrapSummary {
        total: bootstrap_nodes.len(),
        ipv4: bootstrap_nodes.iter().filter(|addr| addr.is_ipv4()).count(),
        ipv6: bootstrap_nodes.iter().filter(|addr| addr.is_ipv6()).count(),
    };
    let warning = match config.preferred_backend {
        DhtBackendKind::Mainline => Some(
            "mainline backend is not wired into src/dht/service.rs yet; using internal runtime"
                .to_string(),
        ),
        _ => None,
    };
    let runtime = Runtime::bind(RuntimeConfig {
        local_node_id,
        bootstrap_nodes,
        ipv4_bind_addr: (!disable_ipv4).then_some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            config.port,
        )),
        ipv6_bind_addr: (!disable_ipv6).then_some(SocketAddr::new(
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            config.port,
        )),
        persistence: persistence_config(),
    })
    .await
    .map_err(|error| error.to_string())?;
    let startup_bootstrap_due =
        (std::env::var_os("SUPERSEEDR_DHT_SKIP_STARTUP_BOOTSTRAP").is_none())
            .then_some(Instant::now() + DHT_STARTUP_BOOTSTRAP_DELAY);

    Ok(BuiltRuntime {
        active_runtime: Some(ActiveRuntime {
            runtime,
            backend: DhtBackendKind::InternalPrototype,
            bootstrap,
            startup_bootstrap_due,
        }),
        backend: DhtBackendKind::InternalPrototype,
        warning,
        bootstrap,
    })
}

fn build_status(
    active_runtime: Option<&ActiveRuntime>,
    backend: DhtBackendKind,
    preferred_backend: DhtBackendKind,
    warning: Option<String>,
    generation: u64,
    bootstrap: BootstrapSummary,
) -> DhtStatus {
    let mut health = DhtHealthSnapshot {
        backend,
        preferred_backend: Some(preferred_backend),
        enabled: !matches!(backend, DhtBackendKind::Disabled),
        exported_bootstrap_nodes: bootstrap.total,
        ipv4_bootstrap_nodes: bootstrap.ipv4,
        ipv6_bootstrap_nodes: bootstrap.ipv6,
        ..Default::default()
    };

    if let Some(active_runtime) = active_runtime {
        let runtime_health = active_runtime.runtime.health_snapshot();
        let ipv4_local_addr = active_runtime.runtime.ipv4_local_addr();
        let ipv6_local_addr = active_runtime.runtime.ipv6_local_addr();
        health.local_addr = ipv4_local_addr.or(ipv6_local_addr);
        health.ipv4_local_addr = ipv4_local_addr;
        health.ipv6_local_addr = ipv6_local_addr;
        health.bound_family_count = active_runtime.runtime.bound_family_count();
        health.cached_ipv4_routes = runtime_health.routing_nodes_ipv4;
        health.cached_ipv6_routes = runtime_health.routing_nodes_ipv6;
        health.active_ipv4_routes = runtime_health.routing_nodes_ipv4;
        health.active_ipv6_routes = runtime_health.routing_nodes_ipv6;
        health.inflight_lookups = active_runtime.runtime.active_lookup_count();
        health.inflight_ipv4_queries = runtime_health.inflight_queries_ipv4;
        health.inflight_ipv6_queries = runtime_health.inflight_queries_ipv6;
        health.server_mode = Some(health.bound_family_count > 0);

        let responsive = runtime_health.bootstrap_responsive_count;
        let responsive_ipv4 = responsive.min(active_runtime.bootstrap.ipv4);
        let responsive_ipv6 = responsive
            .saturating_sub(responsive_ipv4)
            .min(active_runtime.bootstrap.ipv6);
        health.responsive_ipv4_bootstrap_nodes = responsive_ipv4;
        health.responsive_ipv6_bootstrap_nodes = responsive_ipv6;
    }

    DhtStatus {
        generation,
        warning,
        health,
    }
}

fn publish_status(
    status_tx: &watch::Sender<DhtStatus>,
    active_runtime: Option<&ActiveRuntime>,
    warning: Option<String>,
    generation: u64,
    preferred_backend: DhtBackendKind,
) {
    let backend = active_runtime
        .map(|active| active.backend)
        .unwrap_or(DhtBackendKind::Disabled);
    let bootstrap = active_runtime
        .map(|active| active.bootstrap)
        .unwrap_or_default();
    let _ = status_tx.send(build_status(
        active_runtime,
        backend,
        preferred_backend,
        warning,
        generation,
        bootstrap,
    ));
}

fn persistence_config() -> Option<PersistenceConfig> {
    if std::env::var_os("SUPERSEEDR_DHT_DISABLE_PERSISTENCE").is_some()
        || std::env::var_os("SUPERSEEDR_DHT_FRESH_BOOTSTRAP").is_some()
    {
        return None;
    }
    let path = config::runtime_persistence_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dht_state.json");
    Some(PersistenceConfig {
        path,
        max_age: DHT_PERSISTENCE_MAX_AGE,
    })
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

async fn resolve_bootstrap_nodes(bootstrap_nodes: &[String]) -> Vec<SocketAddr> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();

    for bootstrap in bootstrap_nodes {
        let Ok(addresses) = lookup_host(bootstrap.as_str()).await else {
            continue;
        };
        for addr in addresses {
            if seen.insert(addr) {
                resolved.push(addr);
            }
        }
    }

    resolved
}

async fn summarize_lookup_receiver(
    peers_rx: &mut ManagedLookupReceiver,
    idle_timeout: Duration,
    overall_timeout: Duration,
) -> Option<DhtLookupRun> {
    let started_at = std::time::Instant::now();
    let mut idle_sleep = Box::pin(tokio::time::sleep(idle_timeout));
    let overall_sleep = tokio::time::sleep(overall_timeout);
    tokio::pin!(overall_sleep);

    let mut unique_peers = HashSet::new();
    let mut batch_count = 0usize;
    let mut total_peers = 0usize;
    let mut first_batch_ms = None;
    let mut first_ipv4_batch_ms = None;
    let mut first_ipv6_batch_ms = None;

    loop {
        tokio::select! {
            _ = &mut overall_sleep => break,
            _ = &mut idle_sleep => break,
            maybe_batch = peers_rx.recv() => {
                let Some(peers) = maybe_batch else {
                    break;
                };
                batch_count += 1;
                total_peers += peers.len();
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                for peer in peers {
                    if peer.is_ipv4() && first_ipv4_batch_ms.is_none() {
                        first_ipv4_batch_ms = Some(elapsed_ms);
                    }
                    if peer.is_ipv6() && first_ipv6_batch_ms.is_none() {
                        first_ipv6_batch_ms = Some(elapsed_ms);
                    }
                    unique_peers.insert(peer);
                }
                if first_batch_ms.is_none() {
                    first_batch_ms = Some(elapsed_ms);
                }
                idle_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + idle_timeout);
            }
        }
    }

    let unique_ipv4_peers = unique_peers.iter().filter(|peer| peer.is_ipv4()).count();
    let unique_ipv6_peers = unique_peers.len().saturating_sub(unique_ipv4_peers);

    Some(DhtLookupRun {
        batch_count,
        total_peers,
        unique_peers: unique_peers.len(),
        unique_ipv4_peers,
        unique_ipv6_peers,
        first_batch_ms,
        first_ipv4_batch_ms,
        first_ipv6_batch_ms,
    })
}

#[cfg(feature = "dht")]
async fn summarize_lookup_stream<S>(
    peers_stream: &mut S,
    idle_timeout: Duration,
    overall_timeout: Duration,
) -> Option<DhtLookupRun>
where
    S: tokio_stream::Stream<Item = Vec<SocketAddr>> + Unpin,
{
    let started_at = std::time::Instant::now();
    let mut idle_sleep = Box::pin(tokio::time::sleep(idle_timeout));
    let overall_sleep = tokio::time::sleep(overall_timeout);
    tokio::pin!(overall_sleep);

    let mut unique_peers = HashSet::new();
    let mut batch_count = 0usize;
    let mut total_peers = 0usize;
    let mut first_batch_ms = None;
    let mut first_ipv4_batch_ms = None;
    let mut first_ipv6_batch_ms = None;

    loop {
        tokio::select! {
            _ = &mut overall_sleep => break,
            _ = &mut idle_sleep => break,
            maybe_batch = peers_stream.next() => {
                let Some(peers) = maybe_batch else {
                    break;
                };
                batch_count += 1;
                total_peers += peers.len();
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                for peer in peers {
                    if peer.is_ipv4() && first_ipv4_batch_ms.is_none() {
                        first_ipv4_batch_ms = Some(elapsed_ms);
                    }
                    if peer.is_ipv6() && first_ipv6_batch_ms.is_none() {
                        first_ipv6_batch_ms = Some(elapsed_ms);
                    }
                    unique_peers.insert(peer);
                }
                if first_batch_ms.is_none() {
                    first_batch_ms = Some(elapsed_ms);
                }
                idle_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + idle_timeout);
            }
        }
    }

    let unique_ipv4_peers = unique_peers.iter().filter(|peer| peer.is_ipv4()).count();
    let unique_ipv6_peers = unique_peers.len().saturating_sub(unique_ipv4_peers);

    Some(DhtLookupRun {
        batch_count,
        total_peers,
        unique_peers: unique_peers.len(),
        unique_ipv4_peers,
        unique_ipv6_peers,
        first_batch_ms,
        first_ipv4_batch_ms,
        first_ipv6_batch_ms,
    })
}

fn forced_internal_backend_error(config: &DhtServiceConfig) -> Option<String> {
    #[cfg(test)]
    if config.force_internal_failure {
        return Some("forced internal backend failure".to_string());
    }

    let _ = config;
    None
}
