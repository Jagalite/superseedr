// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use super::*;

#[derive(Debug)]
pub(in crate::dht::service) struct StartedLookup {
    pub(in crate::dht::service) lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    pub(in crate::dht::service) receiver: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
    pub(in crate::dht::service) accepting_families: Arc<AtomicBool>,
}

pub(in crate::dht::service) struct LookupCancelGuard {
    pub(in crate::dht::service) command_tx: DhtCommandSender,
    pub(in crate::dht::service) lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
}

impl Drop for LookupCancelGuard {
    fn drop(&mut self) {
        let mut lookup_ids = self.lookup_ids.lock().expect("managed dht lookup ids lock");
        if lookup_ids.is_empty() {
            return;
        }
        let _ = send_dht_command(
            &self.command_tx,
            DhtCommand::CancelLookups {
                lookup_ids: std::mem::take(&mut *lookup_ids),
            },
        );
    }
}

pub(in crate::dht::service) struct ManagedLookupReceiver {
    pub(in crate::dht::service) receiver: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
    pub(in crate::dht::service) cancel_guard: Option<LookupCancelGuard>,
}

impl ManagedLookupReceiver {
    pub(in crate::dht::service) fn new(
        receiver: mpsc::UnboundedReceiver<Vec<SocketAddr>>,
        command_tx: DhtCommandSender,
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

    pub(in crate::dht::service) fn empty() -> Self {
        let (_tx, receiver) = mpsc::unbounded_channel();
        Self {
            receiver,
            cancel_guard: None,
        }
    }

    pub(in crate::dht::service) async fn recv(&mut self) -> Option<Vec<SocketAddr>> {
        self.receiver.recv().await
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::dht::service) struct BootstrapSummary {
    pub(in crate::dht::service) total: usize,
    pub(in crate::dht::service) ipv4: usize,
    pub(in crate::dht::service) ipv6: usize,
}

#[derive(Debug)]
pub(in crate::dht::service) struct ActiveRuntime {
    pub(in crate::dht::service) runtime: Runtime,
    pub(in crate::dht::service) backend: DhtBackendKind,
    pub(in crate::dht::service) bootstrap: BootstrapSummary,
    pub(in crate::dht::service) startup_bootstrap_due: Option<Instant>,
}

#[derive(Debug)]
pub(in crate::dht::service) struct BuiltRuntime {
    pub(in crate::dht::service) active_runtime: Option<ActiveRuntime>,
    pub(in crate::dht::service) backend: DhtBackendKind,
    pub(in crate::dht::service) warning: Option<String>,
    pub(in crate::dht::service) bootstrap: BootstrapSummary,
}

#[derive(Debug)]
pub(in crate::dht::service) struct DhtRuntimeBuildError {
    message: String,
    retry_scope_id: Option<NetworkScopeId>,
}

impl DhtRuntimeBuildError {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_scope_id: None,
        }
    }

    pub(super) fn bind(scope_id: NetworkScopeId, error: std::io::Error) -> Self {
        Self {
            retry_scope_id: (error.kind() == std::io::ErrorKind::AddrInUse).then_some(scope_id),
            message: error.to_string(),
        }
    }

    pub(super) fn retry_scope_id(&self) -> Option<NetworkScopeId> {
        self.retry_scope_id
    }
}

impl std::fmt::Display for DhtRuntimeBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}

