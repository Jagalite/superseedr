// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::Settings;
use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_stream::{empty, Stream, StreamExt};

#[cfg(feature = "dht")]
use mainline::{async_dht::AsyncDht, Dht, Id};

type PeerBatchStream = Pin<Box<dyn Stream<Item = Vec<SocketAddr>> + Send>>;
type HealthFuture = Pin<Box<dyn Future<Output = DhtHealthSnapshot> + Send>>;

const DHT_LOOKUP_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const DHT_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const DHT_HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
                .unwrap_or(DhtBackendKind::Mainline),
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DhtHealthSnapshot {
    pub backend: DhtBackendKind,
    pub enabled: bool,
    pub local_addr: Option<SocketAddr>,
    pub public_addr: Option<SocketAddr>,
    pub firewalled: Option<bool>,
    pub server_mode: Option<bool>,
    pub exported_bootstrap_nodes: usize,
    pub dht_size_estimate: Option<(usize, f64)>,
    pub ipv4_bootstrap_nodes: usize,
    pub ipv6_bootstrap_nodes: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DhtStatus {
    pub generation: u64,
    pub warning: Option<String>,
    pub health: DhtHealthSnapshot,
}

#[derive(Clone)]
struct DhtRuntimeState {
    generation: u64,
    client: Arc<dyn DhtBackendClient>,
}

impl std::fmt::Debug for DhtRuntimeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhtRuntimeState")
            .field("generation", &self.generation)
            .field("backend", &self.client.backend_kind())
            .finish()
    }
}

trait DhtBackendClient: Send + Sync + 'static {
    fn backend_kind(&self) -> DhtBackendKind;
    fn get_peers(&self, info_hash: [u8; 20]) -> PeerBatchStream;
    fn health_snapshot(&self) -> HealthFuture;
}

#[derive(Debug, Clone, Default)]
struct DisabledDhtClient;

impl DhtBackendClient for DisabledDhtClient {
    fn backend_kind(&self) -> DhtBackendKind {
        DhtBackendKind::Disabled
    }

    fn get_peers(&self, _info_hash: [u8; 20]) -> PeerBatchStream {
        Box::pin(empty())
    }

    fn health_snapshot(&self) -> HealthFuture {
        Box::pin(async move {
            DhtHealthSnapshot {
                backend: DhtBackendKind::Disabled,
                enabled: false,
                ..Default::default()
            }
        })
    }
}

#[derive(Debug, Clone)]
struct InternalPrototypeClient {
    state: Arc<InternalPrototypeState>,
}

impl InternalPrototypeClient {
    fn new(bootstrap_nodes: &[String]) -> Self {
        Self {
            state: Arc::new(InternalPrototypeState::from_bootstrap_nodes(bootstrap_nodes)),
        }
    }
}

impl DhtBackendClient for InternalPrototypeClient {
    fn backend_kind(&self) -> DhtBackendKind {
        DhtBackendKind::InternalPrototype
    }

    fn get_peers(&self, _info_hash: [u8; 20]) -> PeerBatchStream {
        Box::pin(empty())
    }

    fn health_snapshot(&self) -> HealthFuture {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            DhtHealthSnapshot {
                backend: DhtBackendKind::InternalPrototype,
                enabled: true,
                ipv4_bootstrap_nodes: state.ipv4_bootstrap_nodes.len(),
                ipv6_bootstrap_nodes: state.ipv6_bootstrap_nodes.len(),
                ..Default::default()
            }
        })
    }
}

#[derive(Debug, Default)]
struct InternalPrototypeState {
    ipv4_bootstrap_nodes: HashSet<SocketAddr>,
    ipv6_bootstrap_nodes: HashSet<SocketAddr>,
}

impl InternalPrototypeState {
    fn from_bootstrap_nodes(nodes: &[String]) -> Self {
        let mut state = Self::default();

        for node in nodes {
            let Ok(addr) = node.parse::<SocketAddr>() else {
                continue;
            };
            if addr.is_ipv4() {
                state.ipv4_bootstrap_nodes.insert(addr);
            } else {
                state.ipv6_bootstrap_nodes.insert(addr);
            }
        }

        state
    }
}

