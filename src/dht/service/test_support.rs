#![allow(dead_code)]

use super::*;
use crate::networking::NetworkSupervisor;

pub(super) fn peer(addr: &str) -> SocketAddr {
    addr.parse().expect("valid socket address")
}

pub(super) fn hash_index(index: u32) -> InfoHash {
    let mut bytes = [0u8; InfoHash::LEN];
    bytes[..4].copy_from_slice(&index.to_be_bytes());
    InfoHash::from(bytes)
}

pub(super) fn active_lookup(lookup_id: LookupId, class: DemandSliceClass) -> ActiveDemandLookup {
    ActiveDemandLookup {
        lookup_ids: Arc::new(StdMutex::new(vec![lookup_id])),
        slice_class: class,
    }
}

pub(super) fn synthetic_peers(key: u8, count: u8) -> HashSet<SocketAddr> {
    (0..count)
        .map(|index| {
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, key, index, key.wrapping_add(index))),
                40_000 + u16::from(index),
            )
        })
        .collect()
}

pub(super) fn lookup_state_for_family(
    lookup_id: LookupId,
    family: AddressFamily,
    target_index: u32,
    now: Instant,
) -> LookupState {
    let bootstrap = match family {
        AddressFamily::Ipv4 => vec![peer("127.0.0.10:6881")],
        AddressFamily::Ipv6 => vec![peer("[::1]:6881")],
    };
    let routing = crate::dht::routing::RoutingSnapshot {
        family,
        buckets: Vec::new(),
        nodes: Vec::new(),
        replacement_count: 0,
        refresh_due_count: 0,
    };
    crate::dht::lookup::LookupManager::new(crate::dht::lookup::LookupConfig::default()).start(
        crate::dht::lookup::LookupRequest {
            lookup_id,
            kind: crate::dht::lookup::LookupKind::GetPeers,
            target: crate::dht::lookup::LookupTarget::InfoHash(hash_index(target_index)),
        },
        family,
        &routing,
        &bootstrap,
        &[],
        now,
    )
}

pub(super) fn disabled_service_config() -> DhtServiceConfig {
    DhtServiceConfig {
        port: 0,
        bootstrap_nodes: Vec::new(),
        preferred_backend: DhtBackendKind::Disabled,
        force_internal_failure: false,
    }
}

pub(super) fn initial_disabled_status(config: &DhtServiceConfig) -> DhtStatus {
    build_status(
        None,
        DhtBackendKind::Disabled,
        config.preferred_backend,
        None,
        0,
        literal_bootstrap_summary(&config.bootstrap_nodes),
    )
}
pub(super) async fn local_ipv4_active_runtime() -> ActiveRuntime {
    let bootstrap_addr = peer("127.0.0.1:9");
    local_ipv4_active_runtime_with_bootstrap(vec![bootstrap_addr]).await
}

pub(super) async fn local_ipv4_active_runtime_without_bootstrap() -> ActiveRuntime {
    local_ipv4_active_runtime_with_bootstrap(Vec::new()).await
}

pub(super) async fn local_ipv4_active_runtime_with_bootstrap(
    bootstrap_nodes: Vec<SocketAddr>,
) -> ActiveRuntime {
    let (network_handle, network_lease) = crate::networking::runtime::test_network_lease();
    let runtime = Runtime::bind(
        &network_lease,
        RuntimeConfig {
            local_node_id: NodeId::from([9u8; NodeId::LEN]),
            allow_public_ipv4_identity: false,
            bootstrap_nodes: bootstrap_nodes.clone(),
            bootstrap_sources: Vec::new(),
            ipv4_bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            ipv6_bind_addr: None,
            persistence: None,
        },
    )
    .await
    .expect("bind local ipv4 runtime");
    std::mem::forget(network_handle);

    ActiveRuntime {
        runtime,
        backend: DhtBackendKind::InternalPrototype,
        bootstrap: BootstrapSummary {
            total: bootstrap_nodes.len(),
            ipv4: bootstrap_nodes.iter().filter(|addr| addr.is_ipv4()).count(),
            ipv6: 0,
        },
        startup_bootstrap_due: None,
    }
}

pub(super) fn test_network_handle() -> NetworkActivationHandle {
    let network_handle = crate::networking::NetworkSupervisor::spawn_unrestricted()
        .expect("spawn test network supervisor")
        .0;
    let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
    publisher
        .activate(network_handle.try_lease().unwrap(), 0)
        .unwrap();
    Box::leak(Box::new(publisher));
    Box::leak(Box::new(network_handle));
    activation
}

pub(super) fn insert_synthetic_drain(
    draining_demands: &mut HashMap<InfoHash, DrainingDemandLookup>,
    info_hash: InfoHash,
    key: u8,
    lookup_id: LookupId,
    slice_class: DemandSliceClass,
    unique_peers: u8,
    now: Instant,
) {
    insert_synthetic_drain_with_stop_reason(
        draining_demands,
        info_hash,
        key,
        lookup_id,
        slice_class,
        DemandSliceStopReason::WallTime,
        unique_peers,
        now,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn insert_synthetic_drain_with_stop_reason(
    draining_demands: &mut HashMap<InfoHash, DrainingDemandLookup>,
    info_hash: InfoHash,
    key: u8,
    lookup_id: LookupId,
    slice_class: DemandSliceClass,
    stop_reason: DemandSliceStopReason,
    unique_peers: u8,
    now: Instant,
) {
    let unique_peers = synthetic_peers(key, unique_peers);
    let unique_peer_count = unique_peers.len();
    let parked_outcome = slice_class.parked_slice_outcome(stop_reason, unique_peer_count, false);
    let duration = demand_drain_duration(
        slice_class,
        stop_reason,
        Some(parked_outcome),
        unique_peer_count,
    )
    .unwrap_or(Duration::from_secs(1));
    draining_demands.insert(
        info_hash,
        DrainingDemandLookup {
            lookup_ids: vec![lookup_id],
            slice_class,
            stop_reason,
            started_at: now,
            total_peers: unique_peer_count,
            initial_unique_peers: unique_peer_count,
            unique_peers,
            deadline: now + duration,
            no_late_yield_deadline: now
                + demand_drain_no_late_yield_grace(slice_class).min(duration),
            initial_inflight_queries: 1,
            score: 1,
        },
    );
}