pub(in crate::dht::service) async fn start_get_peers_lookup(
    active_runtime: Option<&mut ActiveRuntime>,
    command_tx: &DhtCommandSender,
    demand_planner: &mut DemandPlannerModel,
    slice_metrics: Option<&mut DemandSliceMetrics>,
    info_hash: InfoHash,
    slice_class: DemandSliceClass,
    record_metrics: bool,
) -> Result<StartedLookup, String> {
    let Some(active_runtime) = active_runtime else {
        return Ok(StartedLookup {
            lookup_ids: Arc::new(StdMutex::new(Vec::new())),
            receiver: ManagedLookupReceiver::empty().receiver,
            accepting_families: Arc::new(AtomicBool::new(false)),
        });
    };

    let lookup_ids = Arc::new(StdMutex::new(Vec::new()));
    let (merged_tx, merged_rx) = mpsc::unbounded_channel();
    let first_batch_seen = Arc::new(AtomicBool::new(false));
    let accepting_families = Arc::new(AtomicBool::new(true));
    let mut slice_metrics = slice_metrics;

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
            demand_planner,
            slice_metrics.as_deref_mut(),
            info_hash,
            family,
            slice_class,
            merged_tx.clone(),
            lookup_ids.clone(),
            first_batch_seen.clone(),
            accepting_families.clone(),
        )
        .await?;
    }

    let can_try_ipv6_hedge = primary_family == Some(AddressFamily::Ipv4)
        && active_runtime.runtime.family_bound(AddressFamily::Ipv6);
    if can_try_ipv6_hedge {
        let primary_started = !lookup_ids
            .lock()
            .expect("managed dht lookup ids lock")
            .is_empty();
        if primary_started {
            let command_tx = command_tx.clone();
            let merged_tx = merged_tx.clone();
            let lookup_ids = lookup_ids.clone();
            let first_batch_seen = first_batch_seen.clone();
            let accepting_families = accepting_families.clone();
            tokio::spawn(async move {
                tokio::time::sleep(DHT_IPV6_HEDGE_DELAY).await;
                if merged_tx.is_closed() || !accepting_families.load(Ordering::Acquire) {
                    return;
                }
                let _ = send_dht_command(
                    &command_tx,
                    DhtCommand::StartGetPeersFamily {
                        info_hash,
                        family: AddressFamily::Ipv6,
                        slice_class,
                        record_metrics,
                        merged_tx,
                        lookup_ids,
                        first_batch_seen,
                        accepting_families,
                    },
                );
            });
        } else {
            attach_lookup_family(
                Some(active_runtime),
                demand_planner,
                slice_metrics,
                info_hash,
                AddressFamily::Ipv6,
                slice_class,
                merged_tx.clone(),
                lookup_ids.clone(),
                first_batch_seen.clone(),
                accepting_families.clone(),
            )
            .await?;
        }
    }

    if lookup_ids
        .lock()
        .expect("managed dht lookup ids lock")
        .is_empty()
    {
        return Ok(StartedLookup {
            lookup_ids: Arc::new(StdMutex::new(Vec::new())),
            receiver: ManagedLookupReceiver::empty().receiver,
            accepting_families: Arc::new(AtomicBool::new(false)),
        });
    }

    drop(merged_tx);

    Ok(StartedLookup {
        lookup_ids,
        receiver: merged_rx,
        accepting_families,
    })
}