#[cfg(feature = "dht")]
#[derive(Debug, Clone)]
struct MainlineDhtClient {
    inner: AsyncDht,
}

#[cfg(feature = "dht")]
impl MainlineDhtClient {
    fn new(inner: AsyncDht) -> Self {
        Self { inner }
    }
}

#[cfg(feature = "dht")]
impl DhtBackendClient for MainlineDhtClient {
    fn backend_kind(&self) -> DhtBackendKind {
        DhtBackendKind::Mainline
    }

    fn get_peers(&self, info_hash: [u8; 20]) -> PeerBatchStream {
        let Ok(info_hash_id) = Id::from_bytes(info_hash) else {
            return Box::pin(empty());
        };

        let stream = self.inner.get_peers(info_hash_id).map(|peers| {
            peers
                .into_iter()
                .map(SocketAddr::V4)
                .collect::<Vec<SocketAddr>>()
        });

        Box::pin(stream)
    }

    fn health_snapshot(&self) -> HealthFuture {
        let inner = self.inner.clone();
        Box::pin(async move {
            let info = inner.info().await;
            let exported_bootstrap_nodes = inner.to_bootstrap().await.len();

            DhtHealthSnapshot {
                backend: DhtBackendKind::Mainline,
                enabled: true,
                local_addr: Some(SocketAddr::V4(info.local_addr())),
                public_addr: info.public_address().map(SocketAddr::V4),
                firewalled: Some(info.firewalled()),
                server_mode: Some(info.server_mode()),
                exported_bootstrap_nodes,
                dht_size_estimate: Some(info.dht_size_estimate()),
                ..Default::default()
            }
        })
    }
}

#[derive(Debug)]
struct BuiltRuntime {
    runtime: DhtRuntimeState,
    warning: Option<String>,
}

enum DhtCommand {
    Reconfigure(DhtServiceConfig),
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
        let initial = build_runtime(&config, 0, false).await?;
        let initial_status = build_status(&initial.runtime, initial.warning.clone()).await;
        let (runtime_tx, runtime_rx) = watch::channel(initial.runtime);
        let (status_tx, status_rx) = watch::channel(initial_status);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let task = Some(tokio::spawn(run_service(
            config,
            runtime_tx,
            status_tx,
            command_rx,
            shutdown_rx,
        )));

        Ok(Self {
            handle: DhtHandle { runtime_rx },
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

    #[allow(dead_code)]
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

#[derive(Clone)]
pub struct DhtHandle {
    runtime_rx: watch::Receiver<DhtRuntimeState>,
}

impl std::fmt::Debug for DhtHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let runtime = self.runtime_rx.borrow().clone();
        f.debug_struct("DhtHandle")
            .field("generation", &runtime.generation)
            .field("backend", &runtime.client.backend_kind())
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
        let client: Arc<dyn DhtBackendClient> = Arc::new(MainlineDhtClient::new(inner));
        Self::from_client(client, 0)
    }

    fn from_client(client: Arc<dyn DhtBackendClient>, generation: u64) -> Self {
        let (_runtime_tx, runtime_rx) =
            watch::channel(DhtRuntimeState { generation, client });
        Self { runtime_rx }
    }

    pub fn disabled() -> Self {
        let client: Arc<dyn DhtBackendClient> = Arc::new(DisabledDhtClient);
        Self::from_client(client, 0)
    }

    pub fn spawn_lookup_task(
        &self,
        info_hash: Vec<u8>,
        dht_tx: Sender<Vec<SocketAddr>>,
        mut shutdown_rx: broadcast::Receiver<()>,
        mut dht_trigger_rx: watch::Receiver<()>,
    ) -> Option<JoinHandle<()>> {
        let info_hash: [u8; 20] = info_hash.try_into().ok()?;
        let mut runtime_rx = self.runtime_rx.clone();

        Some(tokio::spawn(async move {
            loop {
                let runtime = runtime_rx.borrow().clone();
                let mut peers_stream = runtime.client.get_peers(info_hash);

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
                    changed = runtime_rx.changed() => {
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
}

async fn run_service(
    mut config: DhtServiceConfig,
    runtime_tx: watch::Sender<DhtRuntimeState>,
    status_tx: watch::Sender<DhtStatus>,
    mut command_rx: mpsc::UnboundedReceiver<DhtCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut retry_interval = tokio::time::interval(DHT_RETRY_INTERVAL);
    let mut health_interval = tokio::time::interval(DHT_HEALTH_REFRESH_INTERVAL);
    retry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    health_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut current_generation = status_tx.borrow().generation;

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            Some(command) = command_rx.recv() => {
                match command {
                    DhtCommand::Reconfigure(new_config) => {
                        config = new_config;
                        current_generation = current_generation.saturating_add(1);
                        match build_runtime(&config, current_generation, true).await {
                            Ok(next_runtime) => {
                                let status = build_status(&next_runtime.runtime, next_runtime.warning).await;
                                let _ = runtime_tx.send(next_runtime.runtime);
                                let _ = status_tx.send(status);
                            }
                            Err(error) => {
                                let _ = status_tx.send(DhtStatus {
                                    generation: current_generation,
                                    warning: Some(error),
                                    health: status_tx.borrow().health.clone(),
                                });
                            }
                        }
                    }
                }
            }
            _ = retry_interval.tick(), if status_tx.borrow().warning.is_some() => {
                if let Ok(Some(next_runtime)) = try_recover_preferred_runtime(&config, current_generation.saturating_add(1)).await {
                    current_generation = next_runtime.runtime.generation;
                    let status = build_status(&next_runtime.runtime, None).await;
                    let _ = runtime_tx.send(next_runtime.runtime);
                    let _ = status_tx.send(status);
                }
            }
            _ = health_interval.tick() => {
                let runtime = runtime_tx.borrow().clone();
                let warning = status_tx.borrow().warning.clone();
                let status = build_status(&runtime, warning).await;
                let _ = status_tx.send(status);
            }
        }
    }
}

async fn build_status(runtime: &DhtRuntimeState, warning: Option<String>) -> DhtStatus {
    DhtStatus {
        generation: runtime.generation,
        warning,
        health: runtime.client.health_snapshot().await,
    }
}

async fn build_runtime(
    config: &DhtServiceConfig,
    generation: u64,
    allow_disabled_fallback: bool,
) -> Result<BuiltRuntime, String> {
    let (client, warning) = match config.preferred_backend {
        DhtBackendKind::Disabled => (Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>, None),
        DhtBackendKind::InternalPrototype => (
            Arc::new(InternalPrototypeClient::new(&config.bootstrap_nodes)) as Arc<dyn DhtBackendClient>,
            None,
        ),
        DhtBackendKind::Mainline => build_mainline_runtime(config, allow_disabled_fallback)?,
    };

    Ok(BuiltRuntime {
        runtime: DhtRuntimeState { generation, client },
        warning,
    })
}

#[cfg(feature = "dht")]
fn build_mainline_runtime(
    config: &DhtServiceConfig,
    allow_disabled_fallback: bool,
) -> Result<(Arc<dyn DhtBackendClient>, Option<String>), String> {
    match build_mainline_async(config, true) {
        Ok(inner) => Ok((
            Arc::new(MainlineDhtClient::new(inner)) as Arc<dyn DhtBackendClient>,
            None,
        )),
        Err(bootstrap_error) => {
            let warning = format!(
                "Warning: DHT bootstrap unavailable ({}). Running without bootstrap; retrying automatically.",
                bootstrap_error
            );

            match build_mainline_async(config, false) {
                Ok(inner) => Ok((
                    Arc::new(MainlineDhtClient::new(inner)) as Arc<dyn DhtBackendClient>,
                    Some(warning),
                )),
                Err(fallback_error) if allow_disabled_fallback => Ok((
                    Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>,
                    Some(format!(
                        "Warning: DHT unavailable (bootstrap error: {}; fallback error: {}). Running with DHT disabled until reconfigured.",
                        bootstrap_error, fallback_error
                    )),
                )),
                Err(fallback_error) => Err(format!(
                    "Failed to initialize DHT startup fallback. Bootstrap error: {}. Fallback error: {}",
                    bootstrap_error, fallback_error
                )),
            }
        }
    }
}

#[cfg(not(feature = "dht"))]
fn build_mainline_runtime(
    _config: &DhtServiceConfig,
    _allow_disabled_fallback: bool,
) -> Result<(Arc<dyn DhtBackendClient>, Option<String>), String> {
    Ok((
        Arc::new(DisabledDhtClient) as Arc<dyn DhtBackendClient>,
        None,
    ))
}

#[cfg(feature = "dht")]
fn build_mainline_async(config: &DhtServiceConfig, with_bootstrap: bool) -> Result<AsyncDht, String> {
    let mut builder = Dht::builder();
    let bootstrap_nodes: Vec<&str> = config.bootstrap_nodes.iter().map(String::as_str).collect();

    if with_bootstrap && !bootstrap_nodes.is_empty() {
        builder.bootstrap(&bootstrap_nodes);
    }

    builder
        .port(config.port)
        .server_mode()
        .build()
        .map(|dht| dht.as_async())
        .map_err(|error| error.to_string())
}

#[cfg(feature = "dht")]
async fn try_recover_preferred_runtime(
    config: &DhtServiceConfig,
    generation: u64,
) -> Result<Option<BuiltRuntime>, String> {
    match config.preferred_backend {
        DhtBackendKind::Mainline => match build_mainline_async(config, true) {
            Ok(inner) => Ok(Some(BuiltRuntime {
                runtime: DhtRuntimeState {
                    generation,
                    client: Arc::new(MainlineDhtClient::new(inner)),
                },
                warning: None,
            })),
            Err(_) => Ok(None),
        },
        DhtBackendKind::Disabled | DhtBackendKind::InternalPrototype => Ok(None),
    }
}

#[cfg(not(feature = "dht"))]
async fn try_recover_preferred_runtime(
    _config: &DhtServiceConfig,
    _generation: u64,
) -> Result<Option<BuiltRuntime>, String> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    #[derive(Debug, Clone)]
    struct FakeBackend {
        backend: DhtBackendKind,
        batches: Arc<Mutex<VecDeque<Vec<SocketAddr>>>>,
    }

    impl FakeBackend {
        fn new(backend: DhtBackendKind, batches: Vec<Vec<SocketAddr>>) -> Self {
            Self {
                backend,
                batches: Arc::new(Mutex::new(batches.into())),
            }
        }
    }

    impl DhtBackendClient for FakeBackend {
        fn backend_kind(&self) -> DhtBackendKind {
            self.backend
        }

        fn get_peers(&self, _info_hash: [u8; 20]) -> PeerBatchStream {
            let batches = self
                .batches
                .lock()
                .expect("fake backend lock")
                .drain(..)
                .collect::<Vec<_>>();
            Box::pin(tokio_stream::iter(batches))
        }

        fn health_snapshot(&self) -> HealthFuture {
            let backend = self.backend;
            Box::pin(async move {
                DhtHealthSnapshot {
                    backend,
                    enabled: true,
                    ..Default::default()
                }
            })
        }
    }

    #[tokio::test]
    async fn lookup_task_restarts_after_runtime_update() {
        let first_client: Arc<dyn DhtBackendClient> = Arc::new(FakeBackend::new(
            DhtBackendKind::Mainline,
            vec![vec!["127.0.0.1:41000".parse().expect("v4 peer")]],
        ));
        let second_client: Arc<dyn DhtBackendClient> = Arc::new(FakeBackend::new(
            DhtBackendKind::InternalPrototype,
            vec![vec!["[::1]:42000".parse().expect("v6 peer")]],
        ));
        let (runtime_tx, runtime_rx) = watch::channel(DhtRuntimeState {
            generation: 0,
            client: first_client,
        });
        let handle = DhtHandle { runtime_rx };
        let (dht_tx, mut dht_rx) = mpsc::channel(8);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (trigger_tx, trigger_rx) = watch::channel(());
        let info_hash = vec![1u8; 20];

        let task = handle
            .spawn_lookup_task(info_hash, dht_tx, shutdown_tx.subscribe(), trigger_rx)
            .expect("lookup task");

        let first_batch = tokio::time::timeout(Duration::from_secs(1), dht_rx.recv())
            .await
            .expect("first batch timeout")
            .expect("first batch value");
        assert_eq!(first_batch.len(), 1);
        assert!(first_batch[0].is_ipv4());

        runtime_tx
            .send(DhtRuntimeState {
                generation: 1,
                client: second_client,
            })
            .expect("runtime update");
        trigger_tx.send(()).expect("trigger update");

        let second_batch = tokio::time::timeout(Duration::from_secs(1), dht_rx.recv())
            .await
            .expect("second batch timeout")
            .expect("second batch value");
        assert_eq!(second_batch.len(), 1);
        assert!(second_batch[0].is_ipv6());

        let _ = shutdown_tx.send(());
        task.await.expect("lookup task join");
    }

    #[tokio::test]
    async fn dht_service_reconfigure_switches_backend_and_status_generation() {
        let (shutdown_tx, _) = broadcast::channel(1);
        let service = DhtService::new(
            DhtServiceConfig {
                port: 0,
                bootstrap_nodes: vec!["127.0.0.1:6881".to_string(), "[::1]:6881".to_string()],
                preferred_backend: DhtBackendKind::InternalPrototype,
            },
            shutdown_tx.subscribe(),
        )
        .await
        .expect("internal prototype service");
        let mut status_rx = service.subscribe_status();

        assert_eq!(
            status_rx.borrow().health.backend,
            DhtBackendKind::InternalPrototype
        );
        assert_eq!(status_rx.borrow().generation, 0);

        service.reconfigure(DhtServiceConfig {
            port: 0,
            bootstrap_nodes: Vec::new(),
            preferred_backend: DhtBackendKind::Disabled,
        });

        tokio::time::timeout(Duration::from_secs(1), status_rx.changed())
            .await
            .expect("status change timeout")
            .expect("status change");

        let status = status_rx.borrow().clone();
        assert_eq!(status.generation, 1);
        assert_eq!(status.health.backend, DhtBackendKind::Disabled);
        assert!(status.warning.is_none());

        let _ = shutdown_tx.send(());
    }

    #[test]
    fn internal_prototype_ignores_unparseable_bootstrap_nodes() {
        let state = InternalPrototypeState::from_bootstrap_nodes(&[
            "127.0.0.1:6881".to_string(),
            "[::1]:6881".to_string(),
            "not-an-address".to_string(),
        ]);

        assert_eq!(state.ipv4_bootstrap_nodes.len(), 1);
        assert_eq!(state.ipv6_bootstrap_nodes.len(), 1);
    }

    proptest! {
        #[test]
        fn internal_prototype_keeps_ipv4_and_ipv6_bootstrap_nodes_separated(
            nodes in proptest::collection::vec((any::<bool>(), 1u16..=65535, 1u16..=65535), 0..32)
        ) {
            let raw_nodes = nodes
                .iter()
                .enumerate()
                .map(|(idx, (is_v6, port_a, port_b))| {
                    if *is_v6 {
                        format!("[2001:db8::{:x}]:{}", idx + 1, port_a)
                    } else {
                        format!("10.0.{}.{}:{}", idx % 200, (idx / 200) + 1, port_b)
                    }
                })
                .collect::<Vec<_>>();
            let state = InternalPrototypeState::from_bootstrap_nodes(&raw_nodes);
            let expected_v4 = raw_nodes.iter().filter(|node| !node.starts_with('[')).count();
            let expected_v6 = raw_nodes.iter().filter(|node| node.starts_with('[')).count();

            prop_assert_eq!(state.ipv4_bootstrap_nodes.len(), expected_v4);
            prop_assert_eq!(state.ipv6_bootstrap_nodes.len(), expected_v6);
        }
    }

    #[test]
    fn backend_override_aliases_parse_to_expected_variants() {
        assert_eq!(
            DhtBackendKind::from_override("mainline"),
            Some(DhtBackendKind::Mainline)
        );
        assert_eq!(
            DhtBackendKind::from_override("internal-prototype"),
            Some(DhtBackendKind::InternalPrototype)
        );
        assert_eq!(
            DhtBackendKind::from_override("off"),
            Some(DhtBackendKind::Disabled)
        );
        assert_eq!(DhtBackendKind::from_override("unknown"), None);
    }
}