pub(in crate::dht::service) async fn ensure_lookup_routes(
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
        match tokio::time::timeout(Duration::from_millis(200), active_runtime.runtime.step()).await
        {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => break,
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => {}
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(in crate::dht::service) async fn attach_lookup_family(
    active_runtime: Option<&mut ActiveRuntime>,
    demand_planner: &mut DemandPlannerModel,
    slice_metrics: Option<&mut DemandSliceMetrics>,
    info_hash: InfoHash,
    family: AddressFamily,
    slice_class: DemandSliceClass,
    merged_tx: mpsc::UnboundedSender<Vec<SocketAddr>>,
    lookup_ids: Arc<StdMutex<Vec<LookupId>>>,
    first_batch_seen: Arc<AtomicBool>,
    accepting_families: Arc<AtomicBool>,
) -> Result<(), String> {
    let Some(active_runtime) = active_runtime else {
        return Ok(());
    };
    if !accepting_families.load(Ordering::Acquire) {
        return Ok(());
    }
    if !active_runtime.runtime.family_bound(family) {
        return Ok(());
    }

    let mut slice_metrics = slice_metrics;
    let resumed_state = demand_planner.take_parked_family_state(
        slice_metrics.as_deref_mut(),
        info_hash,
        family,
        slice_class,
    );
    let resumed = resumed_state.is_some();
    let (lookup_id, mut family_rx) = match resumed_state {
        Some(state) => active_runtime
            .runtime
            .start_get_peers_with_state(state)
            .await
            .map_err(|error| error.to_string())?,
        None => active_runtime
            .runtime
            .start_get_peers(family, info_hash)
            .await
            .map_err(|error| error.to_string())?,
    };
    if !active_runtime.runtime.is_lookup_active(lookup_id) {
        return Ok(());
    }

    if let Some(metrics) = slice_metrics {
        metrics.record_start(slice_class, resumed);
    }
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

pub(in crate::dht::service) fn announce_peer_job(
    active_runtime: Option<&ActiveRuntime>,
    info_hash: InfoHash,
    port: Option<u16>,
) -> Option<AnnouncePeerJob> {
    active_runtime?.runtime.announce_peer_job(info_hash, port)
}

pub(in crate::dht::service) async fn build_runtime(
    network_activation: &NetworkActivationHandle,
    config: &DhtServiceConfig,
    local_node_id: NodeId,
) -> Result<BuiltRuntime, DhtRuntimeBuildError> {
    build_runtime_for_scope(network_activation, config, local_node_id, None).await
}

pub(in crate::dht::service) async fn build_runtime_for_scope(
    network_activation: &NetworkActivationHandle,
    config: &DhtServiceConfig,
    local_node_id: NodeId,
    expected_scope_id: Option<NetworkScopeId>,
) -> Result<BuiltRuntime, DhtRuntimeBuildError> {
    if let Some(error) = forced_internal_backend_error(config) {
        return Err(DhtRuntimeBuildError::permanent(error));
    }

    if matches!(config.preferred_backend, DhtBackendKind::Disabled) {
        let bootstrap = literal_bootstrap_summary(&config.bootstrap_nodes);
        return Ok(BuiltRuntime {
            active_runtime: None,
            backend: DhtBackendKind::Disabled,
            warning: None,
            bootstrap,
        });
    }

    let active_network = network_activation
        .try_active()
        .map_err(|error| DhtRuntimeBuildError::permanent(error.to_string()))?;
    let scope_id = active_network.scope().id();
    if expected_scope_id.is_some_and(|expected| expected != scope_id) {
        return Err(DhtRuntimeBuildError::permanent(
            "network activation changed before the DHT runtime retry",
        ));
    }
    let network_lease = active_network.scope().lease().clone();
    let bootstrap_nodes =
        crate::dht::resolve_bootstrap_sources(&network_lease, &config.bootstrap_nodes).await;
    let bootstrap = BootstrapSummary {
        total: bootstrap_nodes.len(),
        ipv4: bootstrap_nodes.iter().filter(|addr| addr.is_ipv4()).count(),
        ipv6: bootstrap_nodes.iter().filter(|addr| addr.is_ipv6()).count(),
    };
    let warning = match config.preferred_backend {
        DhtBackendKind::Mainline => {
            Some("mainline backend setting now maps to the internal runtime".to_string())
        }
        _ => None,
    };
    let runtime = Runtime::bind(
        &network_lease,
        RuntimeConfig {
            local_node_id,
            allow_public_ipv4_identity: std::env::var_os("SUPERSEEDR_DHT_NODE_ID_HEX").is_none(),
            bootstrap_nodes,
            bootstrap_sources: config.bootstrap_nodes.clone(),
            ipv4_bind_addr: network_lease.ipv4_enabled().then_some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                config.port,
            )),
            ipv6_bind_addr: network_lease.ipv6_enabled().then_some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                config.port,
            )),
            persistence: persistence_config(),
        },
    )
    .await
    .map_err(|error| DhtRuntimeBuildError::bind(scope_id, error))?;
    let startup_bootstrap_due = (std::env::var_os("SUPERSEEDR_DHT_SKIP_STARTUP_BOOTSTRAP")
        .is_none())
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

pub(in crate::dht::service) fn persistence_config() -> Option<PersistenceConfig> {
    if std::env::var_os("SUPERSEEDR_DHT_DISABLE_PERSISTENCE").is_some()
        || std::env::var_os("SUPERSEEDR_DHT_FRESH_BOOTSTRAP").is_some()
    {
        return None;
    }
    let path = crate::config::runtime_persistence_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dht_state.json");
    Some(PersistenceConfig {
        path,
        max_age: DHT_PERSISTENCE_MAX_AGE,
    })
}

pub(in crate::dht::service) fn literal_bootstrap_summary(
    bootstrap_nodes: &[String],
) -> BootstrapSummary {
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

pub(in crate::dht::service) async fn summarize_lookup_receiver(
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
pub(in crate::dht::service) async fn summarize_lookup_stream<S>(
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
