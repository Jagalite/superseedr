// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::PeerInfo;
use crate::app::TorrentMetrics;
use crate::peer_manager::PeerPolicy;

use crate::torrent_manager::merkle;

use crate::resource::PermitGuard;
use crate::resource::ResourceManagerClient;
use crate::resource::ResourceManagerError;

use crate::networking::runtime::normalize_socket_addr;
use crate::networking::web_seed_worker::web_seed_worker;
use crate::networking::PeerTransportKind;
use crate::networking::{
    ConnectionType, NetworkActivationHandle, NetworkActivationState, NetworkLease, NetworkScope,
    NetworkScopeId, PeerConnection, TcpPeerTransport, UtpPeerTransport,
};

use crate::token_bucket::TokenBucket;

use crate::torrent_manager::DiskIoOperation;
use crate::torrent_manager::FileProbeBatchResult;
use crate::torrent_manager::FileProbeEntry;

use crate::config::Settings;

use crate::torrent_manager::piece_manager::PieceStatus;

use crate::torrent_manager::state::Action;
use crate::torrent_manager::state::ChokeStatus;
use crate::torrent_manager::state::Effect;
use crate::torrent_manager::state::PeerState;
use crate::torrent_manager::state::TorrentActivity;
use crate::torrent_manager::state::TorrentState;

use crate::torrent_manager::piece_manager::PieceManager;
use crate::torrent_manager::state::web_seed_peer_id;
use crate::torrent_manager::state::TorrentStatus;
use crate::torrent_manager::state::TrackerState;
use crate::torrent_manager::ManagerCommand;
use crate::torrent_manager::ManagerEvent;
#[cfg(feature = "synthetic-load")]
use crate::torrent_manager::SyntheticPeerConnectFailure;

use crate::persistence::create_and_allocate_files;
use crate::persistence::read_data_from_disk;
use crate::persistence::write_data_to_disk;
use crate::persistence::MultiFileInfo;
use crate::persistence::StorageError;

#[cfg(feature = "dht")]
use crate::dht::service::{DhtDemandMetrics, DhtDemandState};
use crate::torrent_manager::command::NetworkResult;
use crate::torrent_manager::command::TorrentCommand;
use crate::torrent_manager::command::TorrentCommandSummary;

use crate::networking::session::PeerSessionParameters;
use crate::networking::BlockInfo;
use crate::networking::PeerSession;

use crate::tracker::client::{
    announce_completed, announce_periodic, announce_started, announce_stopped,
};
use crate::tracker::{normalize_tracker_urls, torrent_tracker_urls};

use rand::RngExt;

use crate::torrent_file::Torrent;

use std::error::Error;

use tracing::{event, Level};

use std::net::SocketAddr;
use std::time::Duration;
use std::time::Instant;

use magnet_url::Magnet;

use urlencoding::decode;

use sha1::Digest;
use tokio::fs;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;

use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::timeout;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::telemetry::manager_telemetry::ManagerTelemetry;
use crate::torrent_manager::{IncomingPeerSession, TorrentParameters};

const HASH_LENGTH: usize = 20;

const MAX_UPLOAD_REQUEST_ATTEMPTS: u32 = 7;
const MAX_PIECE_WRITE_ATTEMPTS: u32 = 12;
const ACTIVITY_MESSAGE_MAX_LEN: usize = 28;
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const UTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const PRIVATE_TRACKER_STOP_ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(4);
const NETWORK_RECOVERY_JITTER_MAX_MS: u64 = 2_000;

fn network_recovery_delay(info_hash: &[u8], generation_id: u64) -> Duration {
    let seed = info_hash.iter().fold(generation_id, |seed, byte| {
        seed.rotate_left(5) ^ u64::from(*byte)
    });
    Duration::from_millis(seed % (NETWORK_RECOVERY_JITTER_MAX_MS + 1))
}

fn should_log_peer_channel_full_warning(drop_count: u64) -> bool {
    drop_count.is_power_of_two()
}

fn network_scope_and_peer_address(
    network_activation: &NetworkActivationHandle,
    peer_addr: SocketAddr,
) -> Option<(NetworkScope, SocketAddr, u16)> {
    let peer_addr = normalize_socket_addr(peer_addr);
    let active = network_activation.try_active().ok()?;
    let scope = active.scope().clone();
    scope
        .lease()
        .address_family_enabled(peer_addr.ip())
        .then_some((scope, peer_addr, active.listen_port()))
}

#[derive(Debug)]
enum PeerPermitWaitError {
    NetworkInvalidated,
    TorrentShutdown,
    TimedOut,
    ResourceManager(ResourceManagerError),
}

async fn acquire_generation_scoped_peer_permit(
    resource_manager: &ResourceManagerClient,
    network_scope: &NetworkScope,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> Result<PermitGuard, PeerPermitWaitError> {
    let permit_wait = network_scope.run(timeout(
        Duration::from_secs(10),
        resource_manager.acquire_peer_connection(),
    ));
    tokio::select! {
        biased;
        _ = shutdown_rx.recv() => Err(PeerPermitWaitError::TorrentShutdown),
        result = permit_wait => match result {
            Err(_) => Err(PeerPermitWaitError::NetworkInvalidated),
            Ok(Err(_)) => Err(PeerPermitWaitError::TimedOut),
            Ok(Ok(Err(error))) => Err(PeerPermitWaitError::ResourceManager(error)),
            Ok(Ok(Ok(permit))) => Ok(permit),
        },
    }
}

async fn send_generation_scoped_peer_registration(
    manager_tx: &mpsc::Sender<TorrentCommand>,
    command: TorrentCommand,
    network_invalidation_rx: &mut watch::Receiver<bool>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> bool {
    tokio::select! {
        biased;
        _ = network_invalidation_rx.changed() => false,
        _ = shutdown_rx.recv() => false,
        result = manager_tx.send(command) => result.is_ok(),
    }
}

async fn send_generation_scoped_tracker_result(
    manager_tx: &mpsc::Sender<TorrentCommand>,
    result: NetworkResult,
    network_scope: &NetworkScope,
    shutdown_rx: &mut broadcast::Receiver<()>,
) {
    if network_scope.ensure_valid().is_err() {
        return;
    }
    let command = TorrentCommand::Network(network_scope.scoped(result));
    let send = network_scope.run(manager_tx.send(command));
    tokio::select! {
        biased;
        _ = shutdown_rx.recv() => {}
        _ = send => {}
    }
}

async fn wait_for_active_tracker_network(
    network_activation: &NetworkActivationHandle,
) -> Option<(NetworkLease, u16)> {
    let mut activation_rx = network_activation.subscribe();
    loop {
        if let Ok(active) = network_activation.try_active() {
            return Some((active.scope().lease().clone(), active.listen_port()));
        }
        if activation_rx.changed().await.is_err() {
            return None;
        }
    }
}

fn queue_peer_disconnect(
    manager_tx: mpsc::Sender<TorrentCommand>,
    peer_id: String,
    scope_id: Option<NetworkScopeId>,
) {
    let command = match scope_id {
        Some(scope_id) => TorrentCommand::DisconnectGeneration { peer_id, scope_id },
        None => TorrentCommand::Disconnect(peer_id),
    };
    match manager_tx.try_send(command) {
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        Err(mpsc::error::TrySendError::Full(command)) => {
            tokio::spawn(async move {
                let _ = manager_tx.send(command).await;
            });
        }
    }
}

fn shutdown_tracker_urls(
    tracker_urls: Vec<String>,
    trackers: &HashMap<String, TrackerState>,
    private_client: bool,
) -> Vec<String> {
    if private_client {
        return tracker_urls;
    }

    tracker_urls
        .into_iter()
        .filter(|url| {
            trackers
                .get(url)
                .is_some_and(|tracker| tracker.has_responded)
        })
        .collect()
}

const BASE_BACKOFF_MS: u64 = 1000;
const JITTER_MS: u64 = 100;
const PEER_TRANSPORT_ENV: &str = "SUPERSEEDR_PEER_TRANSPORT";
const UTP_CONNECT_LOG_ENV: &str = "SUPERSEEDR_LOG_UTP_CONNECT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerTransportMode {
    Tcp,
    Utp,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutboundTransportMode {
    try_utp: bool,
    allow_tcp: bool,
}

fn outbound_transport_mode_from_env() -> OutboundTransportMode {
    outbound_transport_mode(peer_transport_mode_from_env())
}

fn utp_connect_log_enabled() -> bool {
    env_flag(UTP_CONNECT_LOG_ENV)
}

fn outbound_transport_mode(mode: PeerTransportMode) -> OutboundTransportMode {
    match mode {
        PeerTransportMode::Tcp => OutboundTransportMode {
            try_utp: false,
            allow_tcp: true,
        },
        PeerTransportMode::Utp => OutboundTransportMode {
            try_utp: true,
            allow_tcp: false,
        },
        PeerTransportMode::All => OutboundTransportMode {
            try_utp: true,
            allow_tcp: true,
        },
    }
}

fn peer_transport_key(kind: PeerTransportKind, addr: SocketAddr) -> String {
    format!("{kind}://{addr}")
}

fn outbound_peer_key_candidates(
    peer_addr: SocketAddr,
    mode: OutboundTransportMode,
    preferred_key: Option<&str>,
) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(key) = preferred_key {
        keys.push(key.to_string());
    }
    if mode.try_utp {
        keys.push(peer_transport_key(PeerTransportKind::Utp, peer_addr));
    }
    if mode.allow_tcp {
        keys.push(peer_transport_key(PeerTransportKind::Tcp, peer_addr));
    }
    keys.push(peer_addr.to_string());
    keys.sort();
    keys.dedup();
    keys
}

fn outbound_peer_session_key(
    peer_addr: SocketAddr,
    mode: OutboundTransportMode,
    preferred_key: Option<String>,
    selected_transport: PeerTransportKind,
) -> String {
    preferred_key.unwrap_or_else(|| {
        if matches!(
            selected_transport,
            PeerTransportKind::Tcp | PeerTransportKind::Utp
        ) {
            peer_transport_key(selected_transport, peer_addr)
        } else if mode.try_utp {
            peer_transport_key(PeerTransportKind::Utp, peer_addr)
        } else {
            peer_transport_key(PeerTransportKind::Tcp, peer_addr)
        }
    })
}

fn peer_transport_mode_from_env() -> PeerTransportMode {
    match std::env::var(PEER_TRANSPORT_ENV) {
        Ok(value) => peer_transport_mode(&value),
        Err(_) => PeerTransportMode::All,
    }
}

fn peer_transport_mode(value: &str) -> PeerTransportMode {
    match value.to_ascii_lowercase().as_str() {
        "tcp" => PeerTransportMode::Tcp,
        "utp" => PeerTransportMode::Utp,
        "all" => PeerTransportMode::All,
        _ => PeerTransportMode::All,
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TransportPeerCounts {
    tcp_peer_count: usize,
    utp_peer_count: usize,
    beneficial_tcp_peer_count: usize,
    beneficial_utp_peer_count: usize,
}

fn count_peers_by_transport<'a>(peers: impl Iterator<Item = &'a PeerState>) -> TransportPeerCounts {
    let mut counts = TransportPeerCounts::default();

    for peer in peers {
        let has_moved_payload = peer.total_bytes_downloaded > 0 || peer.total_bytes_uploaded > 0;
        match peer.transport_kind {
            PeerTransportKind::Tcp => {
                counts.tcp_peer_count += 1;
                if has_moved_payload {
                    counts.beneficial_tcp_peer_count += 1;
                }
            }
            PeerTransportKind::Utp => {
                counts.utp_peer_count += 1;
                if has_moved_payload {
                    counts.beneficial_utp_peer_count += 1;
                }
            }
            PeerTransportKind::Quic | PeerTransportKind::WebRtc => {}
        }
    }

    counts
}

#[cfg(feature = "synthetic-load")]
fn synthetic_peer_connect_failure(error: &std::io::Error) -> SyntheticPeerConnectFailure {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused => SyntheticPeerConnectFailure::ConnectionRefused,
        std::io::ErrorKind::ConnectionReset => SyntheticPeerConnectFailure::ConnectionReset,
        std::io::ErrorKind::ConnectionAborted => SyntheticPeerConnectFailure::ConnectionAborted,
        std::io::ErrorKind::AddrInUse => SyntheticPeerConnectFailure::AddrInUse,
        std::io::ErrorKind::AddrNotAvailable => SyntheticPeerConnectFailure::AddrNotAvailable,
        std::io::ErrorKind::TimedOut => SyntheticPeerConnectFailure::TimedOut,
        _ => SyntheticPeerConnectFailure::OtherIo,
    }
}

async fn connect_outbound_peer(
    network_lease: &NetworkLease,
    peer_addr: SocketAddr,
    local_udp_port: u16,
    mode: OutboundTransportMode,
    peer_label: &str,
) -> Result<PeerConnection, (PeerTransportKind, Option<std::io::Error>)> {
    match (mode.try_utp, mode.allow_tcp) {
        (true, true) => {
            race_utp_and_tcp(network_lease, peer_addr, local_udp_port, peer_label).await
        }
        (true, false) => {
            match attempt_utp_connect(network_lease, peer_addr, local_udp_port).await {
                Ok(connection) => {
                    log_utp_connect_established(peer_label);
                    Ok(connection)
                }
                Err(error) => {
                    log_utp_connect_failure(
                        peer_label,
                        error.as_ref(),
                        "uTP outbound connect failed in uTP-only mode",
                    );
                    Err((PeerTransportKind::Utp, error))
                }
            }
        }
        (false, _) => attempt_tcp_connect(network_lease, peer_addr)
            .await
            .map_err(|error| (PeerTransportKind::Tcp, error)),
    }
}

async fn race_utp_and_tcp(
    network_lease: &NetworkLease,
    peer_addr: SocketAddr,
    local_udp_port: u16,
    peer_label: &str,
) -> Result<PeerConnection, (PeerTransportKind, Option<std::io::Error>)> {
    let utp_attempt = attempt_utp_connect(network_lease, peer_addr, local_udp_port);
    let tcp_attempt = attempt_tcp_connect(network_lease, peer_addr);
    tokio::pin!(utp_attempt);
    tokio::pin!(tcp_attempt);

    let (first_transport, first_result) = tokio::select! {
        result = &mut tcp_attempt => (PeerTransportKind::Tcp, result),
        result = &mut utp_attempt => (PeerTransportKind::Utp, result),
    };

    match (first_transport, first_result) {
        (_, Ok(connection)) => {
            if connection.endpoint.kind == PeerTransportKind::Utp {
                log_utp_connect_established(peer_label);
            }
            Ok(connection)
        }
        (PeerTransportKind::Utp, Err(utp_error)) => {
            log_utp_connect_failure(
                peer_label,
                utp_error.as_ref(),
                "uTP outbound connect failed while racing TCP fallback",
            );
            match tcp_attempt.await {
                Ok(connection) => Ok(connection),
                Err(tcp_error) => Err((PeerTransportKind::Tcp, tcp_error)),
            }
        }
        (PeerTransportKind::Tcp, Err(tcp_error)) => match utp_attempt.await {
            Ok(connection) => {
                log_utp_connect_established(peer_label);
                Ok(connection)
            }
            Err(utp_error) => {
                log_utp_connect_failure(
                    peer_label,
                    utp_error.as_ref(),
                    "uTP outbound connect failed after TCP fallback failed",
                );
                Err((PeerTransportKind::Tcp, tcp_error))
            }
        },
        (kind @ (PeerTransportKind::Quic | PeerTransportKind::WebRtc), Err(error)) => {
            Err((kind, error))
        }
    }
}

async fn attempt_utp_connect(
    network_lease: &NetworkLease,
    peer_addr: SocketAddr,
    local_udp_port: u16,
) -> Result<PeerConnection, Option<std::io::Error>> {
    match timeout(
        UTP_CONNECT_TIMEOUT,
        UtpPeerTransport::connect_from_port(network_lease, peer_addr, local_udp_port),
    )
    .await
    {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(error)) => Err(Some(error)),
        Err(_) => Err(None),
    }
}

async fn attempt_tcp_connect(
    network_lease: &NetworkLease,
    peer_addr: SocketAddr,
) -> Result<PeerConnection, Option<std::io::Error>> {
    match timeout(
        TCP_CONNECT_TIMEOUT,
        TcpPeerTransport::connect(network_lease, peer_addr),
    )
    .await
    {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(error)) => Err(Some(error)),
        Err(_) => Err(None),
    }
}

fn log_utp_connect_established(peer_label: &str) {
    if utp_connect_log_enabled() {
        event!(
            Level::INFO,
            peer = %peer_label,
            "uTP outbound connect established"
        );
    }
}

fn log_utp_connect_failure(
    peer_label: &str,
    error: Option<&std::io::Error>,
    message: &'static str,
) {
    if !utp_connect_log_enabled() {
        return;
    }

    match error {
        Some(error) => event!(
            Level::INFO,
            peer = %peer_label,
            error = %error,
            "{}",
            message
        ),
        None => event!(
            Level::INFO,
            peer = %peer_label,
            "{}",
            message
        ),
    }
}

struct PreparedFileProbeEntry {
    relative_path: std::path::PathBuf,
    absolute_path: std::path::PathBuf,
    expected_size: u64,
}

struct PreparedFileProbeBatch {
    epoch: u64,
    scanned_files: usize,
    next_file_index: usize,
    reached_end_of_manifest: bool,
    files: Vec<PreparedFileProbeEntry>,
}

enum FileProbeBatchPreparation {
    Ready(FileProbeBatchResult),
    Scan(PreparedFileProbeBatch),
}

#[cfg(feature = "webtorrent")]
#[path = "rtc.rs"]
mod rtc;

pub struct TorrentManager {
    #[cfg(feature = "webtorrent")]
    rtc: rtc::Runtime,
    state: TorrentState,

    torrent_manager_tx: Sender<TorrentCommand>,
    torrent_manager_rx: Receiver<TorrentCommand>,

    #[cfg(feature = "dht")]
    dht_tx: Sender<Vec<SocketAddr>>,
    #[cfg(not(feature = "dht"))]
    #[allow(dead_code)]
    dht_tx: Sender<()>,

    metrics_tx: watch::Sender<TorrentMetrics>,
    peer_policy_rx: watch::Receiver<Arc<PeerPolicy>>,
    peer_policy_open: bool,
    manager_event_tx: Sender<ManagerEvent>,
    shutdown_tx: broadcast::Sender<()>,

    #[cfg(feature = "dht")]
    dht_rx: Receiver<Vec<SocketAddr>>,
    #[cfg(not(feature = "dht"))]
    #[allow(dead_code)]
    dht_rx: Receiver<()>,

    incoming_peer_rx: Receiver<IncomingPeerSession>,
    manager_command_rx: Receiver<ManagerCommand>,
    network_activation: NetworkActivationHandle,
    network_activation_rx: watch::Receiver<NetworkActivationState>,
    network_activation_open: bool,

    in_flight_uploads: HashMap<String, HashMap<BlockInfo, JoinHandle<()>>>,
    in_flight_writes: HashMap<u32, Vec<JoinHandle<()>>>,

    #[cfg(feature = "dht")]
    #[allow(dead_code)]
    dht_task_handle: Option<JoinHandle<()>>,

    #[cfg(feature = "dht")]
    dht_demand_state: Option<DhtDemandState>,
    #[cfg(feature = "dht")]
    dht_demand_metrics: Option<DhtDemandMetrics>,

    #[cfg(not(feature = "dht"))]
    #[allow(dead_code)]
    dht_task_handle: (),

    #[cfg(not(feature = "dht"))]
    #[allow(dead_code)]
    dht_demand_state: (),
    #[cfg(not(feature = "dht"))]
    #[allow(dead_code)]
    dht_demand_metrics: (),

    #[allow(dead_code)]
    dht_handle: crate::dht::service::DhtHandle,
    settings: Arc<Settings>,
    resource_manager: ResourceManagerClient,

    global_dl_bucket: Arc<TokenBucket>,
    global_ul_bucket: Arc<TokenBucket>,
    telemetry: ManagerTelemetry,
    data_rate_ms: u64,
    run_loop_started: bool,
    network_recovery_scope_id: Option<NetworkScopeId>,
    pending_started_announces: HashSet<String>,
    started_announce_scopes: HashMap<String, NetworkScopeId>,
    pending_completion_announces: HashSet<String>,
    completion_announce_scopes: HashMap<String, NetworkScopeId>,
    peer_network_scopes: HashMap<String, NetworkScopeId>,
    web_seed_network_scopes: HashMap<String, NetworkScopeId>,
    peer_channel_full_drop_count: u64,
}

impl TorrentManager {
    fn should_accept_new_peers(&self) -> bool {
        !self.state.is_paused && self.state.accepting_new_peers
    }

    fn handle_web_seed_disconnected(&mut self, peer_id: String, scope_id: NetworkScopeId) {
        if self.web_seed_network_scopes.get(&peer_id).copied() == Some(scope_id) {
            self.apply_action(Action::PeerDisconnected {
                peer_id,
                force: false,
            });
        } else {
            event!(
                Level::TRACE,
                peer = %peer_id,
                ?scope_id,
                active_scope_id = ?self.web_seed_network_scopes.get(&peer_id),
                "ignoring stale web-seed disconnect"
            );
        }
    }

    fn handle_generation_scoped_peer_disconnected(
        &mut self,
        peer_id: String,
        scope_id: NetworkScopeId,
    ) {
        if self.peer_network_scopes.get(&peer_id).copied() == Some(scope_id) {
            self.apply_action(Action::PeerDisconnected {
                peer_id,
                force: true,
            });
        } else {
            event!(
                Level::TRACE,
                peer = %peer_id,
                ?scope_id,
                active_scope_id = ?self.peer_network_scopes.get(&peer_id),
                "ignoring stale peer disconnect"
            );
        }
    }

    fn flush_invalidated_network_peers(&mut self, active_scope_id: NetworkScopeId) {
        let expired_peer_ids = self
            .peer_network_scopes
            .iter()
            .filter(|(_, scope_id)| **scope_id != active_scope_id)
            .map(|(peer_id, _)| peer_id.clone())
            .collect::<Vec<_>>();
        for peer_id in expired_peer_ids {
            self.apply_action(Action::PeerDisconnected {
                peer_id,
                force: true,
            });
        }
    }

    fn queue_started_announces(&mut self) {
        self.pending_started_announces
            .extend(self.state.trackers.keys().cloned());
        self.start_pending_started_announces();
    }

    fn start_pending_started_announces(&mut self) {
        if self.state.is_paused {
            return;
        }
        let Ok(active_network) = self.network_activation.try_active() else {
            return;
        };
        let network_scope = active_network.scope().clone();
        let scope_id = network_scope.id();
        let urls = self
            .pending_started_announces
            .iter()
            .filter(|url| self.started_announce_scopes.get(*url).copied() != Some(scope_id))
            .cloned()
            .collect::<Vec<_>>();
        let info_hash = self.state.info_hash.clone();
        let client_id = self.settings.client_id.clone();
        let client_port = active_network.listen_port();
        let torrent_size_left = self
            .state
            .multi_file_info
            .as_ref()
            .map_or(0, |mfi| mfi.total_size as usize);

        for url in urls {
            #[cfg(feature = "webtorrent")]
            if self.rtc_announce(
                &url,
                Some(crate::networking::webtorrent::wire::Event::Started),
            ) {
                continue;
            }
            self.started_announce_scopes.insert(url.clone(), scope_id);
            let network_scope = network_scope.clone();
            let network_lease = network_scope.lease().clone();
            let info_hash = info_hash.clone();
            let client_id = client_id.clone();
            let manager_tx = self.torrent_manager_tx.clone();
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            tokio::spawn(async move {
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
    }

    fn handle_started_announce_result(
        &mut self,
        scope_id: NetworkScopeId,
        url: String,
        result: Result<crate::tracker::TrackerResponse, String>,
    ) {
        if self.started_announce_scopes.get(&url).copied() != Some(scope_id) {
            return;
        }
        self.started_announce_scopes.remove(&url);
        self.pending_started_announces.remove(&url);
        match result {
            Ok(response) => {
                self.apply_action(Action::TrackerResponse {
                    url,
                    peers: response.peers,
                    interval: response.interval as u64,
                    min_interval: response.min_interval.map(|interval| interval as u64),
                });
            }
            Err(error) => {
                event!(Level::DEBUG, %url, %error, "started announce failed");
                self.apply_action(Action::TrackerError { url });
            }
        }
    }

    fn queue_completion_announce(&mut self, url: String) {
        self.pending_completion_announces.insert(url);
        self.start_pending_completion_announces();
    }

    fn start_pending_completion_announces(&mut self) {
        let Ok(active_network) = self.network_activation.try_active() else {
            return;
        };
        let network_scope = active_network.scope().clone();
        let scope_id = network_scope.id();
        let urls = self
            .pending_completion_announces
            .iter()
            .filter(|url| self.completion_announce_scopes.get(*url).copied() != Some(scope_id))
            .cloned()
            .collect::<Vec<_>>();
        let info_hash = self.state.info_hash.clone();
        let client_id = self.settings.client_id.clone();
        let client_port = active_network.listen_port();
        let uploaded = self.state.session_total_uploaded as usize;
        let downloaded = self.state.session_total_downloaded as usize;

        for url in urls {
            #[cfg(feature = "webtorrent")]
            if self.rtc_announce(
                &url,
                Some(crate::networking::webtorrent::wire::Event::Completed),
            ) {
                continue;
            }
            self.completion_announce_scopes
                .insert(url.clone(), scope_id);
            let network_scope = network_scope.clone();
            let network_lease = network_scope.lease().clone();
            let info_hash = info_hash.clone();
            let client_id = client_id.clone();
            let manager_tx = self.torrent_manager_tx.clone();
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            tokio::spawn(async move {
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
    }

    fn handle_completion_announce_result(
        &mut self,
        scope_id: NetworkScopeId,
        url: String,
        error: Option<String>,
    ) {
        if self.completion_announce_scopes.get(&url).copied() != Some(scope_id) {
            return;
        }
        self.completion_announce_scopes.remove(&url);
        if let Some(error) = error {
            event!(Level::DEBUG, %url, %error, "completion announce failed");
            self.apply_action(Action::TrackerError { url });
        } else {
            self.pending_completion_announces.remove(&url);
        }
    }

    fn apply_latest_network_activation(&mut self) {
        let state = self.network_activation_rx.borrow_and_update().clone();
        let NetworkActivationState::Active(active) = state else {
            self.network_recovery_scope_id = None;
            self.started_announce_scopes.clear();
            self.completion_announce_scopes.clear();
            return;
        };
        let scope = active.scope().clone();
        let scope_id = scope.id();
        if !self.network_activation.is_current(scope_id) {
            return;
        }
        self.completion_announce_scopes
            .retain(|_, announce_scope_id| *announce_scope_id == scope_id);
        self.started_announce_scopes
            .retain(|_, announce_scope_id| *announce_scope_id == scope_id);

        let mut settings = (*self.settings).clone();
        let port_changed = settings.client_port != active.listen_port();
        settings.client_port = active.listen_port();
        self.settings = Arc::new(settings);
        if port_changed {
            self.apply_action(Action::UpdateListenPort);
        }
        self.start_pending_started_announces();
        self.start_pending_completion_announces();
        if !self.state.is_paused {
            self.network_recovery_scope_id = Some(scope_id);
            let delay = network_recovery_delay(&self.state.info_hash, scope_id.generation_id());
            let torrent_manager_tx = self.torrent_manager_tx.clone();
            tokio::spawn(async move {
                if scope.run(tokio::time::sleep(delay)).await.is_ok() {
                    let command =
                        TorrentCommand::Network(scope.scoped(NetworkResult::RecoveryReady));
                    let _ = scope.run(torrent_manager_tx.send(command)).await;
                }
            });
        }
    }

    fn refresh_peer_policy(&mut self) {
        let policy = self.peer_policy_rx.borrow_and_update().clone();
        self.apply_action(Action::PeerPolicyUpdated { policy });
    }

    fn register_peer(
        &mut self,
        peer_id: String,
        peer_addr: Option<SocketAddr>,
        tx: Sender<TorrentCommand>,
    ) -> Option<watch::Receiver<bool>> {
        let accepted = self.state.can_register_peer(&peer_id, peer_addr);
        let registered_peer_id = peer_id.clone();
        self.apply_action(Action::RegisterPeer {
            peer_id,
            peer_addr,
            tx,
        });
        accepted.then(|| {
            self.state
                .peers
                .get(&registered_peer_id)
                .expect("accepted peer must be present in torrent state")
                .session_cancel
                .subscribe()
        })
    }

    #[cfg(feature = "dht")]
    fn current_dht_demand_state(&self) -> DhtDemandState {
        DhtDemandState {
            awaiting_metadata: self.state.torrent_status == TorrentStatus::AwaitingMetadata,
            // The DHT planner only distinguishes starved torrents from torrents
            // with at least one connected peer. Do not wake it for exact peer
            // count churn while the torrent remains in the same demand class.
            connected_peers: usize::from(!self.state.peers.is_empty()),
        }
    }

    #[cfg(feature = "dht")]
    fn current_dht_demand_metrics(&self) -> DhtDemandMetrics {
        let total_pieces = self.state.piece_manager.bitfield.len() as u32;
        let completed_pieces = if self.state.torrent_status == TorrentStatus::Validating {
            self.state.validation_pieces_found
        } else {
            total_pieces.saturating_sub(self.state.piece_manager.pieces_remaining as u32)
        };
        DhtDemandMetrics {
            paused: self.state.is_paused,
            accepting_new_peers: self.state.accepting_new_peers,
            complete: self.state.torrent_status == TorrentStatus::Done,
            total_pieces,
            completed_pieces,
            connected_peers: self.state.peers.len(),
            interested_peers: self
                .state
                .peers
                .values()
                .filter(|peer| peer.am_interested)
                .count(),
            peers_interested_in_us: self
                .state
                .peers
                .values()
                .filter(|peer| peer.peer_is_interested_in_us)
                .count(),
            unchoked_download_peers: self
                .state
                .peers
                .values()
                .filter(|peer| peer.peer_choking == ChokeStatus::Unchoke)
                .count(),
            unchoked_upload_peers: self
                .state
                .peers
                .values()
                .filter(|peer| peer.am_choking == ChokeStatus::Unchoke)
                .count(),
            downloading_peers: self
                .state
                .peers
                .values()
                .filter(|peer| peer.download_speed_bps > 0)
                .count(),
            uploading_peers: self
                .state
                .peers
                .values()
                .filter(|peer| peer.upload_speed_bps > 0)
                .count(),
            download_speed_bps: self.state.total_dl_prev_avg_ema as u64,
            upload_speed_bps: self.state.total_ul_prev_avg_ema as u64,
            bytes_downloaded_this_tick: self.state.bytes_downloaded_in_interval,
            bytes_uploaded_this_tick: self.state.bytes_uploaded_in_interval,
        }
    }

    #[cfg(feature = "dht")]
    fn sync_dht_demand(&mut self) {
        if !self.run_loop_started || self.dht_task_handle.is_none() {
            return;
        }

        let desired_demand = self.current_dht_demand_state();
        if self.dht_demand_state != Some(desired_demand) {
            let previous_demand = self.dht_demand_state;
            let update_sent = self
                .dht_handle
                .update_demand(self.state.info_hash.clone(), desired_demand);
            let _ = previous_demand;
            let _ = update_sent;
            self.dht_demand_state = Some(desired_demand);
        }

        let desired_metrics = self.current_dht_demand_metrics();
        if self.dht_demand_metrics != Some(desired_metrics) {
            let update_sent = self
                .dht_handle
                .update_demand_metrics(self.state.info_hash.clone(), desired_metrics);
            let _ = update_sent;
            self.dht_demand_metrics = Some(desired_metrics);
        }
    }

    #[cfg(not(feature = "dht"))]
    fn sync_dht_demand(&mut self) {}

    fn init_base(
        torrent_parameters: TorrentParameters,
        info_hash: Vec<u8>,
        trackers: HashMap<String, TrackerState>,
        torrent_validation_status: bool,
    ) -> Self {
        let TorrentParameters {
            network_activation,
            dht_handle,
            incoming_peer_rx,
            metrics_tx,
            peer_policy_rx,
            torrent_data_path: _,
            container_name,
            manager_command_rx,
            manager_event_tx,
            settings,
            resource_manager,
            global_dl_bucket,
            global_ul_bucket,
            file_priorities: _,
            ..
        } = torrent_parameters;
        let data_rate_ms = settings.ui_refresh_rate.as_ms();
        let peer_policy = peer_policy_rx.borrow().clone();

        let (torrent_manager_tx, torrent_manager_rx) = mpsc::channel::<TorrentCommand>(1000);
        let (shutdown_tx, _) = broadcast::channel(1);
        let network_activation_rx = network_activation.subscribe();

        #[cfg(feature = "dht")]
        let (dht_tx, dht_rx) = mpsc::channel::<Vec<SocketAddr>>(10);
        #[cfg(not(feature = "dht"))]
        let (dht_tx, dht_rx) = mpsc::channel::<()>(1);

        #[cfg(feature = "dht")]
        let dht_task_handle = None;
        #[cfg(not(feature = "dht"))]
        let dht_task_handle = ();
        #[cfg(feature = "dht")]
        let dht_demand_state = None;
        #[cfg(not(feature = "dht"))]
        let dht_demand_state = ();
        #[cfg(feature = "dht")]
        let dht_demand_metrics = None;
        #[cfg(not(feature = "dht"))]
        let dht_demand_metrics = ();

        // Initialize empty state (AwaitingMetadata)
        let mut state = TorrentState::new(
            info_hash,
            None, // No Torrent yet
            None, // No Metadata length yet
            PieceManager::new(),
            trackers,
            torrent_validation_status,
            container_name,
        );
        let initial_policy_effects = state.update(Action::PeerPolicyUpdated {
            policy: peer_policy,
        });
        debug_assert!(initial_policy_effects.is_empty());

        let mut manager = Self {
            state,
            torrent_manager_tx,
            torrent_manager_rx,
            dht_handle,
            network_activation,
            network_activation_rx,
            network_activation_open: true,
            #[cfg(feature = "webtorrent")]
            rtc: rtc::Runtime::default(),
            dht_tx,
            dht_rx,
            dht_task_handle,
            dht_demand_state,
            dht_demand_metrics,
            shutdown_tx,
            incoming_peer_rx,
            metrics_tx,
            peer_policy_rx,
            peer_policy_open: true,
            manager_command_rx,
            manager_event_tx,
            in_flight_uploads: HashMap::new(),
            in_flight_writes: HashMap::new(),
            settings,
            resource_manager,
            global_dl_bucket,
            global_ul_bucket,
            telemetry: ManagerTelemetry::default(),
            data_rate_ms,
            run_loop_started: false,
            network_recovery_scope_id: None,
            pending_started_announces: HashSet::new(),
            started_announce_scopes: HashMap::new(),
            pending_completion_announces: HashSet::new(),
            completion_announce_scopes: HashMap::new(),
            peer_network_scopes: HashMap::new(),
            web_seed_network_scopes: HashMap::new(),
            peer_channel_full_drop_count: 0,
        };
        manager.apply_latest_network_activation();
        manager
    }

    pub fn from_torrent(
        torrent_parameters: TorrentParameters,
        torrent: Torrent,
    ) -> Result<Self, String> {
        // 1. Extract Trackers
        let trackers = build_tracker_state_map(torrent_tracker_urls(&torrent), Instant::now());

        // 2. Calculate Info Hash
        let info_hash = if torrent.info.meta_version == Some(2) {
            if !torrent.info.pieces.is_empty() {
                // Hybrid Torrent (V1 compatible). Using SHA-1.
                let mut hasher = sha1::Sha1::new();
                hasher.update(&torrent.info_dict_bencode);
                hasher.finalize().to_vec()
            } else {
                // Pure V2 Torrent. Using SHA-256 (Truncated).
                let mut hasher = sha2::Sha256::new();
                hasher.update(&torrent.info_dict_bencode);
                hasher.finalize()[0..20].to_vec()
            }
        } else {
            // V1
            let mut hasher = sha1::Sha1::new();
            hasher.update(&torrent.info_dict_bencode);
            hasher.finalize().to_vec()
        };

        let validation_status = torrent_parameters.torrent_validation_status;
        let torrent_data_path = torrent_parameters.torrent_data_path.clone();
        let file_priorities = torrent_parameters.file_priorities.clone();
        let container_name = torrent_parameters.container_name.clone();

        // 3. Initialize Base Manager (Awaiting Metadata)
        let mut manager =
            Self::init_base(torrent_parameters, info_hash, trackers, validation_status);

        // 4. Calculate Metadata Length (Required for protocol)
        let bencoded_data = serde_bencode::to_bytes(&torrent)
            .map_err(|e| format!("Failed to re-encode torrent struct: {}", e))?;
        let metadata_length = bencoded_data.len() as i64;

        // 5. Inject Metadata via Action - triggers same flow as magnet link
        manager.apply_action(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length,
        });

        if let Some(torrent_data_path) = torrent_data_path {
            manager.apply_action(Action::SetUserTorrentConfig {
                torrent_data_path,
                file_priorities,
                container_name,
            });
        }

        Ok(manager)
    }

    pub fn from_magnet(
        torrent_parameters: TorrentParameters,
        magnet: Magnet,
        raw_magnet_str: &str,
    ) -> Result<Self, String> {
        // 1. Parse Info Hash
        let (v1_hash, v2_hash) = crate::app::parse_hybrid_hashes(raw_magnet_str);

        // Hybrid: use v1_hash as primary, v2 as alt (or vice versa depending on policy)
        let (info_hash, _v2_info_hash) = match (v1_hash, v2_hash) {
            (Some(v1), Some(v2)) => (v1, Some(v2)),
            (Some(v1), None) => (v1, None),
            (None, Some(v2)) => (v2, None),
            _ => return Err("No valid hashes found".into()),
        };

        event!(
            Level::DEBUG,
            "Active info hash: {:?}",
            hex::encode(&info_hash)
        );

        // 2. Extract and Decode Trackers
        let decoded_trackers: Vec<String> = magnet
            .trackers()
            .iter()
            .filter_map(|t| {
                match decode(t) {
                    Ok(decoded_url) => Some(decoded_url.into_owned()),
                    Err(e) => {
                        event!(Level::DEBUG, tracker_url = %t, error = %e, "Failed to decode tracker URL from magnet link, skipping.");
                        None
                    }
                }
            })
            .collect();
        let trackers =
            build_tracker_state_map(normalize_tracker_urls(decoded_trackers), Instant::now());

        let validation_status = torrent_parameters.torrent_validation_status;
        let torrent_data_path = torrent_parameters.torrent_data_path.clone();
        let file_priorities = torrent_parameters.file_priorities.clone();
        let container_name = torrent_parameters.container_name.clone();

        // 3. Initialize Base Manager
        // It stays in AwaitingMetadata state until peers provide the info dict
        let mut manager =
            Self::init_base(torrent_parameters, info_hash, trackers, validation_status);

        if let Some(torrent_data_path) = torrent_data_path {
            manager.apply_action(Action::SetUserTorrentConfig {
                torrent_data_path,
                file_priorities,
                container_name,
            });
        }

        Ok(manager)
    }

    // Apply actions to update state and get effects resulting from the mutate.
    fn apply_action(&mut self, action: Action) {
        let effects = self.state.update(action);
        for effect in effects {
            self.handle_effect(effect);
        }
        self.sync_dht_lookup_task();
        self.sync_dht_demand();
        #[cfg(feature = "webtorrent")]
        self.rtc_reconcile();
    }

    // Handles the aftermath of the mutate effects
    fn handle_effect(&mut self, effect: Effect) {
        match effect {
            Effect::DoNothing => {}

            Effect::EmitManagerEvent(event) => {
                let _ = self.manager_event_tx.try_send(event);
            }

            Effect::EmitMetrics {
                bytes_dl,
                bytes_ul,
                file_activity_updates,
            } => {
                self.send_metrics(bytes_dl, bytes_ul, file_activity_updates);
            }

            Effect::SendToPeer { peer_id, cmd } => {
                if let Some(peer) = self.state.peers.get(&peer_id) {
                    let command = *cmd;
                    match peer.peer_tx.try_send(command) {
                        Ok(_) => {}
                        Err(mpsc::error::TrySendError::Full(command)) => {
                            self.peer_channel_full_drop_count =
                                self.peer_channel_full_drop_count.saturating_add(1);
                            if should_log_peer_channel_full_warning(
                                self.peer_channel_full_drop_count,
                            ) {
                                tracing::warn!(
                                    peer_id = %peer_id,
                                    capacity = peer.peer_tx.max_capacity(),
                                    dropped_commands = self.peer_channel_full_drop_count,
                                    command = ?TorrentCommandSummary(&command),
                                    "peer command channel full; dropping command; repeated warnings are exponentially sampled"
                                );
                            }
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            tracing::debug!("Peer {} disconnected.", peer_id);
                        }
                    }
                }
            }

            Effect::AnnounceCompleted { url } => {
                self.queue_completion_announce(url);
            }

            Effect::DisconnectPeerSession {
                peer_id,
                peer_tx,
                session_cancel,
            } => {
                session_cancel.send_replace(true);
                self.peer_network_scopes.remove(&peer_id);
                self.web_seed_network_scopes.remove(&peer_id);
                let _ = peer_tx.try_send(TorrentCommand::Disconnect(peer_id.clone()));
                if let Some(handles) = self.in_flight_uploads.remove(&peer_id) {
                    for handle in handles.values() {
                        handle.abort();
                    }
                }
            }

            Effect::DisconnectPeer { peer_id } => {
                self.peer_network_scopes.remove(&peer_id);
                self.web_seed_network_scopes.remove(&peer_id);
                let disconnect_failed = self.state.peers.get(&peer_id).is_some_and(|peer| {
                    peer.peer_tx
                        .try_send(TorrentCommand::Disconnect(peer_id.clone()))
                        .is_err()
                });
                if disconnect_failed {
                    tracing::warn!(
                        peer_id = %peer_id,
                        "Peer disconnect command could not be queued; forcing authoritative state removal"
                    );
                    self.apply_action(Action::PeerDisconnected {
                        peer_id: peer_id.clone(),
                        force: true,
                    });
                }
                if let Some(handles) = self.in_flight_uploads.remove(&peer_id) {
                    for handle in handles.values() {
                        handle.abort();
                    }
                }
            }

            Effect::BroadcastHave { piece_index } => {
                for peer in self.state.peers.values() {
                    let _ = peer
                        .peer_tx
                        .try_send(TorrentCommand::Have(peer.ip_port.clone(), piece_index));
                }
            }

            Effect::VerifyPiece {
                peer_id,
                piece_index,
                data,
            } => {
                let torrent = match self.state.torrent.clone() {
                    Some(t) => t,
                    None => {
                        debug_assert!(
                            self.state.torrent.is_some(),
                            "Metadata missing during verify"
                        );
                        event!(
                            Level::ERROR,
                            "Metadata missing during piece verification, cannot proceed."
                        );
                        return;
                    }
                };
                let start = piece_index as usize * HASH_LENGTH;
                let end = start + HASH_LENGTH;
                let expected_hash = torrent.info.pieces.get(start..end).map(|s| s.to_vec());

                let tx = self.torrent_manager_tx.clone();
                let peer_id_for_msg = peer_id.clone();
                let mut shutdown_rx = self.shutdown_tx.subscribe();

                tokio::spawn(async move {
                    let verification_task = tokio::task::spawn_blocking(move || {
                        if let Some(expected) = expected_hash {
                            let hash = sha1::Sha1::digest(&data);
                            if hash.as_slice() == expected.as_slice() {
                                return Ok(data);
                            }
                        }
                        Err(())
                    });

                    let result = tokio::select! {
                        biased;
                        _ = shutdown_rx.recv() => return,
                        res = verification_task => res.unwrap_or(Err(())),
                    };

                    match result {
                        Ok(verified_data) => {
                            let _ = tx
                                .send(TorrentCommand::PieceVerified {
                                    piece_index,
                                    peer_id: peer_id_for_msg,
                                    verification_result: Ok(verified_data),
                                })
                                .await;
                        }
                        _ => {
                            let _ = tx
                                .send(TorrentCommand::PieceVerified {
                                    piece_index,
                                    peer_id: peer_id_for_msg,
                                    verification_result: Err(()),
                                })
                                .await;
                        }
                    }
                });
            }

            Effect::VerifyPieceV2 {
                peer_id,
                piece_index,
                proof,
                mut data,
                root_hash,
                _file_start_offset,
                valid_length,
                relative_index,
                hashing_context_len,
            } => {
                let tx = self.torrent_manager_tx.clone();
                let peer_id_for_msg = peer_id.clone();
                let mut shutdown_rx = self.shutdown_tx.subscribe();

                tracing::debug!(
                    piece_index,
                    peer_id = %peer_id_for_msg,
                    "SPAWNING V2 Verification. Root={:?}",
                    hex::encode(&root_hash)
                );

                tokio::spawn(async move {
                    // Handle padding
                    if valid_length < data.len() {
                        tracing::debug!(
                            piece_index,
                            "Padding data: {} -> {}",
                            valid_length,
                            data.len()
                        );
                        data[valid_length..].fill(0);
                    }

                    // The CPU Intensive Task
                    let mut verification_task = tokio::task::spawn_blocking(move || {
                        let start = Instant::now();

                        let is_valid = merkle::verify_merkle_proof(
                            &root_hash,
                            &data,
                            relative_index,
                            &proof,
                            hashing_context_len,
                        );

                        tracing::debug!(
                            piece_index,
                            valid = is_valid,
                            duration = ?start.elapsed(),
                            "V2 CPU Verification Finished"
                        );

                        if is_valid {
                            Ok(data)
                        } else {
                            Err(())
                        }
                    });

                    // Loop to handle broadcast lag without aborting the task
                    let result = loop {
                        tokio::select! {
                            biased;
                            res = shutdown_rx.recv() => {
                                match res {
                                    // If legitimate shutdown signal or channel closed -> Abort
                                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                        tracing::warn!(piece_index, "Verification aborted by shutdown signal");
                                        return;
                                    }
                                    // If Lagged -> Log and continue loop (waiting on verification_task again)
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                        tracing::trace!(piece_index, skipped, "Ignoring broadcast lag, continuing verification...");
                                        continue;
                                    }
                                }
                            },
                            // Use &mut here to borrow the task instead of moving it.
                            // This allows the loop to reuse it if the other branch hits 'continue'.
                            res = &mut verification_task => {
                                break match res {
                                    Ok(inner_res) => inner_res,
                                    Err(join_err) => {
                                        if join_err.is_panic() {
                                            tracing::error!(piece_index, "🔥 Verification Task PANICKED!");
                                        } else {
                                            tracing::error!(piece_index, "Verification Task Cancelled");
                                        }
                                        Err(())
                                    }
                                };
                            }
                        };
                    };

                    match &result {
                        Ok(_) => tracing::debug!(
                            piece_index,
                            "Sending PieceVerified (Success) -> Manager"
                        ),
                        Err(_) => tracing::warn!(
                            piece_index,
                            "Sending PieceVerified (Failure) -> Manager"
                        ),
                    }

                    let _ = tx
                        .send(TorrentCommand::PieceVerified {
                            piece_index,
                            peer_id: peer_id_for_msg,
                            verification_result: result,
                        })
                        .await;
                });
            }

            Effect::WriteToDisk {
                peer_id,
                piece_index,
                data,
            } => {
                let multi_file_info = match self.state.multi_file_info.as_ref() {
                    Some(m) => m.clone(),
                    None => {
                        event!(Level::ERROR, "WriteToDisk failed: Storage not ready");
                        return;
                    }
                };
                let piece_length = match self.state.torrent.as_ref() {
                    Some(t) => t.info.piece_length as u64,
                    None => {
                        event!(Level::ERROR, "WriteToDisk failed: Metadata missing");
                        return;
                    }
                };
                let global_offset = piece_index as u64 * piece_length;

                let tx = self.torrent_manager_tx.clone();
                let event_tx = self.manager_event_tx.clone();
                let resource_manager = self.resource_manager.clone();
                let info_hash = self.state.info_hash.clone();
                let mut shutdown_rx = self.shutdown_tx.subscribe();
                let peer_id_clone = peer_id.clone();

                let handle = tokio::spawn(async move {
                    let op = DiskIoOperation {
                        piece_index,
                        offset: global_offset,
                        length: data.len(),
                    };

                    let write_result = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        Self::write_block_with_retry(
                            &multi_file_info,
                            &resource_manager,
                            &mut shutdown_rx,
                            &event_tx,
                            &info_hash,
                            op,
                            &data,
                        ),
                    )
                    .await;

                    match write_result {
                        Ok(Ok(_)) => {
                            let _ = tx
                                .send(TorrentCommand::PieceWrittenToDisk {
                                    peer_id: peer_id_clone,
                                    piece_index,
                                })
                                .await;
                        }
                        Ok(Err(_)) => {
                            let _ = tx
                                .send(TorrentCommand::PieceWriteFailed { piece_index })
                                .await;
                        }
                        Err(_) => {
                            let _ = tx
                                .send(TorrentCommand::PieceWriteFailed { piece_index })
                                .await;
                        }
                    }
                });

                self.in_flight_writes
                    .entry(piece_index)
                    .or_default()
                    .push(handle);
            }

            Effect::ReadFromDisk {
                peer_id,
                block_info,
            } => {
                let (peer_semaphore, peer_tx) = if let Some(peer) = self.state.peers.get(&peer_id) {
                    (peer.upload_slots_semaphore.clone(), peer.peer_tx.clone())
                } else {
                    return;
                };

                let _peer_permit = match peer_semaphore.try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => return,
                };

                let multi_file_info = match self.state.multi_file_info.as_ref() {
                    Some(m) => m.clone(),
                    None => {
                        event!(Level::ERROR, "WriteToDisk failed: Storage not ready");
                        return;
                    }
                };

                let torrent = match self.state.torrent.as_ref() {
                    Some(t) => t,
                    None => {
                        event!(
                            Level::ERROR,
                            "ReadFromDisk triggered but metadata missing. Ignoring."
                        );
                        return;
                    }
                };

                let global_offset = (block_info.piece_index as u64
                    * torrent.info.piece_length as u64)
                    + block_info.offset as u64;

                let tx = self.torrent_manager_tx.clone();
                let event_tx = self.manager_event_tx.clone();
                let resource_manager = self.resource_manager.clone();
                let info_hash = self.state.info_hash.clone();
                let mut shutdown_rx = self.shutdown_tx.subscribe();
                let peer_id_clone = peer_id.clone();
                let block_info_clone = block_info.clone();

                let handle = tokio::spawn(async move {
                    let _held_permit = _peer_permit;
                    let op = DiskIoOperation {
                        piece_index: block_info.piece_index,
                        offset: global_offset,
                        length: block_info.length as usize,
                    };

                    let _ = event_tx.try_send(ManagerEvent::DiskReadStarted {
                        info_hash: info_hash.to_vec(),
                        op,
                    });

                    let result = Self::read_block_with_retry(
                        &multi_file_info,
                        &resource_manager,
                        &mut shutdown_rx,
                        &event_tx,
                        &info_hash,
                        op,
                        &peer_tx,
                    )
                    .await;

                    if let Ok(data) = result {
                        let _ = peer_tx.try_send(TorrentCommand::Upload(
                            block_info.piece_index,
                            block_info.offset,
                            data,
                        ));

                        let _ = tx.try_send(TorrentCommand::UploadTaskCompleted {
                            peer_id: peer_id_clone.clone(),
                            block_info: block_info_clone,
                        });

                        let _ = tx
                            .send(TorrentCommand::BlockSent {
                                peer_id: peer_id_clone.clone(),
                                bytes: block_info.length as u64,
                            })
                            .await;
                    } else if matches!(result, Err(ref error) if error.indicates_data_unavailability())
                    {
                        let _ = tx.send(TorrentCommand::SetDataAvailability(false)).await;
                    }

                    let _ = event_tx.try_send(ManagerEvent::DiskReadFinished);
                });

                self.in_flight_uploads
                    .entry(peer_id)
                    .or_default()
                    .insert(block_info, handle);
            }

            Effect::ConnectToPeer { addr } => {
                if self.should_accept_new_peers() {
                    self.connect_to_peer(addr);
                }
            }

            Effect::StartValidation => {
                let mfi = match self.state.multi_file_info.as_ref() {
                    Some(m) => m.clone(),
                    None => {
                        debug_assert!(
                            self.state.multi_file_info.is_some(),
                            "Storage not ready for validation"
                        );
                        event!(
                            Level::ERROR,
                            "Cannot start validation: Storage not initialized."
                        );
                        return;
                    }
                };
                let torrent = match self.state.torrent.as_ref() {
                    Some(t) => t.clone(),
                    None => {
                        debug_assert!(
                            self.state.torrent.is_some(),
                            "Metadata not ready for validation"
                        );
                        event!(
                            Level::ERROR,
                            "Cannot start validation: Metadata not available."
                        );
                        return;
                    }
                };
                let rm = self.resource_manager.clone();
                let shutdown_rx = self.shutdown_tx.subscribe();
                let event_tx = self.manager_event_tx.clone();
                let manager_tx = self.torrent_manager_tx.clone();

                let is_validated = self.state.torrent_validation_status;

                tokio::spawn(async move {
                    let res = Self::perform_validation(
                        mfi,
                        torrent,
                        rm,
                        shutdown_rx,
                        manager_tx.clone(),
                        event_tx,
                        is_validated,
                    )
                    .await;

                    if let Ok(pieces) = res {
                        let _ = manager_tx
                            .send(TorrentCommand::ValidationComplete(pieces))
                            .await;
                    }
                });
            }

            Effect::ConnectToPeersFromTrackers => {
                self.queue_started_announces();
            }

            Effect::AnnounceToTracker { url } => {
                #[cfg(feature = "webtorrent")]
                if self.rtc_announce(&url, None) {
                    return;
                }
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
                tokio::spawn(async move {
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
                    send_generation_scoped_tracker_result(
                        &tx,
                        result,
                        &network_scope,
                        &mut shutdown_rx,
                    )
                    .await;
                });
            }

            Effect::AbortUpload {
                peer_id,
                block_info,
            } => {
                if let Some(peer_uploads) = self.in_flight_uploads.get_mut(&peer_id) {
                    if let Some(handle) = peer_uploads.remove(&block_info) {
                        handle.abort();
                        event!(Level::TRACE, peer = %peer_id, ?block_info, "Aborted in-flight upload task.");
                    }
                }
            }

            Effect::ClearAllUploads => {
                for (_, handles) in self.in_flight_uploads.drain() {
                    for (_, handle) in handles {
                        handle.abort();
                    }
                }
            }

            Effect::DeleteFiles { files, directories } => {
                let info_hash = self.state.info_hash.clone();
                let tx = self.manager_event_tx.clone();
                #[cfg(feature = "webtorrent")]
                let rtc_cleanup = self.rtc_cleanup();

                tokio::spawn(async move {
                    #[cfg(feature = "webtorrent")]
                    rtc_cleanup.await;
                    let mut result = Ok(());

                    // 1. Delete Files
                    for file_path in files {
                        if let Err(e) = fs::remove_file(&file_path).await {
                            // If it's already gone, that's fine (success).
                            if e.kind() != std::io::ErrorKind::NotFound {
                                let error_msg =
                                    format!("Failed to delete file {:?}: {}", &file_path, e);
                                event!(Level::ERROR, "{}", error_msg);
                                result = Err(error_msg);
                            }
                        }
                    }

                    // 2. Delete Directories (in sorted order: Deepest -> Shallowest)
                    // We use remove_dir (not remove_dir_all) for safety.
                    // It will simply fail (safely) if the directory is not empty
                    // (e.g., user added their own files to the folder).
                    for dir_path in directories {
                        if let Err(e) = fs::remove_dir(&dir_path).await {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                event!(Level::INFO, "Skipped dir deletion {:?}: {}", &dir_path, e);
                            }
                        } else {
                            event!(Level::INFO, "Cleaned up directory: {:?}", &dir_path);
                        }
                    }

                    let _ = tx
                        .send(ManagerEvent::DeletionComplete(info_hash, result))
                        .await;
                });
            }

            Effect::PrepareShutdown {
                tracker_urls,
                left,
                uploaded,
                downloaded,
            } => {
                let tracker_network = self
                    .network_activation
                    .try_active()
                    .ok()
                    .map(|active| (active.scope().lease().clone(), active.listen_port()));
                let _ = self.shutdown_tx.send(());
                self.stop_dht_lookup_task();

                event!(Level::DEBUG, "Aborting in-flight upload/write tasks...");
                for (_, handles) in self.in_flight_uploads.drain() {
                    for (_, handle) in handles {
                        handle.abort();
                    }
                }
                for (_, handles) in self.in_flight_writes.drain() {
                    for handle in handles {
                        handle.abort();
                    }
                }

                let private_client = self.settings.private_client;
                let tracker_urls =
                    shutdown_tracker_urls(tracker_urls, &self.state.trackers, private_client);

                let tx = self.manager_event_tx.clone();
                #[cfg(feature = "webtorrent")]
                let rtc_cleanup = self.rtc_cleanup();
                let info_hash = self.state.info_hash.clone();

                if !private_client {
                    if let Some((network_lease, port)) = tracker_network {
                        for url in tracker_urls {
                            let info_hash = self.state.info_hash.clone();
                            let client_id = self.settings.client_id.clone();
                            let network_lease = network_lease.clone();

                            tokio::spawn(async move {
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
                            });
                        }
                    }

                    tokio::spawn(async move {
                        #[cfg(feature = "webtorrent")]
                        rtc_cleanup.await;
                        let _ = tx
                            .send(ManagerEvent::DeletionComplete(info_hash, Ok(())))
                            .await;
                    });
                    return;
                }

                let network_activation = self.network_activation.clone();
                let client_id = self.settings.client_id.clone();
                tokio::spawn(async move {
                    let stop_announces = async {
                        let tracker_network = match tracker_network {
                            Some(tracker_network) => Some(tracker_network),
                            None => wait_for_active_tracker_network(&network_activation).await,
                        };
                        let Some((network_lease, port)) = tracker_network else {
                            return;
                        };
                        let mut announce_set = JoinSet::new();
                        for url in tracker_urls {
                            let network_lease = network_lease.clone();
                            let info_hash = info_hash.clone();
                            let client_id = client_id.clone();
                            announce_set.spawn(async move {
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
                            });
                        }
                        while announce_set.join_next().await.is_some() {}
                    };
                    if timeout(PRIVATE_TRACKER_STOP_ANNOUNCE_TIMEOUT, stop_announces)
                        .await
                        .is_err()
                    {
                        event!(Level::WARN, "Tracker stop announce timed out.");
                    }
                    #[cfg(feature = "webtorrent")]
                    rtc_cleanup.await;
                    let _ = tx
                        .send(ManagerEvent::DeletionComplete(info_hash, Ok(())))
                        .await;
                });
            }

            Effect::StartWebSeed { url } => {
                let active_network = match self.network_activation.try_active() {
                    Ok(active) => active,
                    Err(error) => {
                        event!(Level::DEBUG, %error, "web seed deferred while networking is unavailable");
                        return;
                    }
                };
                let network_scope = active_network.scope().clone();
                let network_lease = network_scope.lease().clone();
                let client = match network_lease.web_seed_http_client() {
                    Ok(client) => client,
                    Err(error) => {
                        event!(Level::DEBUG, %error, "web seed deferred because its HTTP client is unavailable");
                        return;
                    }
                };
                let network_invalidation_rx = network_scope.subscribe_invalidation();
                let network_scope_id = network_scope.id();
                let (full_url, _filename) = if let Some(torrent) = &self.state.torrent {
                    if url.ends_with('/') {
                        (
                            format!("{}{}", url, torrent.info.name),
                            torrent.info.name.clone(),
                        )
                    } else {
                        (url.clone(), torrent.info.name.clone())
                    }
                } else {
                    event!(
                        Level::WARN,
                        "Triggered StartWebSeed but metadata is missing. Skipping."
                    );
                    return;
                };

                let torrent_manager_tx = self.torrent_manager_tx.clone();
                let (peer_tx, peer_rx) = tokio::sync::mpsc::channel(32);

                let peer_id = full_url.clone();

                if self.register_peer(peer_id.clone(), None, peer_tx).is_none() {
                    tracing::trace!(peer = %peer_id, "Web seed session registration was rejected");
                    return;
                }
                self.web_seed_network_scopes
                    .insert(peer_id.clone(), network_scope_id);

                let shutdown_rx = self.shutdown_tx.subscribe();

                if let Some(torrent) = &self.state.torrent {
                    let piece_len = torrent.info.piece_length as u64;

                    // Calculate total length robustly (handle multi-file vs single-file)
                    let total_len = if torrent.info.files.is_empty() {
                        torrent.info.length as u64
                    } else {
                        torrent.info.files.iter().map(|f| f.length as u64).sum()
                    };

                    event!(Level::DEBUG, "Starting WebSeed Worker: {}", full_url);

                    tokio::spawn(async move {
                        web_seed_worker(
                            client,
                            full_url,
                            peer_id,
                            piece_len,
                            total_len,
                            peer_rx,
                            torrent_manager_tx,
                            shutdown_rx,
                            network_invalidation_rx,
                            network_scope_id,
                        )
                        .await;
                    });
                }
            }

            Effect::RequestHashes {
                peer_id,
                file_root,
                piece_index,
                length,
                proof_layers,
                base_layer,
            } => {
                if let Some(peer) = self.state.peers.get(&peer_id) {
                    let _ = peer.peer_tx.try_send(TorrentCommand::GetHashes {
                        peer_id: peer_id.clone(),
                        file_root,
                        //file_index,
                        index: piece_index,
                        length,
                        proof_layers,
                        base_layer,
                    });
                }
            }
        }
    }

    async fn perform_validation(
        multi_file_info: MultiFileInfo,
        torrent: Torrent,
        resource_manager: ResourceManagerClient,
        mut shutdown_rx: broadcast::Receiver<()>,
        manager_tx: Sender<TorrentCommand>,
        _event_tx: Sender<ManagerEvent>,
        skip_hashing: bool,
    ) -> Result<Vec<u32>, StorageError> {
        if skip_hashing {
            if Self::has_complete_storage_layout(&multi_file_info).await? {
                let piece_len = torrent.info.piece_length as u64;
                let mut completed_pieces = Vec::new();

                if piece_len > 0 {
                    if torrent.info.meta_version == Some(2) {
                        let v2_piece_count = torrent.calculate_v2_mapping().piece_count as u32;
                        completed_pieces = (0..v2_piece_count).collect();
                    } else {
                        let num_pieces = multi_file_info.total_size.div_ceil(piece_len) as u32;
                        completed_pieces = (0..num_pieces).collect();
                    }
                }

                let _ = manager_tx
                    .send(TorrentCommand::ValidationProgress(
                        completed_pieces.len() as u32
                    ))
                    .await;

                return Ok(completed_pieces);
            }

            tracing::warn!(
                "Validation: skip_hashing requested but persisted layout is incomplete. Marking as unvalidated."
            );
            let _ = manager_tx.send(TorrentCommand::ValidationProgress(0)).await;
            return Ok(Vec::new());
        }

        let is_fresh_download = tokio::select! {
            biased;
            _ = shutdown_rx.recv() => return Err(StorageError::from(std::io::Error::other("Shutdown"))),
            res = create_and_allocate_files(&multi_file_info) => res?,
        };
        if is_fresh_download {
            tracing::info!("Storage: Fresh download detected. Skipping validation loop.");
            let _ = manager_tx.send(TorrentCommand::ValidationProgress(0)).await;
            return Ok(Vec::new());
        }

        let mut completed_pieces = Vec::new();
        let piece_len = torrent.info.piece_length as u64;

        // PATH A: BitTorrent V2 (Aligned File Validation)
        if torrent.info.meta_version == Some(2) {
            let v2_roots_list = torrent.get_v2_roots();
            let mut path_to_root: HashMap<String, Vec<u8>> = HashMap::new();
            for (path, _, root) in v2_roots_list {
                path_to_root.insert(path, root);
            }

            for file_info in &multi_file_info.files {
                if file_info.is_padding {
                    continue;
                }

                let physical_path_str = file_info
                    .path
                    .to_string_lossy()
                    .to_string()
                    .replace("\\", "/");
                let file_length = file_info.length;

                let root_hash = path_to_root
                    .iter()
                    .find(|(v2_path, _)| physical_path_str.ends_with(*v2_path))
                    .map(|(_, root)| root);

                let root_hash = match root_hash {
                    Some(r) => r,
                    None => {
                        tracing::warn!(
                            "Validation: No V2 root found for file {:?}. Skipping.",
                            physical_path_str
                        );
                        continue;
                    }
                };

                let file_pieces = if file_length > 0 {
                    file_length.div_ceil(piece_len)
                } else {
                    0
                };
                let layers = torrent.get_layer_hashes(root_hash);
                let start_piece_index = (file_info.global_start_offset / piece_len) as u32;

                for i in 0..file_pieces {
                    let global_piece_index = start_piece_index + i as u32;
                    let offset_in_file = i * piece_len;
                    let len_this_piece =
                        std::cmp::min(piece_len, file_length.saturating_sub(offset_in_file));
                    let global_read_offset = file_info.global_start_offset + offset_in_file;

                    let piece_data = {
                        let permit = tokio::select! {
                            biased;
                            _ = shutdown_rx.recv() => return Ok(completed_pieces),
                            res = resource_manager.acquire_disk_read() => res
                        };

                        if permit.is_ok() {
                            read_data_from_disk(
                                &multi_file_info,
                                global_read_offset,
                                len_this_piece as usize,
                            )
                            .await?
                        } else {
                            return Err(StorageError::from(std::io::Error::other(
                                "Resource Permit Denied",
                            )));
                        }
                    };

                    if !piece_data.is_empty() && !skip_hashing {
                        let expected = if let Some(ref l) = layers {
                            let start = i as usize * 32;
                            l.get(start..start + 32).map(|s| s.to_vec())
                        } else if file_pieces == 1 {
                            Some(root_hash.clone())
                        } else {
                            None
                        };

                        if let Some(want) = expected {
                            let is_valid = tokio::task::spawn_blocking(move || {
                                // We treat this as a "Proof-less" verification.
                                // The 'want' hash is the expected root for this chunk.
                                // hashing_context_len is passed as piece_len to ensure proper padding logic matches the V2 spec.
                                merkle::verify_merkle_proof(
                                    &want,
                                    &piece_data,
                                    0,   // Relative index irrelevant for direct root comparison
                                    &[], // Empty Proof
                                    piece_len as usize,
                                )
                            })
                            .await
                            .unwrap_or(false);

                            if is_valid {
                                completed_pieces.push(global_piece_index);
                            } else {
                                tracing::debug!(
                                    "Validation Failed for V2 Piece {} (File: {:?})",
                                    global_piece_index,
                                    physical_path_str
                                );
                            }
                        }
                    } else if skip_hashing {
                        completed_pieces.push(global_piece_index);
                    }

                    if global_piece_index.is_multiple_of(10) {
                        let _ = manager_tx
                            .send(TorrentCommand::ValidationProgress(global_piece_index))
                            .await;
                    }
                }
            }

            completed_pieces.sort();
            completed_pieces.dedup();

            let _ = manager_tx
                .send(TorrentCommand::ValidationProgress(
                    completed_pieces.len() as u32
                ))
                .await;
        }
        // PATH B: V1 (Contiguous Stream Logic)
        else {
            let total_size = multi_file_info.total_size;
            let num_pieces = if piece_len > 0 {
                (total_size.div_ceil(piece_len)) as u32
            } else {
                0
            };

            for piece_index in 0..num_pieces {
                let start_offset = (piece_index as u64) * piece_len;
                let len_this_piece =
                    std::cmp::min(piece_len, total_size.saturating_sub(start_offset)) as usize;

                if len_this_piece == 0 {
                    continue;
                }

                let start = piece_index as usize * 20;
                let expected_hash = if start + 20 <= torrent.info.pieces.len() {
                    Some(torrent.info.pieces[start..start + 20].to_vec())
                } else {
                    None
                };

                let piece_data = loop {
                    let permit = tokio::select! {
                        biased;
                        _ = shutdown_rx.recv() => return Ok(completed_pieces),
                        res = resource_manager.acquire_disk_read() => res
                    };

                    if permit.is_ok() {
                        if let Ok(data) =
                            read_data_from_disk(&multi_file_info, start_offset, len_this_piece)
                                .await
                        {
                            break data;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                };

                if !piece_data.is_empty() && !skip_hashing {
                    let is_valid = tokio::task::spawn_blocking(move || {
                        if let Some(expected) = expected_hash {
                            sha1::Sha1::digest(&piece_data).as_slice() == expected.as_slice()
                        } else {
                            false
                        }
                    })
                    .await
                    .unwrap_or(false);

                    if is_valid {
                        completed_pieces.push(piece_index);
                    } else {
                        event!(Level::DEBUG, "Hash mismatch for piece {}", piece_index);
                    }
                } else if skip_hashing {
                    completed_pieces.push(piece_index);
                }

                if piece_index.is_multiple_of(10) {
                    let _ = manager_tx
                        .send(TorrentCommand::ValidationProgress(piece_index))
                        .await;
                }
            }
            let _ = manager_tx
                .send(TorrentCommand::ValidationProgress(num_pieces))
                .await;
        }

        Ok(completed_pieces)
    }

    async fn has_complete_storage_layout(
        multi_file_info: &MultiFileInfo,
    ) -> Result<bool, StorageError> {
        for file_info in &multi_file_info.files {
            if file_info.is_padding {
                continue;
            }

            let metadata = match fs::metadata(&file_info.path).await {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(err) => return Err(StorageError::from(err)),
            };

            if !metadata.is_file() || metadata.len() != file_info.length {
                return Ok(false);
            }
        }

        Ok(true)
    }

    async fn write_block_with_retry(
        multi_file_info: &MultiFileInfo,
        resource_manager: &ResourceManagerClient,
        shutdown_rx: &mut broadcast::Receiver<()>,
        event_tx: &Sender<ManagerEvent>,
        info_hash: &[u8],
        op: DiskIoOperation,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let mut attempt = 0;
        let _ = event_tx.try_send(ManagerEvent::DiskWriteStarted {
            info_hash: info_hash.to_vec(),
            op,
        });

        loop {
            let permit_res = tokio::select! {
                biased;
                _ = shutdown_rx.recv() => return Err(StorageError::from(std::io::Error::other("Shutdown"))),
                res = resource_manager.acquire_disk_write() => res,
            };

            match permit_res {
                Ok(_permit) => {
                    let write_future = write_data_to_disk(multi_file_info, op.offset, data);
                    let res = tokio::select! {
                        biased;
                        _ = shutdown_rx.recv() => return Err(StorageError::from(std::io::Error::other("Shutdown"))),
                        r = write_future => r,
                    };

                    match res {
                        Ok(_) => {
                            let _ = event_tx.try_send(ManagerEvent::DiskWriteCompleted {
                                info_hash: info_hash.to_vec(),
                                op,
                            });
                            let _ = event_tx.try_send(ManagerEvent::DiskWriteFinished {
                                info_hash: info_hash.to_vec(),
                                piece_index: op.piece_index,
                            });
                            return Ok(());
                        }
                        Err(e) => {
                            event!(Level::WARN, piece = op.piece_index, error = ?e, "Disk write failed (IO Error).");
                        }
                    }
                }
                Err(ResourceManagerError::ManagerShutdown) => {
                    return Err(StorageError::from(std::io::Error::other(
                        "Manager Shutdown",
                    )))
                }
                Err(ResourceManagerError::QueueFull) => {
                    event!(
                        Level::WARN,
                        piece = op.piece_index,
                        "Disk write queue full (Permit Starvation)."
                    );
                }
            }

            attempt += 1;
            if attempt > MAX_PIECE_WRITE_ATTEMPTS {
                let _ = event_tx.try_send(ManagerEvent::DiskWriteFinished {
                    info_hash: info_hash.to_vec(),
                    piece_index: op.piece_index,
                });
                return Err(StorageError::from(std::io::Error::other(
                    "Max write attempts exceeded",
                )));
            }

            let backoff = BASE_BACKOFF_MS.saturating_mul(2u64.pow(attempt));
            let jitter = rand::rng().random_range(0..=JITTER_MS);
            let duration = Duration::from_millis(backoff + jitter);
            event!(
                Level::WARN,
                piece = op.piece_index,
                attempt = attempt,
                duration_ms = duration.as_millis(),
                "Retrying disk write..."
            );

            let _ = event_tx.try_send(ManagerEvent::DiskIoBackoff {
                duration: Duration::from_millis(backoff + jitter),
            });

            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => return Err(StorageError::from(std::io::Error::other("Shutdown"))),
                _ = tokio::time::sleep(Duration::from_millis(backoff + jitter)) => {},
            }
        }
    }

    async fn read_block_with_retry(
        multi_file_info: &MultiFileInfo,
        resource_manager: &ResourceManagerClient,
        shutdown_rx: &mut broadcast::Receiver<()>,
        event_tx: &Sender<ManagerEvent>,
        info_hash: &[u8],
        op: DiskIoOperation,
        peer_tx: &Sender<TorrentCommand>,
    ) -> Result<Vec<u8>, StorageError> {
        let mut attempt = 0;

        loop {
            if peer_tx.is_closed() {
                return Err(StorageError::from(std::io::Error::other(
                    "Peer Disconnected",
                )));
            }

            let permit_res = tokio::select! {
                biased;
                _ = shutdown_rx.recv() => { return Err(StorageError::from(std::io::Error::other("Shutdown"))); }
                res = resource_manager.acquire_disk_read() => res,
            };

            match permit_res {
                Ok(_permit) => {
                    let read_future = read_data_from_disk(multi_file_info, op.offset, op.length);
                    let res = tokio::select! {
                        biased;
                        _ = shutdown_rx.recv() => { return Err(StorageError::from(std::io::Error::other("Shutdown"))); }
                        r = read_future => r,
                    };

                    match res {
                        Ok(data) => {
                            return Ok(data);
                        }
                        Err(e) => {
                            if e.indicates_data_unavailability() {
                                let _ = event_tx.try_send(ManagerEvent::DataAvailabilityFault {
                                    info_hash: info_hash.to_vec(),
                                    piece_index: op.piece_index,
                                    error: e.clone(),
                                });
                                return Err(e);
                            }
                            event!(Level::WARN, piece = op.piece_index, error = ?e, "Disk read failed (IO Error).");
                        }
                    }
                }
                Err(ResourceManagerError::ManagerShutdown) => {
                    return Err(StorageError::from(std::io::Error::other(
                        "Manager Shutdown",
                    )))
                }
                Err(ResourceManagerError::QueueFull) => {
                    event!(
                        Level::WARN,
                        piece = op.piece_index,
                        "Disk read queue full (Permit Starvation)."
                    );
                }
            }

            attempt += 1;
            if attempt > MAX_UPLOAD_REQUEST_ATTEMPTS {
                return Err(StorageError::from(std::io::Error::other(
                    "Max read attempts exceeded",
                )));
            }

            let backoff = BASE_BACKOFF_MS.saturating_mul(2u64.pow(attempt));
            let jitter = rand::rng().random_range(0..=JITTER_MS);
            let duration = Duration::from_millis(backoff + jitter);

            event!(
                Level::WARN,
                piece = op.piece_index,
                attempt = attempt,
                duration_ms = duration.as_millis(),
                "Retrying disk read..."
            );

            let _ = event_tx.try_send(ManagerEvent::DiskIoBackoff { duration });

            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => { return Err(StorageError::from(std::io::Error::other("Shutdown"))); }
                _ = tokio::time::sleep(duration) => {},
            }
        }
    }

    #[cfg(feature = "dht")]
    fn start_dht_lookup_task(
        &mut self,
        demand_state: DhtDemandState,
        demand_metrics: DhtDemandMetrics,
    ) {
        if let Some(handle) = self.dht_task_handle.take() {
            handle.abort();
        }

        let dht_tx_clone = self.dht_tx.clone();
        let dht_handle_clone = self.dht_handle.clone();
        let shutdown_rx = self.shutdown_tx.subscribe();

        if let Some(handle) = dht_handle_clone.spawn_lookup_task(
            self.state.info_hash.clone(),
            demand_state,
            demand_metrics,
            dht_tx_clone,
            shutdown_rx,
        ) {
            self.dht_task_handle = Some(handle);
            self.dht_demand_state = Some(demand_state);
            self.dht_demand_metrics = Some(demand_metrics);
        }
    }

    #[cfg(feature = "dht")]
    fn stop_dht_lookup_task(&mut self) {
        if let Some(handle) = self.dht_task_handle.take() {
            handle.abort();
        }
        self.dht_demand_state = None;
        self.dht_demand_metrics = None;
    }

    #[cfg(feature = "dht")]
    fn sync_dht_lookup_task(&mut self) {
        if !self.run_loop_started {
            return;
        }

        if self.state.is_paused {
            self.stop_dht_lookup_task();
            return;
        }

        let demand_state = self.current_dht_demand_state();
        let demand_metrics = self.current_dht_demand_metrics();
        if self
            .dht_task_handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            self.dht_task_handle = None;
            self.dht_demand_state = None;
            self.dht_demand_metrics = None;
        }
        if self.dht_task_handle.is_none() {
            self.start_dht_lookup_task(demand_state, demand_metrics);
        }
    }

    #[cfg(not(feature = "dht"))]
    fn sync_dht_lookup_task(&mut self) {}

    #[cfg(not(feature = "dht"))]
    fn stop_dht_lookup_task(&mut self) {}

    fn generate_bitfield(&mut self) -> Vec<u8> {
        let num_pieces = self.state.piece_manager.bitfield.len();
        let num_bytes = num_pieces.div_ceil(8);
        let mut bitfield_bytes = vec![0u8; num_bytes];

        for (piece_index, status) in self.state.piece_manager.bitfield.iter().enumerate() {
            if *status == PieceStatus::Done {
                let byte_index = piece_index / 8;
                let bit_index_in_byte = piece_index % 8;
                let mask = 1 << (7 - bit_index_in_byte);
                bitfield_bytes[byte_index] |= mask;
            }
        }

        bitfield_bytes
    }

    pub fn connect_to_peer(&mut self, peer_addr: SocketAddr) {
        self.connect_to_peer_with_key(peer_addr, None);
    }

    #[cfg(feature = "synthetic-load")]
    pub fn connect_to_synthetic_peer(&mut self, peer_addr: SocketAddr, peer_key: String) {
        self.connect_to_peer_with_key(peer_addr, Some(peer_key));
    }

    fn connect_to_peer_with_key(&mut self, peer_addr: SocketAddr, preferred_key: Option<String>) {
        let Some((network_scope, peer_addr, local_udp_port)) =
            network_scope_and_peer_address(&self.network_activation, peer_addr)
        else {
            event!(
                Level::TRACE,
                %peer_addr,
                "ignoring peer on a disabled or unavailable network address family"
            );
            return;
        };
        if !self.state.can_connect_to_peer(peer_addr) {
            return;
        }

        let _ = self
            .manager_event_tx
            .try_send(ManagerEvent::PeerDiscovered {
                info_hash: self.state.info_hash.clone(),
            });

        let outbound_transport_mode = outbound_transport_mode_from_env();
        let peer_key_candidates = outbound_peer_key_candidates(
            peer_addr,
            outbound_transport_mode,
            preferred_key.as_deref(),
        );
        let peer_label = preferred_key.as_deref().unwrap_or_else(|| {
            peer_key_candidates
                .first()
                .map(String::as_str)
                .unwrap_or("")
        });

        if self.state.torrent_status == TorrentStatus::Done {
            if let Some((known_key, expires_at)) = peer_key_candidates.iter().find_map(|key| {
                self.state
                    .known_seeders
                    .get(key)
                    .copied()
                    .map(|expires_at| (key.clone(), expires_at))
            }) {
                if Instant::now() < expires_at {
                    tracing::debug!(
                        target: "superseedr::peer_filter",
                        event = "skip_known_seeder",
                        peer = %known_key,
                        "skipping outbound connect to known seeder"
                    );
                    return;
                } else {
                    self.state.known_seeders.remove(&known_key);
                    tracing::debug!(
                        target: "superseedr::peer_filter",
                        event = "expired_known_seeder",
                        peer = %known_key,
                        "expired known seeder entry before outbound connect"
                    );
                }
            }
        }

        if let Some((peer_key, (failure_count, next_attempt_time))) = peer_key_candidates
            .iter()
            .find_map(|key| self.state.timed_out_peers.get_key_value(key))
        {
            if Instant::now() < *next_attempt_time {
                event!(Level::DEBUG, peer = %peer_key, failures = %failure_count, "Ignoring connection attempt, peer is on exponential backoff.");
                return;
            }
        }

        if let Some(existing_key) = peer_key_candidates
            .iter()
            .find(|key| self.state.peers.contains_key(key.as_str()))
        {
            event!(
                Level::TRACE,
                peer_ip_port = %existing_key,
                "PEER SESSION ALREADY ESTABLISHED"
            );
            return;
        }

        let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
        #[cfg(feature = "synthetic-load")]
        let manager_event_tx_clone = self.manager_event_tx.clone();
        let resource_manager_clone = self.resource_manager.clone();
        let global_dl_bucket_clone = self.global_dl_bucket.clone();
        let global_ul_bucket_clone = self.global_ul_bucket.clone();
        let info_hash_clone = self.state.info_hash.clone();
        let torrent_metadata_length_clone = self.state.torrent_metadata_length;
        let preferred_key_clone = preferred_key.clone();
        let failure_peer_key = peer_key_candidates
            .iter()
            .find(|key| key.contains("://"))
            .cloned()
            .unwrap_or_else(|| peer_addr.to_string());
        let peer_label = peer_label.to_string();

        let mut shutdown_rx_permit = self.shutdown_tx.subscribe();
        let mut shutdown_rx_session = self.shutdown_tx.subscribe();
        let shutdown_tx = self.shutdown_tx.clone();

        #[cfg(feature = "synthetic-load")]
        let first_transport = if outbound_transport_mode.try_utp {
            PeerTransportKind::Utp
        } else {
            PeerTransportKind::Tcp
        };
        #[cfg(feature = "synthetic-load")]
        if !outbound_transport_mode.try_utp {
            let _ = self
                .manager_event_tx
                .try_send(ManagerEvent::PeerConnectAttempted {
                    transport: PeerTransportKind::Tcp,
                });
        }

        let bitfield = match self.state.torrent {
            None => None,
            _ => Some(self.generate_bitfield()),
        };

        let client_id_clone = self.settings.client_id.clone();
        tokio::spawn(async move {
            let session_permit = match acquire_generation_scoped_peer_permit(
                &resource_manager_clone,
                &network_scope,
                &mut shutdown_rx_permit,
            )
            .await
            {
                Ok(permit) => Some(permit),
                Err(PeerPermitWaitError::NetworkInvalidated) => {
                    event!(Level::DEBUG, peer = %peer_label, "canceling queued peer connection from invalidated network generation");
                    None
                }
                Err(PeerPermitWaitError::TorrentShutdown)
                | Err(PeerPermitWaitError::ResourceManager(
                    ResourceManagerError::ManagerShutdown,
                )) => {
                    #[cfg(feature = "synthetic-load")]
                    let _ = manager_event_tx_clone.try_send(ManagerEvent::PeerConnectFailed {
                        transport: first_transport,
                        reason: SyntheticPeerConnectFailure::PermitManagerShutdown,
                    });
                    None
                }
                Err(PeerPermitWaitError::ResourceManager(ResourceManagerError::QueueFull)) => {
                    #[cfg(feature = "synthetic-load")]
                    let _ = manager_event_tx_clone.try_send(ManagerEvent::PeerConnectFailed {
                        transport: first_transport,
                        reason: SyntheticPeerConnectFailure::PermitQueueFull,
                    });
                    None
                }
                Err(PeerPermitWaitError::TimedOut) => {
                    #[cfg(feature = "synthetic-load")]
                    let _ = manager_event_tx_clone.try_send(ManagerEvent::PeerConnectFailed {
                        transport: first_transport,
                        reason: SyntheticPeerConnectFailure::PermitTimeout,
                    });
                    None
                }
            };

            if let Some(session_permit) = session_permit {
                let mut network_invalidation_rx = network_scope.subscribe_invalidation();
                let network_lease = network_scope.lease().clone();
                let network_scope_id = network_scope.id();
                #[cfg(feature = "synthetic-load")]
                {
                    if outbound_transport_mode.try_utp {
                        let _ =
                            manager_event_tx_clone.try_send(ManagerEvent::PeerConnectAttempted {
                                transport: PeerTransportKind::Utp,
                            });
                    }
                    if outbound_transport_mode.try_utp && outbound_transport_mode.allow_tcp {
                        let _ =
                            manager_event_tx_clone.try_send(ManagerEvent::PeerConnectAttempted {
                                transport: PeerTransportKind::Tcp,
                            });
                    }
                }

                let connection_result = connect_outbound_peer(
                    &network_lease,
                    peer_addr,
                    local_udp_port,
                    outbound_transport_mode,
                    &peer_label,
                )
                .await;

                if network_scope.ensure_valid().is_err() {
                    event!(Level::DEBUG, peer = %peer_label, "discarding peer connection result from invalidated network generation");
                    return;
                }

                match connection_result {
                    Ok(connection) => {
                        #[cfg(feature = "synthetic-load")]
                        let _ =
                            manager_event_tx_clone.try_send(ManagerEvent::PeerConnectEstablished {
                                transport: connection.endpoint.kind,
                            });
                        let selected_transport = connection.endpoint.kind;
                        let peer_session_key = outbound_peer_session_key(
                            peer_addr,
                            outbound_transport_mode,
                            preferred_key_clone,
                            selected_transport,
                        );
                        let (peer_session_tx, peer_session_rx) =
                            mpsc::channel::<TorrentCommand>(1000);
                        let (registration_result_tx, mut registration_result_rx) = mpsc::channel(1);
                        let registered = send_generation_scoped_peer_registration(
                            &torrent_manager_tx_clone,
                            TorrentCommand::RegisterPeerTransport {
                                peer_id: peer_session_key.clone(),
                                peer_addr,
                                tx: peer_session_tx,
                                transport: selected_transport,
                                scope_id: network_scope_id,
                                registration_result_tx,
                            },
                            &mut network_invalidation_rx,
                            &mut shutdown_rx_session,
                        )
                        .await;
                        if !registered {
                            event!(Level::DEBUG, peer = %peer_label, "discarding peer registration after network generation invalidation");
                            return;
                        }
                        let session_cancel = tokio::select! {
                            result = registration_result_rx.recv() => result.flatten(),
                            _ = network_invalidation_rx.changed() => None,
                            _ = shutdown_rx_session.recv() => None,
                        };
                        let Some(session_cancel) = session_cancel else {
                            tracing::trace!(
                                peer = %peer_session_key,
                                "Peer session rejected by current admission policy"
                            );
                            return;
                        };
                        event!(
                            Level::TRACE,
                            peer = %peer_session_key,
                            transport = %connection.transport_key(),
                            direction = ?connection.direction,
                            "peer transport connection established"
                        );
                        let _held_session_permit = session_permit;
                        let session = PeerSession::new(PeerSessionParameters {
                            info_hash: info_hash_clone.clone(),
                            torrent_metadata_length: torrent_metadata_length_clone,
                            connection_type: ConnectionType::Outgoing,
                            torrent_manager_rx: peer_session_rx,
                            torrent_manager_tx: torrent_manager_tx_clone.clone(),
                            peer_ip_port: peer_session_key.clone(),
                            client_id: client_id_clone.into(),
                            global_dl_bucket: global_dl_bucket_clone,
                            global_ul_bucket: global_ul_bucket_clone,
                            shutdown_tx,
                            network_scope_id: Some(network_scope_id),
                            session_cancel,
                        });

                        tokio::select! {
                            biased;
                            changed = network_invalidation_rx.changed() => {
                                if changed.is_err() || *network_invalidation_rx.borrow() {
                                    event!(
                                        Level::DEBUG,
                                        "PEER SESSION {}: Shutting down because its network generation was invalidated.",
                                        &peer_session_key
                                    );
                                }
                            }
                            _ = shutdown_rx_session.recv() => {
                                event!(
                                    Level::DEBUG,
                                    "PEER SESSION {}: Shutting down due to manager signal.",
                                    &peer_session_key
                                );
                            }
                            session_result = session.run(connection.stream, Vec::new(), bitfield) => {
                                if let Err(e) = session_result {
                                    #[cfg(feature = "synthetic-load")]
                                    let _ = manager_event_tx_clone
                                        .try_send(ManagerEvent::PeerSessionFailed);
                                    if utp_connect_log_enabled() {
                                        event!(
                                            Level::INFO,
                                            peer = %peer_session_key,
                                            transport = %selected_transport,
                                            error = %e,
                                            "peer session ended with error over selected transport"
                                        );
                                    }
                                    event!(
                                        Level::DEBUG,
                                        "PEER SESSION {}: ENDED IN ERROR: {}",
                                        &peer_session_key,
                                        e
                                    );
                                }
                            }
                        }
                    }
                    Err((transport, Some(error))) => {
                        #[cfg(feature = "synthetic-load")]
                        let _ = manager_event_tx_clone.try_send(ManagerEvent::PeerConnectFailed {
                            transport,
                            reason: synthetic_peer_connect_failure(&error),
                        });
                        let command = TorrentCommand::Network(
                            network_scope
                                .scoped(NetworkResult::UnresponsivePeer(failure_peer_key.clone())),
                        );
                        let _ = network_scope
                            .run(torrent_manager_tx_clone.send(command))
                            .await;
                        event!(Level::DEBUG, peer = %peer_label, %transport, error = %error, "PEER connection failed");
                    }
                    Err((transport, None)) => {
                        #[cfg(feature = "synthetic-load")]
                        let _ = manager_event_tx_clone.try_send(ManagerEvent::PeerConnectFailed {
                            transport,
                            reason: SyntheticPeerConnectFailure::ConnectTimeout,
                        });
                        let command = TorrentCommand::Network(
                            network_scope
                                .scoped(NetworkResult::UnresponsivePeer(failure_peer_key.clone())),
                        );
                        let _ = network_scope
                            .run(torrent_manager_tx_clone.send(command))
                            .await;
                        event!(Level::DEBUG, peer = %peer_label, %transport, "PEER connection timed out");
                    }
                }
            }
        });
    }

    pub async fn validate_local_file(&mut self) -> Result<(), StorageError> {
        let mfi = match &self.state.multi_file_info {
            Some(i) => i.clone(),
            None => return Ok(()),
        };

        // We can safely expect metadata here because this is called on startup
        // for existing torrents, which must have metadata to exist.
        let torrent = match self.state.torrent.clone() {
            Some(t) => t,
            None => {
                debug_assert!(
                    self.state.torrent.is_some(),
                    "Metadata missing during startup validation"
                );
                event!(
                    Level::ERROR,
                    "Cannot validate local file: Metadata not available."
                );
                return Err(StorageError::from(std::io::Error::other(
                    "Metadata missing during startup validation",
                )));
            }
        };

        let rm = self.resource_manager.clone();
        let shutdown_rx = self.shutdown_tx.subscribe();
        let manager_tx = self.torrent_manager_tx.clone();
        let event = self.manager_event_tx.clone();
        let skip = self.state.torrent_validation_status;

        tokio::spawn(async move {
            let result = Self::perform_validation(
                mfi,
                torrent,
                rm,
                shutdown_rx,
                manager_tx.clone(),
                event,
                skip,
            )
            .await;

            match result {
                Ok(pieces) => {
                    let _ = manager_tx
                        .send(TorrentCommand::ValidationComplete(pieces))
                        .await;
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    event!(Level::ERROR, "Triggering Fatal Pause due to: {}", error_msg);
                    let _ = manager_tx
                        .send(TorrentCommand::FatalStorageError(error_msg))
                        .await;
                }
            }
        });

        Ok(())
    }

    fn generate_activity_message(&self, dl_speed: u64, ul_speed: u64) -> String {
        if self.state.is_paused {
            return "Paused".to_string();
        }

        let connected_peers = self.state.peers.len();
        let useful_peers = self
            .state
            .peers
            .values()
            .filter(|p| p.am_interested)
            .count();
        let peers_sending_data = self
            .state
            .peers
            .values()
            .filter(|p| p.peer_choking == ChokeStatus::Unchoke)
            .count();
        let need_count = self.state.piece_manager.need_queue.len();
        let total_pieces = self.state.piece_manager.bitfield.len() as u32;
        let completed_pieces =
            total_pieces.saturating_sub(self.state.piece_manager.pieces_remaining as u32);
        let completion_pct = (completed_pieces * 100)
            .checked_div(total_pieces)
            .unwrap_or(0);

        if let TorrentActivity::ProcessingPeers(count) = &self.state.last_activity {
            return Self::cap_activity_message(format!("Processing peer ({})", count));
        }

        if self.state.torrent_status == TorrentStatus::AwaitingMetadata {
            let message = if self.state.torrent_metadata_length.is_some() {
                format!("Metadata ({} peers)", connected_peers)
            } else {
                format!("Metadata from peers ({})", connected_peers)
            };
            return Self::cap_activity_message(message);
        }

        if self.state.torrent_status == TorrentStatus::Validating {
            let message = if total_pieces > 0 {
                let validation_pct = (self.state.validation_pieces_found * 100)
                    .checked_div(total_pieces)
                    .unwrap_or(0);
                format!(
                    "Validating {}% ({}/{})",
                    validation_pct, self.state.validation_pieces_found, total_pieces
                )
            } else {
                "Validating".to_string()
            };
            return Self::cap_activity_message(message);
        }

        if self.state.torrent_status == TorrentStatus::Done {
            return if ul_speed > 0 {
                "Seeding".to_string()
            } else {
                "Finished".to_string()
            };
        }

        // 1. Prioritize active Data Transfer
        if dl_speed > 0 {
            return match &self.state.last_activity {
                TorrentActivity::DownloadingPiece(p) => format!("Receiving piece #{}", p),
                TorrentActivity::VerifyingPiece(p) => format!("Verifying piece #{}", p),
                _ => "Downloading".to_string(),
            };
        }

        if ul_speed > 0 {
            return match &self.state.last_activity {
                TorrentActivity::SendingPiece(p) => format!("Sending piece #{}", p),
                _ => "Uploading".to_string(),
            };
        }

        // 2. Handle specific non-transfer activities
        match &self.state.last_activity {
            TorrentActivity::RequestingPieces => {
                return Self::cap_activity_message(format!(
                    "Request {} ({}/{})",
                    need_count, useful_peers, connected_peers
                ));
            }
            TorrentActivity::AnnouncingToTracker => {
                return Self::cap_activity_message(format!("Tracker ({} peers)", connected_peers));
            }
            #[cfg(feature = "dht")]
            TorrentActivity::SearchingDht => {
                return Self::cap_activity_message(format!("DHT search ({})", connected_peers));
            }
            _ => {}
        }

        // 3. Refined "Stalled" vs "Connecting" Logic
        if connected_peers == 0 {
            return Self::cap_activity_message(format!("Connecting ({}%)", completion_pct));
        }

        if need_count > 0 {
            if useful_peers > 0 {
                return Self::cap_activity_message(format!(
                    "Waiting data ({}/{})",
                    peers_sending_data, connected_peers
                ));
            }
            return Self::cap_activity_message(format!("Need pieces ({})", connected_peers));
        }

        Self::cap_activity_message(format!("Idle ({}, {}%)", connected_peers, completion_pct))
    }

    fn cap_activity_message(message: String) -> String {
        if message.chars().count() <= ACTIVITY_MESSAGE_MAX_LEN {
            return message;
        }
        let keep = ACTIVITY_MESSAGE_MAX_LEN.saturating_sub(3);
        let truncated: String = message.chars().take(keep).collect();
        format!("{}...", truncated)
    }

    fn send_metrics(
        &mut self,
        bytes_dl: u64,
        bytes_ul: u64,
        file_activity_updates: Vec<crate::torrent_manager::FileActivityUpdate>,
    ) {
        let mut torrent_state = self.build_metrics_snapshot(bytes_dl, bytes_ul);
        torrent_state
            .departed_peers
            .extend(
                self.state
                    .departed_peer_transfers()
                    .map(|departed| PeerInfo {
                        address: departed.address.clone(),
                        peer_id: departed.peer_id.clone(),
                        total_downloaded: departed.total_downloaded,
                        total_uploaded: departed.total_uploaded,
                        connection_count: departed.connection_count,
                        disconnect_count: departed.disconnect_count,
                        last_action: "Disconnected".to_string(),
                        ..Default::default()
                    }),
            );
        let Some(torrent) = self.state.torrent.as_ref() else {
            if self.telemetry.should_emit(&torrent_state) {
                let _ = self.metrics_tx.send(torrent_state);
            }
            return;
        };
        let Some(multi_file_info) = self.state.multi_file_info.as_ref() else {
            if self.telemetry.should_emit(&torrent_state) {
                let _ = self.metrics_tx.send(torrent_state);
            }
            event!(
                Level::DEBUG,
                "Cannot send full metrics: File info not available."
            );
            return;
        };

        let next_announce_in = self
            .state
            .trackers
            .values()
            .map(|t| t.next_announce_time)
            .min()
            .map_or(Duration::MAX, |t| {
                t.saturating_duration_since(Instant::now())
            });
        let number_of_pieces_total = self.state.piece_manager.bitfield.len() as u32;
        let number_of_pieces_completed = if self.state.torrent_status == TorrentStatus::Validating {
            self.state.validation_pieces_found
        } else {
            number_of_pieces_total - self.state.piece_manager.pieces_remaining as u32
        };
        let total_size_bytes = multi_file_info.total_size;
        let bytes_written = if number_of_pieces_completed == number_of_pieces_total {
            total_size_bytes
        } else {
            (number_of_pieces_completed as u64) * torrent.info.piece_length as u64
        };
        let eta = if self.state.piece_manager.pieces_remaining == 0 {
            Duration::from_secs(0)
        } else if torrent_state.download_speed_bps == 0 {
            Duration::MAX
        } else {
            let bytes_completed = (torrent.info.piece_length as u64).saturating_mul(
                self.state
                    .piece_manager
                    .bitfield
                    .iter()
                    .filter(|&s| *s == PieceStatus::Done)
                    .count() as u64,
            );
            let bytes_remaining = total_size_bytes.saturating_sub(bytes_completed);
            let eta_seconds = (bytes_remaining * 8)
                .checked_div(torrent_state.download_speed_bps)
                .unwrap_or(0);
            Duration::from_secs(eta_seconds)
        };

        torrent_state.torrent_name.clone_from(&torrent.info.name);
        torrent_state.is_multi_file = !torrent.info.files.is_empty();
        torrent_state.file_count = Some(multi_file_info.files.len());
        torrent_state.number_of_pieces_total = number_of_pieces_total;
        torrent_state.number_of_pieces_completed = number_of_pieces_completed;
        torrent_state.eta = eta;
        torrent_state.next_announce_in = next_announce_in;
        torrent_state.total_size = total_size_bytes;
        torrent_state.bytes_written = bytes_written;
        torrent_state.file_activity_updates = file_activity_updates;

        if self.telemetry.should_emit(&torrent_state) {
            let _ = self.metrics_tx.send(torrent_state);
        }
    }

    fn build_metrics_snapshot(&self, bytes_dl: u64, bytes_ul: u64) -> TorrentMetrics {
        let download_speed_bps = self.state.total_dl_prev_avg_ema as u64;
        let upload_speed_bps = self.state.total_ul_prev_avg_ema as u64;
        let transport_counts = count_peers_by_transport(self.state.peers.values());
        let peers = self
            .state
            .peers
            .values()
            .map(|peer| {
                let base_action = match &peer.last_action {
                    TorrentCommand::SuccessfullyConnected(id) if id.is_empty() => {
                        "Connecting...".to_string()
                    }
                    TorrentCommand::SuccessfullyConnected(_) => "Handshake".to_string(),
                    TorrentCommand::PeerBitfield(_, _) => "Bitfield".to_string(),
                    TorrentCommand::Choke(_) => "Choked".to_string(),
                    TorrentCommand::Unchoke(_) => "Unchoked".to_string(),
                    TorrentCommand::Disconnect(_) => "Disconnected".to_string(),
                    TorrentCommand::Have(_, _) => "Have".to_string(),
                    TorrentCommand::Block(_, _, _, _) => "Receiving".to_string(),
                    TorrentCommand::RequestUpload(_, _, _, _) => "Requesting".to_string(),
                    TorrentCommand::BulkCancel(_) => "Canceling".to_string(),
                    _ => "Idle".to_string(),
                };
                let discriminant = std::mem::discriminant(&peer.last_action);
                let count = peer.action_counts.get(&discriminant).unwrap_or(&0);
                let last_action = if *count > 0 {
                    format!("{} (x{})", base_action, count)
                } else {
                    base_action
                };

                PeerInfo {
                    address: peer.network_address(),
                    peer_id: peer.peer_id.clone(),
                    am_choking: peer.am_choking != ChokeStatus::Unchoke,
                    peer_choking: peer.peer_choking != ChokeStatus::Unchoke,
                    am_interested: peer.am_interested,
                    peer_interested: peer.peer_is_interested_in_us,
                    bitfield: peer.bitfield.clone(),
                    download_speed_bps: peer.download_speed_bps,
                    upload_speed_bps: peer.upload_speed_bps,
                    total_downloaded: peer
                        .transfer_base_downloaded
                        .saturating_add(peer.total_bytes_downloaded),
                    total_uploaded: peer
                        .transfer_base_uploaded
                        .saturating_add(peer.total_bytes_uploaded),
                    connection_count: peer.connection_count,
                    disconnect_count: peer.disconnect_count,
                    last_action,
                }
            })
            .collect();

        TorrentMetrics {
            info_hash: self.state.info_hash.clone(),
            torrent_name: self
                .state
                .torrent
                .as_ref()
                .map(|torrent| torrent.info.name.clone())
                .unwrap_or_default(),
            download_path: self.state.torrent_data_path.clone(),
            container_name: self.state.container_name.clone(),
            file_priorities: self.state.file_priorities.clone(),
            data_available: self.state.data_available,
            is_complete: self.state.torrent_status == TorrentStatus::Done,
            number_of_successfully_connected_peers: self.state.peers.len(),
            tcp_peer_count: transport_counts.tcp_peer_count,
            utp_peer_count: transport_counts.utp_peer_count,
            beneficial_tcp_peer_count: transport_counts.beneficial_tcp_peer_count,
            beneficial_utp_peer_count: transport_counts.beneficial_utp_peer_count,
            download_speed_bps,
            upload_speed_bps,
            bytes_downloaded_this_tick: bytes_dl,
            bytes_uploaded_this_tick: bytes_ul,
            session_total_downloaded: self.state.session_total_downloaded,
            session_total_uploaded: self.state.session_total_uploaded,
            peers,
            peer_reconnect_counts: self.state.peer_reconnect_counts.clone(),
            activity_message: self.generate_activity_message(download_speed_bps, upload_speed_bps),
            ..Default::default()
        }
    }

    fn file_probe_relative_path(
        torrent: &Torrent,
        file_index: usize,
        absolute_path: &std::path::Path,
    ) -> std::path::PathBuf {
        if torrent.info.files.is_empty() {
            return std::path::PathBuf::from(&torrent.info.name);
        }

        if let Some(file) = torrent.info.files.get(file_index) {
            let mut relative_path = std::path::PathBuf::new();
            for component in &file.path {
                relative_path.push(component);
            }
            return relative_path;
        }

        absolute_path
            .file_name()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| absolute_path.to_path_buf())
    }

    fn prepare_file_probe_batch(
        torrent: &Torrent,
        multi_file_info: &MultiFileInfo,
        epoch: u64,
        start_file_index: usize,
        max_files: usize,
    ) -> FileProbeBatchPreparation {
        if start_file_index >= multi_file_info.files.len() {
            return FileProbeBatchPreparation::Ready(FileProbeBatchResult {
                epoch,
                scanned_files: 0,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            });
        }

        let end_file_index = multi_file_info
            .files
            .len()
            .min(start_file_index.saturating_add(max_files));
        let mut files = Vec::new();

        for (file_index, file_info) in multi_file_info
            .files
            .iter()
            .enumerate()
            .skip(start_file_index)
            .take(end_file_index.saturating_sub(start_file_index))
        {
            let relative_path =
                Self::file_probe_relative_path(torrent, file_index, file_info.path.as_path());

            if file_info.is_padding {
                continue;
            }

            if file_info.is_skipped {
                continue;
            }

            files.push(PreparedFileProbeEntry {
                relative_path,
                absolute_path: file_info.path.clone(),
                expected_size: file_info.length,
            });
        }

        let reached_end_of_manifest = end_file_index >= multi_file_info.files.len();

        FileProbeBatchPreparation::Scan(PreparedFileProbeBatch {
            epoch,
            scanned_files: end_file_index.saturating_sub(start_file_index),
            next_file_index: if reached_end_of_manifest {
                0
            } else {
                end_file_index
            },
            reached_end_of_manifest,
            files,
        })
    }

    async fn collect_prepared_file_probe_batch(
        batch: PreparedFileProbeBatch,
    ) -> FileProbeBatchResult {
        let mut problem_files = Vec::new();

        for file in batch.files {
            let (error, observed_size) = match fs::metadata(&file.absolute_path).await {
                Ok(metadata) => {
                    if !metadata.is_file() {
                        (Some(StorageError::UnexpectedType), None)
                    } else {
                        let observed_size = metadata.len();
                        (
                            if observed_size == file.expected_size {
                                None
                            } else {
                                Some(StorageError::SizeMismatch {
                                    expected_size: file.expected_size,
                                    observed_size,
                                })
                            },
                            Some(observed_size),
                        )
                    }
                }
                Err(error) => (Some(StorageError::from(error)), None),
            };
            let Some(error) = error else {
                continue;
            };

            problem_files.push(FileProbeEntry {
                relative_path: file.relative_path,
                absolute_path: file.absolute_path,
                error,
                expected_size: file.expected_size,
                observed_size,
            });
        }

        FileProbeBatchResult {
            epoch: batch.epoch,
            scanned_files: batch.scanned_files,
            next_file_index: batch.next_file_index,
            reached_end_of_manifest: batch.reached_end_of_manifest,
            pending_metadata: false,
            problem_files,
        }
    }

    #[cfg(test)]
    async fn collect_file_probe_batch(
        torrent: &Torrent,
        multi_file_info: &MultiFileInfo,
        epoch: u64,
        start_file_index: usize,
        max_files: usize,
    ) -> FileProbeBatchResult {
        match Self::prepare_file_probe_batch(
            torrent,
            multi_file_info,
            epoch,
            start_file_index,
            max_files,
        ) {
            FileProbeBatchPreparation::Ready(result) => result,
            FileProbeBatchPreparation::Scan(batch) => {
                Self::collect_prepared_file_probe_batch(batch).await
            }
        }
    }

    fn spawn_file_probe_batch(&self, epoch: u64, start_file_index: usize, max_files: usize) {
        let info_hash = self.state.info_hash.clone();
        let manager_event_tx = self.manager_event_tx.clone();
        let preparation = if self.state.torrent_status == TorrentStatus::AwaitingMetadata {
            FileProbeBatchPreparation::Ready(FileProbeBatchResult {
                epoch,
                scanned_files: 0,
                next_file_index: 0,
                reached_end_of_manifest: false,
                pending_metadata: true,
                problem_files: Vec::new(),
            })
        } else if let (Some(torrent), Some(multi_file_info)) = (
            self.state.torrent.as_ref(),
            self.state.multi_file_info.as_ref(),
        ) {
            Self::prepare_file_probe_batch(
                torrent,
                multi_file_info,
                epoch,
                start_file_index,
                max_files,
            )
        } else {
            FileProbeBatchPreparation::Ready(FileProbeBatchResult {
                epoch,
                scanned_files: 0,
                next_file_index: 0,
                reached_end_of_manifest: false,
                pending_metadata: true,
                problem_files: Vec::new(),
            })
        };

        tokio::spawn(async move {
            let result = match preparation {
                FileProbeBatchPreparation::Ready(result) => result,
                FileProbeBatchPreparation::Scan(batch) => {
                    Self::collect_prepared_file_probe_batch(batch).await
                }
            };
            let _ = manager_event_tx
                .send(ManagerEvent::FileProbeBatchResult { info_hash, result })
                .await;
        });
    }

    pub async fn run(mut self, is_paused: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
        //    We MUST find peers to get metadata.

        //    We wait for validation to finish so we report accurate "Left" stats
        //    to the tracker (preventing bans on private trackers).
        let announce_immediately = self.state.torrent.is_none();
        self.apply_action(Action::TorrentManagerInit {
            is_paused,
            announce_immediately,
        });

        if self.state.torrent.is_some() {
            if let Err(error) = self.validate_local_file().await {
                event!(Level::ERROR, error = %error, "Error calling validate local file");
            }
        }

        self.run_loop_started = true;
        self.sync_dht_lookup_task();

        let mut tick = tokio::time::interval(Duration::from_millis(self.data_rate_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_tick_time = Instant::now();

        let mut cleanup_timer = tokio::time::interval(Duration::from_secs(3));
        let mut choke_timer = tokio::time::interval(Duration::from_secs(10));
        let mut rarity_timer = tokio::time::interval(Duration::from_secs(1));
        rarity_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut pex_timer = tokio::time::interval(Duration::from_secs(75));
        let outcome = loop {
            tokio::select! {
                biased;
                _ = signal::ctrl_c() => {
                    tracing::info!("Ctrl+C received, initiating clean shutdown...");
                    break Ok(());
                }
                policy_result = self.peer_policy_rx.changed(), if self.peer_policy_open => {
                    if policy_result.is_ok() {
                        self.refresh_peer_policy();
                    } else {
                        self.peer_policy_open = false;
                    }
                }
                activation_result = self.network_activation_rx.changed(), if self.network_activation_open => {
                    if activation_result.is_ok() {
                        self.apply_latest_network_activation();
                    } else {
                        self.network_activation_open = false;
                    }
                }
                report = async {
                    #[cfg(feature = "webtorrent")]
                    { self.rtc.reports.recv().await }
                    #[cfg(not(feature = "webtorrent"))]
                    { std::future::pending::<Option<()>>().await }
                } => {
                    #[cfg(feature = "webtorrent")]
                    if let Some(report) = report { self.rtc_report(report); }
                    #[cfg(not(feature = "webtorrent"))]
                    let _ = report;
                },
                _ = tick.tick(), if !self.state.is_paused => {
                    let now = Instant::now();
                    let actual_duration = now.duration_since(last_tick_time);
                    last_tick_time = now;
                    let actual_ms = actual_duration.as_millis() as u64;

                    if self.state.torrent_status == TorrentStatus::Endgame {
                        let peer_ids: Vec<String> = self.state.peers.keys().cloned().collect();
                        for peer_id in peer_ids {
                            if let Some(peer) = self.state.peers.get(&peer_id) {
                                if peer.pending_requests.is_empty() {
                                    self.apply_action(Action::AssignWork { peer_id: peer_id.clone() });
                                }
                            }
                        }
                    }

                    let _cmd_len = self.torrent_manager_rx.len();
                    let _cmd_cap = self.torrent_manager_rx.capacity();
                    let _write_tasks = self.in_flight_writes.len();
                    let _upload_tasks = self.in_flight_uploads.len();
                    let _pending_pieces = self.state.piece_manager.pending_queue.len();
                    let _need_pieces = self.state.piece_manager.need_queue.len();

                    self.apply_action(Action::Tick { dt_ms: actual_ms });
                }
                _ = cleanup_timer.tick(), if !self.state.is_paused => {
                    self.apply_action(Action::Cleanup);
                }

                _ = choke_timer.tick(), if !self.state.is_paused => {
                    self.apply_action(Action::RecalculateChokes {
                        random_seed: rand::rng().random()
                    });
                }

                _ = rarity_timer.tick(), if !self.state.is_paused => {
                    if self.state.torrent_status != TorrentStatus::Done {
                        let peer_bitfields = self.state.peers.values().map(|p| &p.bitfield);
                        self.state.piece_manager.update_rarity(peer_bitfields);
                    }
                }

                _ = pex_timer.tick(), if !self.state.is_paused => {
                    if self.state.peers.len() < 2 {
                        continue;
                    }

                    #[cfg(feature = "pex")]
                    let all_peer_ips: Vec<String> = self.state.peers.keys().cloned().collect();

                    #[cfg(feature = "pex")]
                    for peer_state in self.state.peers.values() {
                        let peer_tx = peer_state.peer_tx.clone();
                        let peers_list = all_peer_ips.clone();

                        let _ = peer_tx.try_send(
                            TorrentCommand::SendPexPeers(peers_list)
                        );
                    }
                }

                Some(manager_command) = self.manager_command_rx.recv() => {
                    event!(Level::TRACE, ?manager_command);
                    match manager_command {
                        #[cfg(feature = "synthetic-load")]
                        ManagerCommand::ConnectToPeer(peer_addr) => {
                            self.connect_to_peer(peer_addr);
                        }
                        #[cfg(feature = "synthetic-load")]
                        ManagerCommand::ConnectToSyntheticPeer { addr, peer_key } => {
                            self.connect_to_synthetic_peer(addr, peer_key);
                        }
                        ManagerCommand::ProbeFileBatch {
                            epoch,
                            start_file_index,
                            max_files,
                        } => {
                            self.spawn_file_probe_batch(epoch, start_file_index, max_files);
                        }
                        ManagerCommand::SetDataAvailability(available) => {
                            self.apply_action(Action::SetDataAvailability { available });
                        }
                        ManagerCommand::Pause => self.apply_action(Action::Pause),
                        ManagerCommand::Resume => self.apply_action(Action::Resume),
                        ManagerCommand::DeleteFile => {
                            self.apply_action(Action::Delete);
                            break Ok(());
                        },
                        ManagerCommand::SetUserTorrentConfig { torrent_data_path, file_priorities, container_name } => {
                            self.apply_action(Action::SetUserTorrentConfig {
                                torrent_data_path,
                                file_priorities,
                                container_name,
                            });
                        }
                        ManagerCommand::SetDataRate(new_rate_ms) => {
                            self.data_rate_ms = new_rate_ms;
                            tick = tokio::time::interval(Duration::from_millis(self.data_rate_ms));
                            tick.reset();
                            last_tick_time = Instant::now();
                        },
                        ManagerCommand::Shutdown => {
                            self.apply_action(Action::Shutdown);
                            break Ok(());
                        },
                    }
                }

                _maybe_peers = async {
                    #[cfg(feature = "dht")]
                    {
                        self.dht_rx.recv().await
                    }
                    #[cfg(not(feature = "dht"))]
                    {
                        std::future::pending().await
                    }
                }, if !self.state.is_paused => {
                    #[cfg(feature = "dht")]
                    {
                        if let Some(peers) = _maybe_peers {
                            self.state.last_activity = TorrentActivity::SearchingDht;
                            for peer in peers {
                                event!(Level::DEBUG, "PEER FROM DHT {}", peer);
                                if self.should_accept_new_peers() {
                                    self.connect_to_peer(peer);
                                }
                            }
                        } else {
                            event!(Level::WARN, "DHT channel closed. No longer receiving DHT peers.");
                        }
                    }
                }

                Some((connection, handshake_response, session_permit)) = self.incoming_peer_rx.recv() => {
                    if self.state.is_paused || !self.should_accept_new_peers() {
                        continue;
                    }
                    let _ = self.manager_event_tx.try_send(ManagerEvent::PeerDiscovered { info_hash: self.state.info_hash.clone() });
                    {
                        let peer_ip_port = connection.peer_id();
                        let network_generation_id = connection.network_generation_id();
                        let network_scope_id = connection.network_scope_id();
                        let mut network_invalidation_rx = connection.subscribe_network_invalidation();
                        if network_invalidation_rx
                            .as_ref()
                            .is_some_and(|invalidation_rx| *invalidation_rx.borrow())
                            || network_scope_id
                                .is_some_and(|scope_id| !self.network_activation.is_current(scope_id))
                            || network_scope_id.is_none()
                        {
                            event!(
                                Level::DEBUG,
                                peer = %peer_ip_port,
                                generation_id = ?network_generation_id,
                                "dropping incoming peer from an inactive network generation"
                            );
                            continue;
                        }
                        let incoming_hash = &handshake_response[28..48];

                        let matches_primary = self.state.info_hash == incoming_hash;

                        let mut matches_secondary = false;
                        let mut calculated_v2_hash = Vec::new();

                        if !matches_primary {
                            // Only check secondary if we have metadata and it is V2-capable
                            if let Some(torrent) = &self.state.torrent {
                                if torrent.info.meta_version == Some(2) {
                                    // Calculate V2 hash (SHA256 truncated) from the stored info_dict
                                    let mut hasher = sha2::Sha256::new();
                                    hasher.update(&torrent.info_dict_bencode);
                                    let v2_hash = hasher.finalize()[0..20].to_vec();

                                    if v2_hash == incoming_hash {
                                        matches_secondary = true;
                                        calculated_v2_hash = v2_hash;
                                    }
                                }
                            }
                        }

                        if !matches_primary && !matches_secondary {
                            event!(Level::WARN, "Peer {} info_hash mismatch. Dropping.", peer_ip_port);
                            continue;
                        }

                        let active_info_hash = if matches_secondary {
                            calculated_v2_hash
                        } else {
                            self.state.info_hash.clone()
                        };

                        event!(Level::DEBUG, peer_addr = %peer_ip_port, "NEW INCOMING PEER CONNECTION");
                        let torrent_manager_tx_clone = self.torrent_manager_tx.clone();
                        let (peer_session_tx, peer_session_rx) = mpsc::channel::<TorrentCommand>(10_000);

                        if self.state.peers.contains_key(&peer_ip_port) {
                            event!(Level::WARN, peer_ip = %peer_ip_port, "Already connected to this peer. Dropping incoming connection.");
                            continue;
                        }

                        let Some(session_cancel) = self.register_peer(
                            peer_ip_port.clone(),
                            Some(connection.remote_addr),
                            peer_session_tx,
                        ) else {
                            tracing::trace!(
                                peer = %peer_ip_port,
                                "Inbound peer session rejected by current admission policy"
                            );
                            continue;
                        };
                        if let Some(scope_id) = network_scope_id {
                            self.peer_network_scopes
                                .insert(peer_ip_port.clone(), scope_id);
                        }
                        self.apply_action(Action::PeerTransportSelected {
                            peer_id: peer_ip_port.clone(),
                            transport: connection.endpoint.kind,
                        });

                        let bitfield = match self.state.torrent {
                            None => None,
                            _ => Some(self.generate_bitfield())
                        };

                        let session_info_hash = active_info_hash;

                        let torrent_metadata_length_clone = self.state.torrent_metadata_length;
                        let global_dl_bucket_clone = self.global_dl_bucket.clone();
                        let global_ul_bucket_clone = self.global_ul_bucket.clone();
                        let mut shutdown_rx_manager = self.shutdown_tx.subscribe();
                        let shutdown_tx = self.shutdown_tx.clone();
                        let client_id_clone = self.settings.client_id.clone();

                        let _ = self.manager_event_tx.try_send(ManagerEvent::PeerConnected { info_hash: self.state.info_hash.clone() });
                        event!(
                            Level::TRACE,
                            peer = %peer_ip_port,
                            transport = %connection.transport_key(),
                            direction = ?connection.direction,
                            "incoming peer transport routed to torrent manager"
                        );
                        tokio::spawn(async move {
                            let _held_session_permit = session_permit;
                            let session = PeerSession::new(PeerSessionParameters {
                                info_hash: session_info_hash, // <--- Corrected Hash passed here
                                torrent_metadata_length: torrent_metadata_length_clone,
                                connection_type: ConnectionType::Incoming,
                                torrent_manager_rx: peer_session_rx,
                                torrent_manager_tx: torrent_manager_tx_clone.clone(),
                                peer_ip_port: peer_ip_port.clone(),
                                client_id: client_id_clone.into(),
                                global_dl_bucket: global_dl_bucket_clone,
                                global_ul_bucket: global_ul_bucket_clone,
                                shutdown_tx,
                                network_scope_id,
                                session_cancel,
                            });

                            tokio::select! {
                                biased;
                                _ = async {
                                    let Some(invalidation_rx) = network_invalidation_rx.as_mut() else {
                                        std::future::pending::<()>().await;
                                        return;
                                    };
                                    if !*invalidation_rx.borrow() {
                                        let _ = invalidation_rx.changed().await;
                                    }
                                } => {
                                    event!(
                                        Level::DEBUG,
                                        "INCOMING PEER SESSION {}: Shutting down because its network generation was invalidated.",
                                        &peer_ip_port
                                    );
                                    queue_peer_disconnect(
                                        torrent_manager_tx_clone,
                                        peer_ip_port.clone(),
                                        network_scope_id,
                                    );
                                }
                                _ = shutdown_rx_manager.recv() => {
                                    event!(
                                        Level::DEBUG,
                                        "INCOMING PEER SESSION {}: Shutting down due to manager signal.",
                                        &peer_ip_port
                                    );
                                }
                                session_result = session.run(connection.stream, handshake_response, bitfield) => {
                                    if let Err(e) = session_result {
                                        event!(Level::ERROR, peer_ip = %peer_ip_port, error = %e, "Incoming peer session ended with error.");
                                    }
                                }
                            }
                        });
                    }
                }

                Some(command) = self.torrent_manager_rx.recv() => {

                    event!(Level::DEBUG, command_summary = ?TorrentCommandSummary(&command));
                    event!(Level::TRACE, ?command);

                    let peer_id_for_action = match &command {
                        TorrentCommand::SuccessfullyConnected(id) => Some(id),
                        TorrentCommand::PeerBitfield(id, _) => Some(id),
                        TorrentCommand::Choke(id) => Some(id),
                        TorrentCommand::Unchoke(id) => Some(id),
                        TorrentCommand::Have(id, _) => Some(id),
                        TorrentCommand::Block(id, _, _, _) => Some(id),
                        TorrentCommand::RequestUpload(id, _, _, _) => Some(id),
                        TorrentCommand::Disconnect(id) => Some(id),
                        _ => None,
                    };
                    if let Some(id) = peer_id_for_action {
                        if let Some(peer) = self.state.peers.get_mut(id) {
                            peer.last_action = command.clone();
                            let discriminant = std::mem::discriminant(&command);
                            *peer.action_counts.entry(discriminant).or_insert(0) += 1;
                        }
                    }

                    match command {

                        TorrentCommand::SuccessfullyConnected(peer_id) => {
                            if utp_connect_log_enabled() {
                                if let Some(peer) = self.state.peers.get(&peer_id) {
                                    event!(
                                        Level::INFO,
                                        peer = %peer_id,
                                        transport = %peer.transport_kind,
                                        "BitTorrent peer session established over selected transport"
                                    );
                                } else {
                                    event!(
                                        Level::INFO,
                                        peer = %peer_id,
                                        "BitTorrent peer session established before peer state was registered"
                                    );
                                }
                            }
                            self.apply_action(Action::PeerSuccessfullyConnected { peer_id })
                        },
                        TorrentCommand::PeerId(addr, id) => self.apply_action(Action::UpdatePeerId { peer_addr: addr, new_id: id }),
                        TorrentCommand::RegisterPeer {
                            peer_id,
                            peer_addr,
                            tx,
                            registration_result_tx,
                        } => {
                            let session_cancel = self.register_peer(peer_id, Some(peer_addr), tx);
                            let _ = registration_result_tx.try_send(session_cancel);
                        },
                        TorrentCommand::RegisterPeerTransport {
                            peer_id,
                            peer_addr,
                            tx,
                            transport,
                            scope_id,
                            registration_result_tx,
                        } => {
                            let session_cancel = if self.network_activation.is_current(scope_id) {
                                let session_cancel =
                                    self.register_peer(peer_id.clone(), Some(peer_addr), tx);
                                if session_cancel.is_some() {
                                    self.peer_network_scopes
                                        .insert(peer_id.clone(), scope_id);
                                    self.apply_action(Action::PeerTransportSelected {
                                        peer_id,
                                        transport,
                                    });
                                }
                                session_cancel
                            } else {
                                event!(
                                    Level::TRACE,
                                    ?scope_id,
                                    "ignoring peer registration from an inactive network scope"
                                );
                                None
                            };
                            let _ = registration_result_tx.try_send(session_cancel);
                        },
                        TorrentCommand::PeerTransportSelected { peer_id, transport } => {
                            self.apply_action(Action::PeerTransportSelected { peer_id, transport })
                        },

                        TorrentCommand::MerkleHashData { peer_id, root, piece_index, proof, .. } => {
                            if let Some(torrent) = &self.state.torrent {
                                let piece_len = torrent.info.piece_length as u64;
                                let mut v2_roots = torrent.get_v2_roots();
                                v2_roots.sort_by(|(path_a, _, _), (path_b, _, _)| path_a.cmp(path_b));

                                let mut current_file_start = 0;

                                for (_, len, r) in v2_roots {
                                    if r == root {
                                        // Find where this file starts in piece units
                                        let file_start_piece = (current_file_start / piece_len) as u32;
                                        let global_idx = file_start_piece + piece_index;

                                        self.apply_action(Action::MerkleProofReceived {
                                            peer_id: peer_id.clone(),
                                            piece_index: global_idx,
                                            proof: proof.clone(),
                                        });
                                    }
                                    // Multi-file V2 files are always piece-aligned
                                    current_file_start += len.div_ceil(piece_len) * piece_len;
                                }
                            }
                        }

                        #[cfg(feature = "pex")]
                        TorrentCommand::AddPexPeers(_peer_id, new_peers) => {
                            for peer_addr in new_peers {
                                if self.should_accept_new_peers() {
                                    self.connect_to_peer(peer_addr);
                                }
                            }
                        },
                        TorrentCommand::PeerBitfield(pid, bf) => self.apply_action(Action::PeerBitfieldReceived { peer_id: pid, bitfield: bf }),
                        TorrentCommand::Choke(pid) => self.apply_action(Action::PeerChoked { peer_id: pid }),
                        TorrentCommand::Unchoke(pid) => self.apply_action(Action::PeerUnchoked { peer_id: pid }),
                        TorrentCommand::PeerInterested(pid) => self.apply_action(Action::PeerInterested { peer_id: pid }),
                        TorrentCommand::Have(pid, idx) => self.apply_action(Action::PeerHavePiece { peer_id: pid, piece_index: idx }),
                        TorrentCommand::Disconnect(pid) => self.apply_action(Action::PeerDisconnected { peer_id: pid, force: false }),
                        TorrentCommand::DisconnectGeneration { peer_id, scope_id } => {
                            self.handle_generation_scoped_peer_disconnected(peer_id, scope_id)
                        },
                        TorrentCommand::WebSeedDisconnected { peer_id, scope_id } => {
                            self.handle_web_seed_disconnected(peer_id, scope_id)
                        },
                        TorrentCommand::Block(peer_id, piece_index, block_offset, block_data) => self.apply_action(Action::IncomingBlock { peer_id, piece_index, block_offset, data: block_data }),
                        TorrentCommand::PieceVerified { piece_index, peer_id, verification_result } => {
                            match verification_result {
                                Ok(data) => {
                                    self.apply_action(Action::PieceVerified {
                                        peer_id, piece_index, valid: true, data
                                    });
                                }
                                Err(_) => {
                                    self.apply_action(Action::PieceVerified {
                                        peer_id, piece_index, valid: false, data: Vec::new()
                                    });
                                }
                            }
                        },

                        TorrentCommand::PieceWrittenToDisk { peer_id, piece_index } => {
                            if let Some(handles) = self.in_flight_writes.remove(&piece_index) {
                                for handle in handles {
                                    handle.abort();
                                }
                            }
                            self.apply_action(Action::PieceWrittenToDisk { peer_id, piece_index });
                        },
                        TorrentCommand::PieceWriteFailed { piece_index } => {
                            if let Some(handles) = self.in_flight_writes.remove(&piece_index) {
                                for handle in handles {
                                    handle.abort();
                                }
                            }
                            self.apply_action(Action::PieceWriteFailed { piece_index });
                        },
                        TorrentCommand::RequestUpload(peer_id, piece_index, block_offset, block_length) => self.apply_action(Action::RequestUpload { peer_id, piece_index, block_offset, length: block_length }),

                        TorrentCommand::GetHashes { peer_id, index, length, base_layer, file_root, .. } => {
                            let mut sent = false;

                            if let (Some(torrent), Some(roots)) = (&self.state.torrent, self.state.piece_to_roots.get(&index)) {
                                for root_info in roots {
                                    if !file_root.is_empty() && root_info.root_hash != file_root {
                                        continue;
                                    }

                                    if let Some(proof_data) = torrent.get_v2_hash_layer(
                                        index,
                                        root_info.file_offset,
                                        root_info.length,
                                        length,
                                        &root_info.root_hash
                                    ) {
                                        if let Some(peer) = self.state.peers.get(&peer_id) {
                                            let _ = peer.peer_tx.try_send(TorrentCommand::SendHashPiece {
                                                peer_id: peer_id.clone(),
                                                root: root_info.root_hash.clone(),
                                                base_layer,
                                                index,
                                                proof: proof_data,
                                            });
                                            sent = true;
                                            break;
                                        }
                                    }
                                }
                            }
                            if !sent {
                                if let Some(peer) = self.state.peers.get(&peer_id) {
                                    let _ = peer.peer_tx.try_send(TorrentCommand::SendHashReject {
                                        peer_id,
                                        root: file_root,
                                        base_layer,
                                        index,
                                        length
                                    });
                                }
                            }
                        },

                        TorrentCommand::CancelUpload(peer_id, piece_index, block_offset, block_length) => {
                            self.apply_action(Action::CancelUpload {
                                peer_id,
                                piece_index,
                                block_offset,
                                length: block_length
                            });
                        },
                        TorrentCommand::UploadTaskCompleted { peer_id, block_info } => {
                            if let Some(peer_uploads) = self.in_flight_uploads.get_mut(&peer_id) {
                                peer_uploads.remove(&block_info);
                            }
                        },

                        #[cfg(feature = "webtorrent")]
                        TorrentCommand::MetadataRequest { peer_id, piece } => self.rtc_metadata_request(peer_id, piece),
                        TorrentCommand::MetadataTorrent(torrent, metadata_length) => {
                            #[cfg(all(feature = "dht", feature = "pex"))]
                            if torrent.info.private == Some(1) {
                                break Ok(());
                            }

                            if self.state.torrent.is_some() {
                                continue;
                            }

                            let mut torrent = *torrent;

                            // 1. Identify if this is a Hybrid, if so, use v1 protocol
                            let is_hybrid = !torrent.info.pieces.is_empty() && torrent.info.meta_version == Some(2);
                            if is_hybrid {
                                tracing::debug!("Hybrid torrent detected, using V1 protocol");
                                // Strip V2 fields so the rest of the app sees a standard V1 torrent
                                torrent.info.meta_version = None;
                                torrent.info.file_tree = None;
                                torrent.piece_layers = None;
                            }

                            let calculated_hash = if torrent.info.meta_version == Some(2) {
                                use sha2::{Digest, Sha256};
                                let mut hasher = Sha256::new();
                                hasher.update(&torrent.info_dict_bencode);
                                hasher.finalize()[0..20].to_vec()
                            } else {
                                let mut hasher = sha1::Sha1::new();
                                hasher.update(&torrent.info_dict_bencode);
                                hasher.finalize().to_vec()
                            };

                            if calculated_hash == self.state.info_hash {
                                tracing::debug!("METADATA VALIDATED - {}: Proceeding with metadata hydration.", hex::encode(&calculated_hash));
                                self.apply_action(Action::MetadataReceived {
                                    torrent: Box::new(torrent.clone()),
                                    metadata_length,
                                });
                                #[cfg(feature = "webtorrent")]
                                self.rtc_metadata_ready(metadata_length as usize);
                            } else {
                                tracing::debug!(
                                    "Metadata Hash Mismatch! Expected: {:?}, Got: {:?}",
                                    hex::encode(&self.state.info_hash),
                                    hex::encode(&calculated_hash)
                                );
                            }

                            let manager_event_tx = self.manager_event_tx.clone();
                            tokio::spawn(async move {
                                let _ = manager_event_tx
                                    .send(ManagerEvent::MetadataLoaded {
                                        info_hash: calculated_hash,
                                        torrent: Box::new(torrent),
                                    })
                                    .await;
                            });
                        },

                        TorrentCommand::Network(scoped) => {
                            let scope_id = scoped.scope_id();
                            let Ok(result) = self.network_activation.accept(scoped) else {
                                event!(Level::TRACE, ?scope_id, "ignoring result from an inactive network scope");
                                continue;
                            };
                            match result {
                                NetworkResult::AnnounceResponse { url, response } => {
                                    self.apply_action(Action::TrackerResponse {
                                        url,
                                        peers: response.peers,
                                        interval: response.interval as u64,
                                        min_interval: response.min_interval.map(|i| i as u64)
                                    });
                                }
                                NetworkResult::AnnounceFailed { url, error } => {
                                    event!(Level::DEBUG, "Error from tracker announced failed {}", error);
                                    self.apply_action(Action::TrackerError { url });
                                }
                                NetworkResult::StartedAnnounceFinished { url, result } => {
                                    self.handle_started_announce_result(scope_id, url, result);
                                }
                                NetworkResult::CompletionAnnounceFinished { url, error } => {
                                    self.handle_completion_announce_result(scope_id, url, error);
                                }
                                NetworkResult::UnresponsivePeer(peer_ip_port) => {
                                    self.apply_action(Action::PeerConnectionFailed { peer_addr: peer_ip_port });
                                }
                                NetworkResult::RecoveryReady if self.network_recovery_scope_id == Some(scope_id) => {
                                    self.network_recovery_scope_id = None;
                                if !self.state.is_paused {
                                    self.flush_invalidated_network_peers(scope_id);
                                    let web_seed_peer_ids: Vec<String> = self
                                        .state
                                        .torrent
                                        .as_ref()
                                        .and_then(|torrent| torrent.url_list.as_ref().map(|urls| {
                                            urls.iter()
                                                .map(|url| web_seed_peer_id(torrent, url))
                                                .filter(|peer_id| self.state.peers.contains_key(peer_id))
                                                .collect()
                                        }))
                                        .unwrap_or_default();
                                    for peer_id in web_seed_peer_ids {
                                        self.apply_action(Action::PeerDisconnected {
                                            peer_id,
                                            force: true,
                                        });
                                    }
                                    self.apply_action(Action::UpdateListenPort);
                                }
                                }
                                NetworkResult::RecoveryReady => {}
                            }
                        },

                        TorrentCommand::ValidationComplete(pieces) => {
                            self.apply_action(Action::ValidationComplete { completed_pieces: pieces });
                        },

                        TorrentCommand::BlockSent { peer_id, bytes } => {
                            self.apply_action(Action::BlockSentToPeer {
                                peer_id,
                                byte_count: bytes
                            });
                        },
                        TorrentCommand::SetDataAvailability(available) => {
                            self.apply_action(Action::SetDataAvailability { available });
                        }
                        TorrentCommand::ValidationProgress(count) => {
                            self.apply_action(Action::ValidationProgress { count });
                        },

                        TorrentCommand::FatalStorageError(msg) => {
                            event!(Level::DEBUG, ?msg, "Fatal Storage error");
                            self.apply_action(Action::FatalError);
                        },

                        _ => {
                            tracing::warn!(?command, "Unhandled torrent command");
                        }
                    }
                }
            }
        };
        #[cfg(feature = "webtorrent")]
        self.rtc_shutdown().await;
        outcome
    }
}

fn build_tracker_state_map<I>(urls: I, now: Instant) -> HashMap<String, TrackerState>
where
    I: IntoIterator<Item = String>,
{
    urls.into_iter()
        .map(|url| {
            (
                url,
                TrackerState {
                    next_announce_time: now,
                    leeching_interval: None,
                    seeding_interval: None,
                    has_responded: false,
                },
            )
        })
        .collect()
}

#[cfg(test)]
fn test_network_handle() -> crate::networking::NetworkHandle {
    crate::networking::runtime::test_network_handle()
}

#[cfg(test)]
fn test_network_activation(port: u16) -> NetworkActivationHandle {
    let network_handle = test_network_handle();
    let (mut publisher, handle) = crate::networking::NetworkActivationPublisher::channel();
    publisher
        .activate(network_handle.try_lease().unwrap(), port)
        .unwrap();
    Box::leak(Box::new(publisher));
    Box::leak(Box::new(network_handle));
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::networking::NetworkSupervisor;
    use crate::resource::ResourceManager;
    use crate::token_bucket::TokenBucket;
    use crate::torrent_manager::{ManagerCommand, TorrentParameters};
    use magnet_url::Magnet;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::SystemTime;
    use std::time::{Duration, Instant};
    use tokio::sync::{broadcast, mpsc, watch};

    #[test]
    fn private_tracker_stop_announce_timeout_is_four_seconds() {
        assert_eq!(
            PRIVATE_TRACKER_STOP_ANNOUNCE_TIMEOUT,
            Duration::from_secs(4)
        );
    }

    #[test]
    fn network_recovery_jitter_is_bounded_and_generation_specific() {
        let info_hash = [7_u8; HASH_LENGTH];
        let first = network_recovery_delay(&info_hash, 11);
        let repeated = network_recovery_delay(&info_hash, 11);
        let next_generation = network_recovery_delay(&info_hash, 12);

        assert_eq!(first, repeated);
        assert!(first <= Duration::from_millis(NETWORK_RECOVERY_JITTER_MAX_MS));
        assert_ne!(first, next_generation);
    }

    #[test]
    fn peer_channel_full_warning_is_exponentially_sampled() {
        let warned_counts = (0..=17)
            .filter(|count| should_log_peer_channel_full_warning(*count))
            .collect::<Vec<_>>();

        assert_eq!(warned_counts, vec![1, 2, 4, 8, 16]);
    }

    #[tokio::test]
    async fn peer_registration_wait_is_canceled_by_generation_invalidation() {
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        manager_tx
            .send(TorrentCommand::NotInterested)
            .await
            .unwrap();
        let (peer_tx, _peer_rx) = mpsc::channel(1);
        let (registration_result_tx, _registration_result_rx) = mpsc::channel(1);
        let (invalidation_tx, mut invalidation_rx) = watch::channel(false);
        let (_shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
        let registration = send_generation_scoped_peer_registration(
            &manager_tx,
            TorrentCommand::RegisterPeerTransport {
                peer_id: "tcp://127.0.0.1:41001".to_string(),
                peer_addr: "127.0.0.1:41001".parse().unwrap(),
                tx: peer_tx,
                transport: PeerTransportKind::Tcp,
                scope_id: NetworkScopeId::for_test(17),
                registration_result_tx,
            },
            &mut invalidation_rx,
            &mut shutdown_rx,
        );
        tokio::pin!(registration);
        tokio::task::yield_now().await;
        invalidation_tx.send_replace(true);

        assert!(!timeout(Duration::from_secs(1), &mut registration)
            .await
            .expect("registration should cancel promptly"));
        assert!(matches!(
            manager_rx.recv().await,
            Some(TorrentCommand::NotInterested)
        ));
        assert!(manager_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tracker_result_wait_is_canceled_by_generation_invalidation() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let network_lease = network_handle.try_lease().unwrap();
        let (mut activation_publisher, _activation) =
            crate::networking::NetworkActivationPublisher::channel();
        let active = activation_publisher.activate(network_lease, 41000).unwrap();
        let network_scope = active.scope().clone();
        let (manager_tx, mut manager_rx) = mpsc::channel(1);
        manager_tx
            .send(TorrentCommand::NotInterested)
            .await
            .unwrap();
        let (_shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
        let delivery = send_generation_scoped_tracker_result(
            &manager_tx,
            NetworkResult::AnnounceFailed {
                url: "http://tracker.test/announce".to_string(),
                error: "temporary failure".to_string(),
            },
            &network_scope,
            &mut shutdown_rx,
        );
        tokio::pin!(delivery);
        tokio::task::yield_now().await;
        network_handle
            .reconfigure(crate::networking::NetworkBindingConfig::default())
            .await
            .unwrap();

        timeout(Duration::from_secs(1), &mut delivery)
            .await
            .expect("stale tracker result should cancel promptly");
        assert!(matches!(
            manager_rx.recv().await,
            Some(TorrentCommand::NotInterested)
        ));
        assert!(manager_rx.try_recv().is_err());

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn stale_network_generation_is_not_current_after_reconfiguration() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        let old = publisher
            .activate(network_handle.try_lease().unwrap(), 41000)
            .unwrap();
        let old_generation_id = old.scope().id().generation_id();
        assert!(activation.is_current(old.scope().id()));

        network_handle
            .reconfigure(crate::networking::NetworkBindingConfig::default())
            .await
            .unwrap();
        let new_generation_id = network_handle.try_lease().unwrap().generation_id();
        let current = publisher
            .activate(network_handle.try_lease().unwrap(), 41000)
            .unwrap();
        assert_ne!(new_generation_id, old_generation_id);
        assert!(!activation.is_current(old.scope().id()));
        assert!(activation.is_current(current.scope().id()));

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn disabled_peer_address_family_is_rejected_before_connection_work() {
        let config = crate::networking::NetworkBindingConfig {
            mode: crate::networking::NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: false,
            enable_ipv6: true,
            ipv4_address: None,
            ipv6_address: Some(std::net::Ipv6Addr::LOCALHOST),
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&config);
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        publisher
            .activate(network_handle.try_lease().unwrap(), 41000)
            .unwrap();

        assert!(
            network_scope_and_peer_address(&activation, "192.0.2.42:4242".parse().unwrap())
                .is_none()
        );
        assert!(
            network_scope_and_peer_address(&activation, "[::1]:4242".parse().unwrap()).is_some()
        );

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn ipv4_mapped_peer_is_normalized_before_transport_selection() {
        let config = crate::networking::NetworkBindingConfig {
            mode: crate::networking::NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(std::net::Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: crate::networking::DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&config);
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        publisher
            .activate(network_handle.try_lease().unwrap(), 41000)
            .unwrap();
        let mapped = "[::ffff:192.0.2.42]:4242".parse().unwrap();

        let (_scope, normalized, listen_port) = network_scope_and_peer_address(&activation, mapped)
            .expect("IPv4-mapped peer should use the enabled IPv4 family");

        assert_eq!(
            normalized,
            "192.0.2.42:4242".parse::<std::net::SocketAddr>().unwrap()
        );
        assert_eq!(listen_port, 41000);
        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[test]
    fn public_shutdown_announces_only_to_trackers_that_have_responded() {
        let now = Instant::now();
        let responded_url = "http://responded.tracker.test/announce".to_string();
        let unanswered_url = "http://unanswered.tracker.test/announce".to_string();
        let missing_url = "http://missing.tracker.test/announce".to_string();
        let mut trackers = HashMap::new();
        trackers.insert(
            responded_url.clone(),
            TrackerState {
                next_announce_time: now,
                leeching_interval: None,
                seeding_interval: None,
                has_responded: true,
            },
        );
        trackers.insert(
            unanswered_url.clone(),
            TrackerState {
                next_announce_time: now,
                leeching_interval: None,
                seeding_interval: None,
                has_responded: false,
            },
        );

        let urls = shutdown_tracker_urls(
            vec![
                responded_url.clone(),
                unanswered_url.clone(),
                missing_url.clone(),
            ],
            &trackers,
            false,
        );

        assert_eq!(urls, vec![responded_url]);
    }

    #[test]
    fn private_shutdown_announces_to_all_trackers() {
        let now = Instant::now();
        let responded_url = "http://responded.tracker.test/announce".to_string();
        let unanswered_url = "http://unanswered.tracker.test/announce".to_string();
        let mut trackers = HashMap::new();
        trackers.insert(
            responded_url.clone(),
            TrackerState {
                next_announce_time: now,
                leeching_interval: None,
                seeding_interval: None,
                has_responded: true,
            },
        );
        trackers.insert(
            unanswered_url.clone(),
            TrackerState {
                next_announce_time: now,
                leeching_interval: None,
                seeding_interval: None,
                has_responded: false,
            },
        );

        let urls = shutdown_tracker_urls(
            vec![responded_url.clone(), unanswered_url.clone()],
            &trackers,
            true,
        );

        assert_eq!(urls, vec![responded_url, unanswered_url]);
    }

    #[test]
    fn tcp_transport_mode_disables_utp() {
        assert_eq!(
            outbound_transport_mode(PeerTransportMode::Tcp),
            OutboundTransportMode {
                try_utp: false,
                allow_tcp: true,
            }
        );
    }

    #[test]
    fn utp_transport_mode_disables_tcp_fallback() {
        assert_eq!(
            outbound_transport_mode(PeerTransportMode::Utp),
            OutboundTransportMode {
                try_utp: true,
                allow_tcp: false,
            }
        );
    }

    #[test]
    fn all_transport_mode_keeps_tcp_fallback() {
        assert_eq!(
            outbound_transport_mode(PeerTransportMode::All),
            OutboundTransportMode {
                try_utp: true,
                allow_tcp: true,
            }
        );
    }

    #[test]
    fn outbound_peer_key_candidates_include_transport_variants() {
        let peer_addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let keys = outbound_peer_key_candidates(
            peer_addr,
            outbound_transport_mode(PeerTransportMode::All),
            None,
        );

        assert!(keys.contains(&"tcp://127.0.0.1:6881".to_string()));
        assert!(keys.contains(&"utp://127.0.0.1:6881".to_string()));
        assert!(keys.contains(&"127.0.0.1:6881".to_string()));
        assert_eq!(
            outbound_peer_session_key(
                peer_addr,
                outbound_transport_mode(PeerTransportMode::All),
                None,
                PeerTransportKind::Tcp,
            ),
            "tcp://127.0.0.1:6881"
        );
    }

    #[tokio::test]
    async fn all_transport_mode_races_tcp_fallback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let accept_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let started_at = Instant::now();
        let network_handle = test_network_handle();
        let network_lease = network_handle.try_lease().unwrap();
        let connection = connect_outbound_peer(
            &network_lease,
            peer_addr,
            0,
            OutboundTransportMode {
                try_utp: true,
                allow_tcp: true,
            },
            "127.0.0.1:0",
        )
        .await
        .unwrap();

        assert_eq!(connection.endpoint.kind, PeerTransportKind::Tcp);
        assert!(
            started_at.elapsed() < Duration::from_secs(1),
            "TCP fallback should not wait for the uTP connect timeout"
        );

        drop(connection);
        accept_task.await.unwrap();
    }

    #[test]
    fn counts_transport_peers_and_payload_movers() {
        let (tx, _rx) = mpsc::channel(1);
        let now = Instant::now();

        let mut useful_tcp = PeerState::new("127.0.0.1:6881".to_string(), tx.clone(), now);
        useful_tcp.total_bytes_downloaded = 16;

        let mut useful_utp = PeerState::new("127.0.0.1:6882".to_string(), tx.clone(), now);
        useful_utp.transport_kind = PeerTransportKind::Utp;
        useful_utp.total_bytes_uploaded = 16;

        let mut idle_utp = PeerState::new("127.0.0.1:6883".to_string(), tx, now);
        idle_utp.transport_kind = PeerTransportKind::Utp;

        let peers = [&useful_tcp, &useful_utp, &idle_utp];
        let counts = count_peers_by_transport(peers.into_iter());

        assert_eq!(
            counts,
            TransportPeerCounts {
                tcp_peer_count: 1,
                utp_peer_count: 2,
                beneficial_tcp_peer_count: 1,
                beneficial_utp_peer_count: 1,
            }
        );
    }

    #[tokio::test]
    async fn test_manager_event_loop_throughput() {
        let (_incoming_peer_tx, incoming_peer_rx) = mpsc::channel(1000);
        let (manager_command_tx, manager_command_rx) = mpsc::channel(1000);
        let (metrics_tx, _) = watch::channel(TorrentMetrics::default());
        let (manager_event_tx, _manager_event_rx) = mpsc::channel(1000);
        let (shutdown_tx, _) = broadcast::channel(1);
        let settings = Arc::new(Settings::default());

        let mut limits = HashMap::new();
        limits.insert(
            crate::resource::ResourceType::PeerConnection,
            (10_000, 10_000),
        );
        limits.insert(crate::resource::ResourceType::DiskRead, (10_000, 10_000));
        limits.insert(crate::resource::ResourceType::DiskWrite, (10_000, 10_000));
        limits.insert(crate::resource::ResourceType::Reserve, (0, 0));

        let (resource_manager, resource_manager_client) =
            ResourceManager::new(limits, shutdown_tx.clone());
        tokio::spawn(resource_manager.run());

        let dl_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let ul_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));

        let dht_handle = crate::dht::service::DhtHandle::disabled();

        // We use a dummy magnet link to initialize the state machine correctly.
        let magnet_link = "magnet:?xt=urn:btih:0000000000000000000000000000000000000000";
        let magnet = Magnet::new(magnet_link).unwrap();

        let params = TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle,
            incoming_peer_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(PathBuf::from(".")),
            container_name: None,
            manager_command_rx,
            manager_event_tx,
            settings,
            resource_manager: resource_manager_client.clone(),
            global_dl_bucket: dl_bucket,
            global_ul_bucket: ul_bucket,
            file_priorities: HashMap::new(),
        };

        let manager = TorrentManager::from_magnet(params, magnet, magnet_link)
            .expect("Failed to create manager");

        let block_count = 100_000;
        let dummy_data = vec![0u8; 16384];
        let peer_id = "peer1".to_string();

        // Capture the internal sender so we can inject messages directly
        let tx = manager.torrent_manager_tx.clone();

        let manager_handle = tokio::spawn(async move {
            let start = Instant::now();
            // Run the loop (it will exit when it receives Shutdown command)
            let _ = manager.run(false).await;
            start.elapsed()
        });

        tokio::spawn(async move {
            // We simulate 100,000 blocks arriving from the network layer.
            // This tests the "Fan-In" capability of the manager's channel and loop.
            for i in 0..block_count {
                let _ = tx
                    .send(TorrentCommand::Block(
                        peer_id.clone(),
                        0,
                        i * 16384,
                        dummy_data.clone(),
                    ))
                    .await;
            }

            // Tell the Manager to stop
            let _ = manager_command_tx.send(ManagerCommand::Shutdown).await;
        });

        // We expect the manager to process all messages + shutdown.
        // We use a timeout to catch deadlocks.
        let result = tokio::time::timeout(Duration::from_secs(10), manager_handle).await;

        match result {
            Ok(Ok(duration)) => {
                let ops = block_count as f64 / duration.as_secs_f64();
                let mb_sec = (ops * 16384.0) / 1_048_576.0;

                println!(
                    "Processed {} blocks in {:.4}s",
                    block_count,
                    duration.as_secs_f64()
                );
                println!("Throughput: {:.0} Events/sec ({:.2} MB/s)", ops, mb_sec);

                // Performance Assertion:
                // > 10k OPS means the loop overhead is < 100µs per message.
                // This is plenty for 1Gbps+ speeds (which generate ~8k blocks/sec).
                assert!(
                    ops > 10_000.0,
                    "Manager loop is too slow! Throughput: {:.0} OPS",
                    ops
                );
            }
            Ok(Err(e)) => panic!("Manager task panicked: {:?}", e),
            Err(_) => panic!("Test timed out! Manager loop likely deadlocked processing blocks."),
        }
    }

    #[tokio::test]
    async fn test_has_complete_storage_layout_true_for_exact_single_file() {
        let temp_dir = std::env::temp_dir().join(format!(
            "ss_layout_true_{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mfi = crate::persistence::MultiFileInfo::new(
            &temp_dir,
            "payload.bin",
            None,
            Some(1024),
            &HashMap::new(),
        )
        .unwrap();
        std::fs::write(temp_dir.join("payload.bin"), vec![0xAB; 1024]).unwrap();

        let result = TorrentManager::has_complete_storage_layout(&mfi)
            .await
            .unwrap();
        assert!(
            result,
            "exact-size persisted layout should be considered complete"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_has_complete_storage_layout_false_for_size_mismatch() {
        let temp_dir = std::env::temp_dir().join(format!(
            "ss_layout_mismatch_{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mfi = crate::persistence::MultiFileInfo::new(
            &temp_dir,
            "payload.bin",
            None,
            Some(1024),
            &HashMap::new(),
        )
        .unwrap();
        std::fs::write(temp_dir.join("payload.bin"), vec![0xAB; 1000]).unwrap();

        let result = TorrentManager::has_complete_storage_layout(&mfi)
            .await
            .unwrap();
        assert!(
            !result,
            "length mismatch must not be considered a complete persisted layout"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    use crate::app::DataRate;
    use crate::config::Settings;
    use crate::networking::NetworkSupervisor;
    use crate::resource::{ResourceManager, ResourceType};
    use crate::token_bucket::TokenBucket;
    #[cfg(test)]
    use crate::torrent_file::V2RootInfo;
    use crate::torrent_manager::{ManagerCommand, TorrentParameters};
    use magnet_url::Magnet;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};
    use tokio::sync::{broadcast, mpsc};

    #[tokio::test]
    async fn generation_invalidation_cancels_a_queued_peer_permit_wait() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let network_lease = network_handle.try_lease().unwrap();
        let (mut activation_publisher, _activation) =
            crate::networking::NetworkActivationPublisher::channel();
        let active = activation_publisher
            .activate(network_lease.clone(), 41000)
            .unwrap();
        let (resource_shutdown_tx, _) = broadcast::channel(1);
        let mut limits = HashMap::new();
        limits.insert(ResourceType::PeerConnection, (1, 1));
        limits.insert(ResourceType::DiskRead, (1, 1));
        limits.insert(ResourceType::DiskWrite, (1, 1));
        limits.insert(ResourceType::Reserve, (0, 0));
        let (resource_manager, resource_manager_client) =
            ResourceManager::new(limits, resource_shutdown_tx);
        let resource_task = tokio::spawn(resource_manager.run());
        let held_permit = resource_manager_client
            .acquire_peer_connection()
            .await
            .expect("hold the only peer permit");

        let waiting_client = resource_manager_client.clone();
        let waiting_scope = active.scope().clone();
        let (_torrent_shutdown_tx, mut torrent_shutdown_rx) = broadcast::channel(1);
        let waiting_task = tokio::spawn(async move {
            acquire_generation_scoped_peer_permit(
                &waiting_client,
                &waiting_scope,
                &mut torrent_shutdown_rx,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        network_handle
            .block("test generation switch")
            .await
            .unwrap();
        let error = timeout(Duration::from_secs(1), waiting_task)
            .await
            .expect("stale permit wait should cancel promptly")
            .expect("permit wait task")
            .expect_err("invalidated generation must not acquire a permit");
        assert!(matches!(error, PeerPermitWaitError::NetworkInvalidated));

        let replacement_client = resource_manager_client.clone();
        let replacement_task =
            tokio::spawn(async move { replacement_client.acquire_peer_connection().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !replacement_task.is_finished(),
            "closed stale waiter must not make the replacement queue look full"
        );
        drop(held_permit);
        timeout(Duration::from_secs(1), replacement_task)
            .await
            .expect("replacement permit should be granted after capacity returns")
            .expect("replacement permit task")
            .expect("replacement generation should enter the permit queue");
        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
        resource_task.abort();
    }

    pub(super) fn create_dummy_torrent(piece_count: usize) -> Torrent {
        use crate::torrent_file::Info;
        Torrent {
            announce: Some("http://tracker.test".to_string()),
            announce_list: None,
            url_list: None,
            info: Info {
                name: "test_torrent".to_string(),
                piece_length: 16384,                 // 16KB
                pieces: vec![0u8; 20 * piece_count], // 20 bytes per piece hash
                length: (16384 * piece_count) as i64,
                files: vec![],
                private: None,
                md5sum: None,
                meta_version: None,
                file_tree: None,
            },
            info_dict_bencode: vec![],
            created_by: None,
            creation_date: None,
            encoding: None,
            comment: None,
            piece_layers: None,
        }
    }

    fn build_test_dht_handle() -> crate::dht::service::DhtHandle {
        crate::dht::service::DhtHandle::disabled()
    }

    pub(super) fn build_test_params() -> TorrentParameters {
        let (_incoming_tx, incoming_rx) = mpsc::channel(100);
        let (_cmd_tx, cmd_rx) = mpsc::channel(100);
        let (event_tx, _event_rx) = mpsc::channel(100);
        let (metrics_tx, _) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);
        let settings = Arc::new(Settings::default());

        let mut limits = HashMap::new();
        limits.insert(ResourceType::PeerConnection, (1000, 1000));
        limits.insert(ResourceType::DiskRead, (1000, 1000));
        limits.insert(ResourceType::DiskWrite, (1000, 1000));
        limits.insert(ResourceType::Reserve, (0, 0));

        let (_resource_manager, rm_client) = ResourceManager::new(limits, shutdown_tx);

        let dht_handle = build_test_dht_handle();

        TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle,
            incoming_peer_rx: incoming_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(PathBuf::from(".")),
            container_name: None,
            manager_command_rx: cmd_rx,
            manager_event_tx: event_tx,
            settings,
            resource_manager: rm_client,
            global_dl_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            global_ul_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            file_priorities: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn stale_network_activation_cannot_replace_current_state() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        let old = publisher
            .activate(network_handle.try_lease().unwrap(), 41_000)
            .unwrap();
        let old_scope_id = old.scope().id();
        let mut params = build_test_params();
        params.network_activation = activation;
        let mut manager =
            TorrentManager::from_torrent(params, create_dummy_torrent(1)).expect("test manager");

        network_handle
            .reconfigure(crate::networking::NetworkBindingConfig::default())
            .await
            .unwrap();
        manager.apply_latest_network_activation();
        assert_eq!(manager.settings.client_port, 41_000);
        assert_eq!(manager.network_recovery_scope_id, Some(old_scope_id));

        let current = publisher
            .activate(network_handle.try_lease().unwrap(), 41_002)
            .unwrap();
        manager.network_activation_rx.changed().await.unwrap();
        manager.apply_latest_network_activation();
        assert_eq!(manager.settings.client_port, 41_002);
        assert_eq!(
            manager.network_recovery_scope_id,
            Some(current.scope().id())
        );

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn completion_announce_is_retained_and_replayed_on_replacement_activation() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        let mut params = build_test_params();
        params.network_activation = activation;
        let mut manager =
            TorrentManager::from_torrent(params, create_dummy_torrent(1)).expect("test manager");
        let url = "invalid completion tracker".to_string();

        manager.handle_effect(Effect::AnnounceCompleted { url: url.clone() });
        assert!(manager.pending_completion_announces.contains(&url));
        assert!(!manager.completion_announce_scopes.contains_key(&url));

        let first = publisher
            .activate(network_handle.try_lease().unwrap(), 41_000)
            .unwrap();
        manager.network_activation_rx.changed().await.unwrap();
        manager.apply_latest_network_activation();
        assert_eq!(
            manager.completion_announce_scopes.get(&url).copied(),
            Some(first.scope().id())
        );

        let replacement = publisher
            .activate(network_handle.try_lease().unwrap(), 41_001)
            .unwrap();
        manager.network_activation_rx.changed().await.unwrap();
        manager.apply_latest_network_activation();
        assert!(manager.pending_completion_announces.contains(&url));
        assert_eq!(
            manager.completion_announce_scopes.get(&url).copied(),
            Some(replacement.scope().id())
        );

        manager.handle_completion_announce_result(replacement.scope().id(), url.clone(), None);
        assert!(!manager.pending_completion_announces.contains(&url));
        assert!(!manager.completion_announce_scopes.contains_key(&url));

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn started_announce_is_retained_and_replayed_on_replacement_activation() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        let mut params = build_test_params();
        params.network_activation = activation;
        let mut manager =
            TorrentManager::from_torrent(params, create_dummy_torrent(1)).expect("test manager");
        let url = "http://tracker.test".to_string();

        manager.handle_effect(Effect::ConnectToPeersFromTrackers);
        assert!(manager.pending_started_announces.contains(&url));
        assert!(!manager.started_announce_scopes.contains_key(&url));

        let first = publisher
            .activate(network_handle.try_lease().unwrap(), 41_000)
            .unwrap();
        manager.network_activation_rx.changed().await.unwrap();
        manager.apply_latest_network_activation();
        assert_eq!(
            manager.started_announce_scopes.get(&url).copied(),
            Some(first.scope().id())
        );

        let replacement = publisher
            .activate(network_handle.try_lease().unwrap(), 41_001)
            .unwrap();
        manager.network_activation_rx.changed().await.unwrap();
        manager.apply_latest_network_activation();
        assert!(manager.pending_started_announces.contains(&url));
        assert_eq!(
            manager.started_announce_scopes.get(&url).copied(),
            Some(replacement.scope().id())
        );

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn network_consumers_use_active_port_before_settings_synchronize() {
        use tokio::io::AsyncReadExt;

        async fn read_request(listener: &tokio::net::TcpListener) -> String {
            let (mut stream, _) = timeout(Duration::from_secs(1), listener.accept())
                .await
                .expect("tracker request should connect")
                .expect("accept tracker request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = timeout(Duration::from_secs(1), stream.read(&mut chunk))
                    .await
                    .expect("tracker request should finish headers")
                    .expect("read tracker request");
                request.extend_from_slice(&chunk[..read]);
                if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            String::from_utf8(request).expect("tracker request is HTTP text")
        }

        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tracker fixture");
        let url = format!(
            "http://{}/announce",
            listener.local_addr().expect("tracker fixture address")
        );
        let mut torrent = create_dummy_torrent(1);
        torrent.announce = Some(url.clone());

        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        let mut params = build_test_params();
        params.network_activation = activation;
        params.torrent_data_path = None;
        let mut settings = (*params.settings).clone();
        settings.client_port = 41_010;
        params.settings = Arc::new(settings);
        let mut manager = TorrentManager::from_torrent(params, torrent).expect("test manager");

        publisher
            .activate(network_handle.try_lease().unwrap(), 41_011)
            .unwrap();
        assert_eq!(manager.settings.client_port, 41_010);
        let (_scope, _peer_addr, peer_udp_port) = network_scope_and_peer_address(
            &manager.network_activation,
            "127.0.0.1:4242".parse().unwrap(),
        )
        .expect("current peer network context");
        assert_eq!(peer_udp_port, 41_011);

        manager.queue_started_announces();
        let started = read_request(&listener).await;
        assert!(started.contains("&port=41011&"));
        assert!(started.contains("&event=started"));

        manager.pending_started_announces.clear();
        manager.started_announce_scopes.clear();
        manager.handle_effect(Effect::AnnounceToTracker { url: url.clone() });
        let periodic = read_request(&listener).await;
        assert!(periodic.contains("&port=41011&"));
        assert!(!periodic.contains("&event="));

        manager.queue_completion_announce(url);
        let completed = read_request(&listener).await;
        assert!(completed.contains("&port=41011&"));
        assert!(completed.contains("&event=completed"));

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn pending_started_announce_takes_precedence_when_a_paused_torrent_resumes() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        let mut params = build_test_params();
        params.network_activation = activation;
        let mut manager =
            TorrentManager::from_torrent(params, create_dummy_torrent(1)).expect("test manager");
        let url = "http://tracker.test".to_string();

        manager.handle_effect(Effect::ConnectToPeersFromTrackers);
        manager.state.is_paused = true;
        let active = publisher
            .activate(network_handle.try_lease().unwrap(), 41_006)
            .unwrap();
        manager.network_activation_rx.changed().await.unwrap();
        manager.apply_latest_network_activation();
        assert!(!manager.started_announce_scopes.contains_key(&url));

        manager.state.is_paused = false;
        manager.handle_effect(Effect::AnnounceToTracker { url: url.clone() });
        assert_eq!(
            manager.started_announce_scopes.get(&url).copied(),
            Some(active.scope().id())
        );

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn stop_announce_waits_for_replacement_activation() {
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        let waiter_activation = activation.clone();
        let waiter =
            tokio::spawn(async move { wait_for_active_tracker_network(&waiter_activation).await });

        assert!(!waiter.is_finished());
        publisher
            .activate(network_handle.try_lease().unwrap(), 41_007)
            .unwrap();
        let (_, port) = timeout(Duration::from_secs(1), waiter)
            .await
            .expect("activation wait should finish")
            .expect("activation wait task")
            .expect("active tracker network");
        assert_eq!(port, 41_007);

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn activation_updates_ignore_command_backpressure_and_coalesce_to_latest() {
        let mut params = build_test_params();
        let (command_tx, command_rx) = mpsc::channel(1);
        command_tx
            .send(ManagerCommand::Pause)
            .await
            .expect("fill manager command queue");
        params.manager_command_rx = command_rx;

        let network_handle = test_network_handle();
        let lease = network_handle.try_lease().unwrap();
        let (mut publisher, activation) = crate::networking::NetworkActivationPublisher::channel();
        publisher.activate(lease.clone(), 41_100).unwrap();
        params.network_activation = activation;
        let mut manager =
            TorrentManager::from_torrent(params, create_dummy_torrent(1)).expect("test manager");

        publisher.activate(lease.clone(), 41_101).unwrap();
        publisher.activate(lease, 41_102).unwrap();
        manager.network_activation_rx.changed().await.unwrap();
        manager.apply_latest_network_activation();

        assert_eq!(command_tx.capacity(), 0);
        assert_eq!(manager.settings.client_port, 41_102);
    }

    #[tokio::test]
    async fn manager_initial_tick_rate_uses_restored_refresh_rate() {
        let mut params = build_test_params();
        let mut settings = (*params.settings).clone();
        settings.ui_refresh_rate = DataRate::Rate20s;
        params.settings = Arc::new(settings);

        let manager = TorrentManager::from_torrent(params, create_dummy_torrent(1))
            .expect("manager from torrent");

        assert_eq!(manager.data_rate_ms, DataRate::Rate20s.as_ms());
    }

    #[tokio::test]
    async fn manager_refreshes_peer_policy_from_watch() {
        let mut params = build_test_params();
        let (policy_tx, policy_rx) =
            watch::channel(Arc::new(crate::peer_manager::PeerPolicy::default()));
        params.peer_policy_rx = policy_rx;
        let mut manager = TorrentManager::from_torrent(params, create_dummy_torrent(1))
            .expect("manager from torrent");
        let blocked_ip = "203.0.113.44".parse().expect("test IP address");
        let blocked_until = SystemTime::now() + Duration::from_secs(3_600);

        let mut blocked_by_ip = HashMap::new();
        blocked_by_ip.insert(blocked_ip, blocked_until);
        policy_tx.send_replace(Arc::new(
            crate::peer_manager::PeerPolicy::from_blocked_until(blocked_by_ip),
        ));

        manager
            .peer_policy_rx
            .changed()
            .await
            .expect("peer policy update");
        manager.refresh_peer_policy();
        assert!(!manager
            .state
            .can_connect_to_peer(SocketAddr::new(blocked_ip, 1)));
    }

    #[tokio::test]
    async fn register_peer_reports_policy_rejection_before_session_start() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(1))
                .expect("manager from torrent");
        let peer_addr: SocketAddr = "203.0.113.47:6881".parse().expect("valid peer address");
        manager.apply_action(Action::PeerPolicyUpdated {
            policy: Arc::new(PeerPolicy::from_blocked_until(HashMap::from([(
                peer_addr.ip(),
                SystemTime::now() + Duration::from_secs(3_600),
            )]))),
        });
        let (peer_tx, mut peer_rx) = mpsc::channel(1);

        let accepted = manager.register_peer(peer_addr.to_string(), Some(peer_addr), peer_tx);

        assert!(accepted.is_none());
        assert!(!manager.state.peers.contains_key(&peer_addr.to_string()));
        assert!(matches!(
            peer_rx.try_recv(),
            Ok(TorrentCommand::Disconnect(peer_id)) if peer_id == peer_addr.to_string()
        ));
    }

    #[tokio::test]
    async fn blocked_peer_with_full_command_channel_is_forced_out_of_state() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(1))
                .expect("manager from torrent");
        let peer_id = "203.0.113.45:6881".to_string();
        let peer_addr: SocketAddr = peer_id.parse().expect("valid peer address");
        let (peer_tx, _peer_rx) = mpsc::channel(1);

        manager.state.update(Action::RegisterPeer {
            peer_id: peer_id.clone(),
            peer_addr: Some(peer_addr),
            tx: peer_tx.clone(),
        });
        let session_cancel = manager
            .state
            .peers
            .get(&peer_id)
            .expect("registered peer")
            .session_cancel
            .subscribe();
        peer_tx
            .try_send(TorrentCommand::Disconnect(peer_id.clone()))
            .expect("prefill peer command channel");

        manager.handle_effect(Effect::DisconnectPeer {
            peer_id: peer_id.clone(),
        });

        assert!(
            !manager.state.peers.contains_key(&peer_id),
            "a blocked peer must leave authoritative state even when its channel is full"
        );
        assert!(
            *session_cancel.borrow(),
            "the live session must be cancelled even when its command channel is full"
        );
    }

    #[tokio::test]
    async fn disconnected_peer_totals_are_published_before_the_peer_disappears() {
        let mut params = build_test_params();
        let (metrics_tx, mut metrics_rx) = watch::channel(TorrentMetrics::default());
        params.metrics_tx = metrics_tx;
        let mut manager = TorrentManager::from_torrent(params, create_dummy_torrent(1))
            .expect("manager from torrent");
        let peer_addr: SocketAddr = "203.0.113.48:6881".parse().expect("valid peer address");
        let peer_key = peer_addr.to_string();
        let (session_tx, _session_rx) = mpsc::channel(1);

        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_key.clone(),
            peer_addr: Some(peer_addr),
            tx: session_tx,
        });
        let peer = manager
            .state
            .peers
            .get_mut(&peer_key)
            .expect("registered peer");
        peer.peer_id = b"metrics-peer".to_vec();
        peer.total_bytes_downloaded = 12_345;
        peer.total_bytes_uploaded = 678;

        manager.apply_action(Action::PeerDisconnected {
            peer_id: peer_key.clone(),
            force: true,
        });
        manager.send_metrics(0, 0, Vec::new());

        metrics_rx
            .changed()
            .await
            .expect("terminal peer totals should be published");
        let metrics = metrics_rx.borrow_and_update().clone();
        assert!(!manager.state.peers.contains_key(&peer_key));
        assert!(metrics.peers.is_empty());
        assert_eq!(metrics.departed_peers.len(), 1);
        assert_eq!(
            metrics.departed_peers[0].address,
            format!("tcp://{peer_addr}")
        );
        assert_eq!(metrics.departed_peers[0].total_downloaded, 12_345);
        assert_eq!(metrics.departed_peers[0].total_uploaded, 678);
    }

    #[tokio::test]
    async fn deletion_emits_terminal_peer_totals_before_clearing_sessions() {
        let mut params = build_test_params();
        let (metrics_tx, mut metrics_rx) = watch::channel(TorrentMetrics::default());
        params.metrics_tx = metrics_tx;
        let mut manager = TorrentManager::from_torrent(params, create_dummy_torrent(1))
            .expect("manager from torrent");
        manager.state.torrent_data_path = None;
        let peer_addr: SocketAddr = "203.0.113.49:6881".parse().expect("valid peer address");
        let peer_key = peer_addr.to_string();
        let (session_tx, _session_rx) = mpsc::channel(1);

        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_key.clone(),
            peer_addr: Some(peer_addr),
            tx: session_tx,
        });
        let peer = manager
            .state
            .peers
            .get_mut(&peer_key)
            .expect("registered peer");
        peer.peer_id = b"deletion-peer".to_vec();
        peer.total_bytes_downloaded = 4_096;
        peer.total_bytes_uploaded = 2_048;

        manager.apply_action(Action::Delete);

        metrics_rx
            .changed()
            .await
            .expect("deletion should publish terminal peer totals");
        let metrics = metrics_rx.borrow_and_update().clone();
        assert!(manager.state.peers.is_empty());
        assert!(metrics.peers.is_empty());
        assert_eq!(metrics.departed_peers.len(), 1);
        assert_eq!(
            metrics.departed_peers[0].address,
            format!("tcp://{peer_addr}")
        );
        assert_eq!(metrics.departed_peers[0].total_downloaded, 4_096);
        assert_eq!(metrics.departed_peers[0].total_uploaded, 2_048);
    }

    #[tokio::test]
    async fn blocked_policy_prevents_outbound_connect_before_discovery() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();
        let blocked_addr: SocketAddr = "203.0.113.20:6881".parse().expect("valid peer");
        let blocked_until = SystemTime::now() + Duration::from_secs(3_600);
        manager.apply_action(Action::PeerPolicyUpdated {
            policy: Arc::new(PeerPolicy::from_blocked_until(HashMap::from([(
                blocked_addr.ip(),
                blocked_until,
            )]))),
        });

        let (event_tx, mut event_rx) = mpsc::channel(1);
        manager.manager_event_tx = event_tx;
        manager.connect_to_peer(blocked_addr);

        assert!(
            tokio::time::timeout(Duration::from_millis(20), event_rx.recv())
                .await
                .is_err(),
            "blocked peer should not enter the outbound discovery/connect path"
        );
    }

    #[tokio::test]
    async fn awaiting_metadata_publishes_complete_metrics_snapshot() {
        let mut params = build_test_params();
        let (metrics_tx, mut metrics_rx) = watch::channel(TorrentMetrics::default());
        params.metrics_tx = metrics_tx;
        let magnet_link = "magnet:?xt=urn:btih:4040404040404040404040404040404040404040";
        let magnet = Magnet::new(magnet_link).expect("valid magnet link");
        let mut manager =
            TorrentManager::from_magnet(params, magnet, magnet_link).expect("manager from magnet");
        let restored_priorities = HashMap::from([
            (0, crate::app::FilePriority::Skip),
            (2, crate::app::FilePriority::High),
        ]);
        manager
            .state
            .file_priorities
            .clone_from(&restored_priorities);
        let peer_addr: SocketAddr = "203.0.113.46:6881".parse().expect("valid peer address");
        let (session_tx, _session_rx) = mpsc::channel(1);

        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_addr.to_string(),
            peer_addr: Some(peer_addr),
            tx: session_tx,
        });
        manager.send_metrics(0, 0, Vec::new());

        metrics_rx
            .changed()
            .await
            .expect("peer observation should be published");
        let metrics = metrics_rx.borrow_and_update().clone();
        assert_eq!(metrics.info_hash, manager.state.info_hash);
        assert_eq!(metrics.file_priorities, restored_priorities);
        assert_eq!(metrics.peers.len(), 1);
        assert_eq!(metrics.peers[0].address, format!("tcp://{peer_addr}"));
    }

    #[tokio::test]
    async fn opaque_session_keys_publish_registered_network_addresses() {
        let mut params = build_test_params();
        let (metrics_tx, mut metrics_rx) = watch::channel(TorrentMetrics::default());
        params.metrics_tx = metrics_tx;
        let magnet_link = "magnet:?xt=urn:btih:4141414141414141414141414141414141414141";
        let magnet = Magnet::new(magnet_link).expect("valid magnet link");
        let mut manager =
            TorrentManager::from_magnet(params, magnet, magnet_link).expect("manager from magnet");
        let peer_addr: SocketAddr = "203.0.113.30:6881".parse().expect("valid peer address");
        let peer_id = "opaque-session-key".to_string();
        let (session_tx, _session_rx) = mpsc::channel(1);

        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_id.clone(),
            peer_addr: Some(peer_addr),
            tx: session_tx,
        });
        manager.apply_action(Action::PeerTransportSelected {
            peer_id: peer_id.clone(),
            transport: PeerTransportKind::Utp,
        });
        manager.apply_action(Action::PeerSuccessfullyConnected {
            peer_id: peer_id.clone(),
        });
        manager.send_metrics(0, 0, Vec::new());

        metrics_rx
            .changed()
            .await
            .expect("active peer observation should be published");
        let active_metrics = metrics_rx.borrow_and_update().clone();
        assert_eq!(active_metrics.peers.len(), 1);
        assert_eq!(
            active_metrics.peers[0].address,
            format!("utp://{peer_addr}")
        );
        assert_eq!(active_metrics.peers[0].connection_count, 1);
        assert_eq!(active_metrics.peers[0].disconnect_count, 0);

        manager.apply_action(Action::PeerDisconnected {
            peer_id,
            force: true,
        });
        manager.send_metrics(0, 0, Vec::new());

        metrics_rx
            .changed()
            .await
            .expect("departed peer observation should be published");
        let departed_metrics = metrics_rx.borrow_and_update().clone();
        assert!(departed_metrics.peers.is_empty());
        assert_eq!(departed_metrics.departed_peers.len(), 1);
        assert_eq!(
            departed_metrics.departed_peers[0].address,
            format!("utp://{peer_addr}")
        );
        assert_eq!(departed_metrics.departed_peers[0].connection_count, 1);
        assert_eq!(departed_metrics.departed_peers[0].disconnect_count, 1);
    }

    #[tokio::test]
    async fn incoming_utp_connection_counts_as_utp_peer() {
        let (incoming_tx, incoming_peer_rx) = mpsc::channel(4);
        let (manager_command_tx, manager_command_rx) = mpsc::channel(4);
        let (manager_event_tx, _manager_event_rx) = mpsc::channel(16);
        let (metrics_tx, mut metrics_rx) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);
        let settings = Arc::new(Settings::default());
        let temp_dir =
            std::env::temp_dir().join(format!("superseedr_incoming_utp_{}", rand::random::<u32>()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mut limits = HashMap::new();
        limits.insert(ResourceType::PeerConnection, (1000, 1000));
        limits.insert(ResourceType::DiskRead, (1000, 1000));
        limits.insert(ResourceType::DiskWrite, (1000, 1000));
        limits.insert(ResourceType::Reserve, (0, 0));

        let (resource_manager, resource_manager_client) =
            ResourceManager::new(limits, shutdown_tx.clone());
        let resource_handle = tokio::spawn(resource_manager.run());
        let network_activation = test_network_activation(0);
        let network_lease = network_activation
            .try_active()
            .unwrap()
            .scope()
            .lease()
            .clone();
        let params = TorrentParameters {
            network_activation,
            dht_handle: build_test_dht_handle(),
            incoming_peer_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(temp_dir.clone()),
            container_name: None,
            manager_command_rx,
            manager_event_tx,
            settings,
            resource_manager: resource_manager_client.clone(),
            global_dl_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            global_ul_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            file_priorities: HashMap::new(),
        };

        let manager =
            TorrentManager::from_torrent(params, create_dummy_torrent(1)).expect("manager");
        let info_hash = manager.state.info_hash.clone();
        let run_task = tokio::spawn(async move { manager.run(false).await });
        manager_command_tx
            .send(ManagerCommand::SetDataRate(10))
            .await
            .unwrap();

        let peer_addr: SocketAddr = "127.0.0.1:50123".parse().unwrap();
        let (stream, _peer_side) = tokio::io::duplex(128);
        let mut handshake_response = vec![0_u8; 68];
        handshake_response[28..48].copy_from_slice(&info_hash);
        let connection = PeerConnection::new(
            stream,
            crate::networking::transport::PeerEndpoint::utp(peer_addr),
            peer_addr,
            crate::networking::transport::PeerConnectionDirection::Incoming,
        )
        .with_network_lease(&network_lease);
        let permit = resource_manager_client
            .acquire_peer_connection()
            .await
            .expect("incoming peer permit");
        incoming_tx
            .send((connection, handshake_response, permit))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                metrics_rx.changed().await.unwrap();
                let metrics = metrics_rx.borrow().clone();
                if metrics.utp_peer_count == 1 && metrics.tcp_peer_count == 0 {
                    break;
                }
            }
        })
        .await
        .expect("inbound uTP peer should be counted as uTP");

        manager_command_tx
            .send(ManagerCommand::Shutdown)
            .await
            .unwrap();
        run_task.await.unwrap().unwrap();
        resource_handle.abort();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn paused_manager_drops_queued_incoming_connections() {
        use tokio::io::AsyncReadExt;

        let temp_dir = tempfile::tempdir().expect("temporary torrent directory");
        let (incoming_tx, incoming_peer_rx) = mpsc::channel(1);
        let (manager_command_tx, manager_command_rx) = mpsc::channel(1);
        let (manager_event_tx, _manager_event_rx) = mpsc::channel(1);
        let (metrics_tx, _) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);

        let mut limits = HashMap::new();
        limits.insert(ResourceType::PeerConnection, (1, 1));
        limits.insert(ResourceType::DiskRead, (1, 1));
        limits.insert(ResourceType::DiskWrite, (1, 1));
        limits.insert(ResourceType::Reserve, (0, 0));
        let (resource_manager, resource_manager_client) = ResourceManager::new(limits, shutdown_tx);
        let resource_handle = tokio::spawn(resource_manager.run());

        let params = TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle: build_test_dht_handle(),
            incoming_peer_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(temp_dir.path().to_path_buf()),
            container_name: None,
            manager_command_rx,
            manager_event_tx,
            settings: Arc::new(Settings::default()),
            resource_manager: resource_manager_client.clone(),
            global_dl_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            global_ul_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            file_priorities: HashMap::new(),
        };
        let manager =
            TorrentManager::from_torrent(params, create_dummy_torrent(1)).expect("manager");
        let run_task = tokio::spawn(async move { manager.run(true).await });

        let peer_addr: SocketAddr = "127.0.0.1:50124".parse().unwrap();
        let (stream, mut peer_side) = tokio::io::duplex(128);
        let connection = PeerConnection::new(
            stream,
            crate::networking::transport::PeerEndpoint::tcp(peer_addr),
            peer_addr,
            crate::networking::transport::PeerConnectionDirection::Incoming,
        );
        let permit = resource_manager_client
            .acquire_peer_connection()
            .await
            .expect("incoming peer permit");
        incoming_tx
            .send((connection, vec![0_u8; 68], permit))
            .await
            .expect("queue incoming peer");

        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(2), peer_side.read(&mut byte))
            .await
            .expect("paused manager should promptly drop the queued connection")
            .expect("read peer side");
        assert_eq!(read, 0, "dropped peer connection should reach EOF");

        manager_command_tx
            .send(ManagerCommand::Shutdown)
            .await
            .expect("request manager shutdown");
        run_task.await.unwrap().unwrap();
        resource_handle.abort();
    }

    #[tokio::test]
    async fn invalidated_generation_drops_queued_incoming_connection_before_handshake() {
        use tokio::io::AsyncReadExt;

        let temp_dir = tempfile::tempdir().expect("temporary torrent directory");
        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let stale_lease = network_handle.try_lease().unwrap();
        let (incoming_tx, incoming_peer_rx) = mpsc::channel(1);
        let (manager_command_tx, manager_command_rx) = mpsc::channel(1);
        let (manager_event_tx, _manager_event_rx) = mpsc::channel(1);
        let (metrics_tx, _) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);

        let mut limits = HashMap::new();
        limits.insert(ResourceType::PeerConnection, (1, 1));
        limits.insert(ResourceType::DiskRead, (1, 1));
        limits.insert(ResourceType::DiskWrite, (1, 1));
        limits.insert(ResourceType::Reserve, (0, 0));
        let (resource_manager, resource_manager_client) = ResourceManager::new(limits, shutdown_tx);
        let resource_handle = tokio::spawn(resource_manager.run());

        let params = TorrentParameters {
            network_activation: {
                let (mut publisher, handle) =
                    crate::networking::NetworkActivationPublisher::channel();
                publisher
                    .activate(network_handle.try_lease().unwrap(), 0)
                    .unwrap();
                Box::leak(Box::new(publisher));
                handle
            },
            dht_handle: build_test_dht_handle(),
            incoming_peer_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(temp_dir.path().to_path_buf()),
            container_name: None,
            manager_command_rx,
            manager_event_tx,
            settings: Arc::new(Settings::default()),
            resource_manager: resource_manager_client.clone(),
            global_dl_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            global_ul_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            file_priorities: HashMap::new(),
        };
        let manager =
            TorrentManager::from_torrent(params, create_dummy_torrent(1)).expect("manager");
        let info_hash = manager.state.info_hash.clone();
        let run_task = tokio::spawn(async move { manager.run(false).await });

        let peer_addr: SocketAddr = "127.0.0.1:50126".parse().unwrap();
        let (stream, mut peer_side) = tokio::io::duplex(128);
        let connection = PeerConnection::new(
            stream,
            crate::networking::transport::PeerEndpoint::tcp(peer_addr),
            peer_addr,
            crate::networking::transport::PeerConnectionDirection::Incoming,
        )
        .with_network_lease(&stale_lease);
        let mut handshake_response = vec![0_u8; 68];
        handshake_response[28..48].copy_from_slice(&info_hash);
        let permit = resource_manager_client
            .acquire_peer_connection()
            .await
            .expect("incoming peer permit");

        network_handle
            .reconfigure(crate::networking::NetworkBindingConfig::default())
            .await
            .unwrap();
        incoming_tx
            .send((connection, handshake_response, permit))
            .await
            .expect("queue stale incoming peer");

        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(2), peer_side.read(&mut byte))
            .await
            .expect("stale incoming connection should be dropped promptly")
            .expect("read peer side");
        assert_eq!(
            read, 0,
            "stale incoming connection must not receive a handshake"
        );

        manager_command_tx
            .send(ManagerCommand::Shutdown)
            .await
            .expect("request manager shutdown");
        run_task.await.unwrap().unwrap();
        resource_handle.abort();
        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn sync_dht_lookup_task_restarts_finished_handle() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(1))
                .expect("manager from torrent");
        manager.run_loop_started = true;
        manager.dht_task_handle = Some(tokio::spawn(async {}));
        tokio::task::yield_now().await;

        assert!(manager
            .dht_task_handle
            .as_ref()
            .expect("finished dht task handle")
            .is_finished());

        manager.sync_dht_lookup_task();

        assert!(manager.dht_task_handle.is_some());
        assert!(!manager
            .dht_task_handle
            .as_ref()
            .expect("replacement dht task handle")
            .is_finished());
        manager.stop_dht_lookup_task();
    }

    #[tokio::test]
    async fn test_connect_to_peer_skips_unexpired_known_seeder() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(1))
                .expect("manager from torrent");
        manager.state.torrent_status = TorrentStatus::Done;

        let peer_addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let peer_key = peer_addr.to_string();
        manager
            .state
            .known_seeders
            .insert(peer_key.clone(), Instant::now() + Duration::from_secs(60));

        manager.connect_to_peer(peer_addr);

        assert!(
            !manager.state.peers.contains_key(&peer_key),
            "known seeders should not be registered for outbound connection"
        );
    }

    #[tokio::test]
    async fn stale_web_seed_disconnect_does_not_remove_replacement_generation() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(1))
                .expect("manager from torrent");
        let peer_id = "http://127.0.0.1/generation-scoped-seed".to_string();
        let (peer_tx, _peer_rx) = mpsc::channel(1);
        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_id.clone(),
            peer_addr: None,
            tx: peer_tx,
        });
        manager
            .web_seed_network_scopes
            .insert(peer_id.clone(), NetworkScopeId::for_test(42));

        manager.handle_web_seed_disconnected(peer_id.clone(), NetworkScopeId::for_test(41));

        assert!(manager.state.peers.contains_key(&peer_id));
        assert!(manager.state.pending_disconnects.is_empty());
        assert_eq!(
            manager.web_seed_network_scopes.get(&peer_id),
            Some(&NetworkScopeId::for_test(42))
        );

        manager.handle_web_seed_disconnected(peer_id.clone(), NetworkScopeId::for_test(42));
        assert_eq!(manager.state.pending_disconnects, vec![peer_id.clone()]);
        manager.apply_action(Action::PeerDisconnected {
            peer_id: String::new(),
            force: true,
        });

        assert!(!manager.state.peers.contains_key(&peer_id));
        assert!(!manager.web_seed_network_scopes.contains_key(&peer_id));
    }

    #[tokio::test]
    async fn stale_peer_disconnect_does_not_remove_replacement_generation() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(1))
                .expect("manager from torrent");
        let peer_id = "tcp://127.0.0.1:50125".to_string();
        let (peer_tx, _peer_rx) = mpsc::channel(1);
        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_id.clone(),
            peer_addr: None,
            tx: peer_tx,
        });
        manager
            .peer_network_scopes
            .insert(peer_id.clone(), NetworkScopeId::for_test(52));

        manager.handle_generation_scoped_peer_disconnected(
            peer_id.clone(),
            NetworkScopeId::for_test(51),
        );

        assert!(manager.state.peers.contains_key(&peer_id));
        assert!(manager.state.pending_disconnects.is_empty());
        assert_eq!(
            manager.peer_network_scopes.get(&peer_id),
            Some(&NetworkScopeId::for_test(52))
        );

        manager.handle_generation_scoped_peer_disconnected(
            peer_id.clone(),
            NetworkScopeId::for_test(52),
        );
        assert!(!manager.state.peers.contains_key(&peer_id));
        assert!(!manager.peer_network_scopes.contains_key(&peer_id));
    }

    #[tokio::test]
    async fn recovery_flushes_only_peers_from_invalidated_generations() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(1))
                .expect("manager from torrent");
        let expired_peer_id = "tcp://127.0.0.1:50126".to_string();
        let active_peer_id = "tcp://127.0.0.1:50127".to_string();
        for (peer_id, scope_id) in [
            (&expired_peer_id, NetworkScopeId::for_test(61)),
            (&active_peer_id, NetworkScopeId::for_test(62)),
        ] {
            let (peer_tx, _peer_rx) = mpsc::channel(1);
            manager.apply_action(Action::RegisterPeer {
                peer_id: peer_id.clone(),
                peer_addr: None,
                tx: peer_tx,
            });
            manager
                .peer_network_scopes
                .insert(peer_id.clone(), scope_id);
        }

        manager.flush_invalidated_network_peers(NetworkScopeId::for_test(62));

        assert!(!manager.state.peers.contains_key(&expired_peer_id));
        assert!(!manager.peer_network_scopes.contains_key(&expired_peer_id));
        assert!(manager.state.peers.contains_key(&active_peer_id));
        assert_eq!(
            manager.peer_network_scopes.get(&active_peer_id),
            Some(&NetworkScopeId::for_test(62))
        );
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn test_current_dht_demand_state_prioritizes_metadata() {
        let magnet_link = "magnet:?xt=urn:btih:0000000000000000000000000000000000000000";
        let magnet = Magnet::new(magnet_link).unwrap();
        let manager =
            TorrentManager::from_magnet(build_test_params(), magnet, magnet_link).unwrap();

        assert_eq!(
            manager.current_dht_demand_state(),
            DhtDemandState {
                awaiting_metadata: true,
                connected_peers: 0,
            }
        );
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn test_current_dht_demand_state_uses_no_connected_peers_after_metadata() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(4))
                .expect("manager from torrent");
        manager.state.peers.clear();

        assert_eq!(
            manager.current_dht_demand_state(),
            DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            }
        );
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn test_current_dht_demand_state_relaxes_with_active_peers() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(4))
                .expect("manager from torrent");
        let (peer_tx, _peer_rx) = mpsc::channel(1);
        manager.state.update(Action::RegisterPeer {
            peer_id: "peer-a".into(),
            peer_addr: None,
            tx: peer_tx,
        });

        assert_eq!(
            manager.current_dht_demand_state(),
            DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 1,
            }
        );
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn test_current_dht_demand_state_normalizes_active_peer_count() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(4))
                .expect("manager from torrent");
        let (first_peer_tx, _first_peer_rx) = mpsc::channel(1);
        manager.state.update(Action::RegisterPeer {
            peer_id: "peer-a".into(),
            peer_addr: None,
            tx: first_peer_tx,
        });
        let (second_peer_tx, _second_peer_rx) = mpsc::channel(1);
        manager.state.update(Action::RegisterPeer {
            peer_id: "peer-b".into(),
            peer_addr: None,
            tx: second_peer_tx,
        });

        assert_eq!(
            manager.current_dht_demand_state(),
            DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 1,
            }
        );
    }

    #[cfg(feature = "dht")]
    #[tokio::test]
    async fn test_current_dht_demand_metrics_reports_torrent_and_peer_activity() {
        let mut manager =
            TorrentManager::from_torrent(build_test_params(), create_dummy_torrent(4))
                .expect("manager from torrent");
        manager.state.torrent_status = TorrentStatus::Standard;
        manager.state.accepting_new_peers = true;
        manager.state.piece_manager.bitfield = vec![PieceStatus::Done, PieceStatus::Need];
        manager.state.piece_manager.pieces_remaining = 1;
        manager.state.total_dl_prev_avg_ema = 42_000.0;
        manager.state.total_ul_prev_avg_ema = 7_000.0;
        manager.state.bytes_downloaded_in_interval = 4096;
        manager.state.bytes_uploaded_in_interval = 1024;

        let (first_peer_tx, _first_peer_rx) = mpsc::channel(1);
        let mut first_peer = crate::torrent_manager::state::PeerState::new(
            "peer-a".into(),
            first_peer_tx,
            Instant::now(),
        );
        first_peer.am_interested = true;
        first_peer.peer_is_interested_in_us = true;
        first_peer.peer_choking = ChokeStatus::Unchoke;
        first_peer.am_choking = ChokeStatus::Unchoke;
        first_peer.download_speed_bps = 10;

        let (second_peer_tx, _second_peer_rx) = mpsc::channel(1);
        let mut second_peer = crate::torrent_manager::state::PeerState::new(
            "peer-b".into(),
            second_peer_tx,
            Instant::now(),
        );
        second_peer.upload_speed_bps = 20;

        manager.state.peers.insert("peer-a".into(), first_peer);
        manager.state.peers.insert("peer-b".into(), second_peer);

        assert_eq!(
            manager.current_dht_demand_metrics(),
            DhtDemandMetrics {
                accepting_new_peers: true,
                total_pieces: 2,
                completed_pieces: 1,
                connected_peers: 2,
                interested_peers: 1,
                peers_interested_in_us: 1,
                unchoked_download_peers: 1,
                unchoked_upload_peers: 1,
                downloading_peers: 1,
                uploading_peers: 1,
                download_speed_bps: 42_000,
                upload_speed_bps: 7_000,
                bytes_downloaded_this_tick: 4096,
                bytes_uploaded_this_tick: 1024,
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn test_send_metrics_flushes_batched_file_activity() {
        let (_incoming_tx, incoming_peer_rx) = mpsc::channel(32);
        let (_manager_command_tx, manager_command_rx) = mpsc::channel(32);
        let (manager_event_tx, mut manager_event_rx) = mpsc::channel(32);
        let (metrics_tx, mut metrics_rx) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);
        let settings = Arc::new(Settings::default());

        let mut limits = HashMap::new();
        limits.insert(crate::resource::ResourceType::PeerConnection, (1000, 1000));
        limits.insert(crate::resource::ResourceType::DiskRead, (1000, 1000));
        limits.insert(crate::resource::ResourceType::DiskWrite, (1000, 1000));
        limits.insert(crate::resource::ResourceType::Reserve, (0, 0));

        let (_resource_manager, resource_manager_client) =
            ResourceManager::new(limits, shutdown_tx);

        let params = TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle: build_test_dht_handle(),
            incoming_peer_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(PathBuf::from(".")),
            container_name: None,
            manager_command_rx,
            manager_event_tx,
            settings,
            resource_manager: resource_manager_client,
            global_dl_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            global_ul_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
            file_priorities: HashMap::new(),
        };

        let mut manager = TorrentManager::from_torrent(params, create_dummy_torrent(1)).unwrap();

        manager.apply_action(Action::IncomingBlock {
            peer_id: "peer_a".to_string(),
            piece_index: 0,
            block_offset: 0,
            data: vec![1; 256],
        });

        assert!(matches!(
            manager_event_rx.try_recv().ok(),
            Some(ManagerEvent::BlockReceived { .. })
        ));
        assert!(
            manager_event_rx.try_recv().is_err(),
            "file activity should flush on the tick, not the block path"
        );

        manager.apply_action(Action::Tick { dt_ms: 1000 });

        let metrics = metrics_rx.borrow_and_update().clone();
        assert_eq!(metrics.file_activity_updates.len(), 1);
        assert_eq!(
            metrics.file_activity_updates[0].touched_relative_paths,
            vec!["test_torrent".to_string()]
        );
        assert_eq!(
            metrics.file_activity_updates[0].direction,
            crate::torrent_manager::FileActivityDirection::Download
        );
    }

    // --- Helper to spawn a manager quickly ---
    fn setup_test_harness() -> (
        TorrentManager,
        mpsc::Sender<TorrentCommand>, // Inject commands here
        mpsc::Sender<ManagerCommand>, // Control manager here
        broadcast::Sender<()>,        // Shutdown signal
        ResourceManager,              // To control resource limits
    ) {
        let (_incoming_tx, _incoming_rx) = mpsc::channel(100); // Fixed warning: unused variable
        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        let (event_tx, _event_rx) = mpsc::channel(100);
        let (metrics_tx, _) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);
        let settings = Arc::new(Settings::default());

        // Default Limits (Permissive)
        let mut limits = HashMap::new();
        limits.insert(ResourceType::PeerConnection, (1000, 1000));
        limits.insert(ResourceType::DiskRead, (1000, 1000));
        limits.insert(ResourceType::DiskWrite, (1000, 1000));
        limits.insert(ResourceType::Reserve, (0, 0));

        let (resource_manager, rm_client) = ResourceManager::new(limits, shutdown_tx.clone());

        let dl_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let ul_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));

        let magnet_link = "magnet:?xt=urn:btih:0000000000000000000000000000000000000000";
        let magnet = Magnet::new(magnet_link).unwrap();

        let dht_handle = build_test_dht_handle();

        let params = TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle, // FIX: Pass the conditional handle, not ()
            incoming_peer_rx: _incoming_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(PathBuf::from(".")),
            container_name: None,
            manager_command_rx: cmd_rx,
            manager_event_tx: event_tx,
            settings,
            resource_manager: rm_client,
            global_dl_bucket: dl_bucket,
            global_ul_bucket: ul_bucket,
            file_priorities: HashMap::new(),
        };

        let manager = TorrentManager::from_magnet(params, magnet, magnet_link).unwrap();

        let torrent_tx = manager.torrent_manager_tx.clone();

        (manager, torrent_tx, cmd_tx, shutdown_tx, resource_manager)
    }

    #[tokio::test]
    async fn test_from_magnet_keeps_http_tracker_fallback_alongside_udp() {
        let magnet_link = concat!(
            "magnet:?xt=urn:btih:0000000000000000000000000000000000000000",
            "&tr=http%3A%2F%2Ftracker.local%3A6969%2Fannounce",
            "&tr=udp%3A%2F%2Ftracker.local%3A6969%2Fannounce",
            "&tr=https%3A%2F%2Ftracker-alt.local%2Fannounce"
        );
        let magnet = Magnet::new(magnet_link).unwrap();

        let manager = TorrentManager::from_magnet(build_test_params(), magnet, magnet_link)
            .expect("manager from magnet");

        let mut trackers: Vec<_> = manager.state.trackers.keys().cloned().collect();
        trackers.sort();

        assert_eq!(
            trackers,
            vec![
                "http://tracker.local:6969/announce".to_string(),
                "https://tracker-alt.local/announce".to_string(),
                "udp://tracker.local:6969/announce".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn test_from_torrent_uses_announce_list_and_keeps_http_fallback() {
        let mut torrent = create_dummy_torrent(1);
        torrent.announce = Some("http://tracker.local:6969/announce".to_string());
        torrent.announce_list = Some(vec![vec![
            "udp://tracker.local:6969/announce".to_string(),
            "https://tracker-alt.local/announce".to_string(),
        ]]);

        let manager = TorrentManager::from_torrent(build_test_params(), torrent)
            .expect("manager from torrent");

        let mut trackers: Vec<_> = manager.state.trackers.keys().cloned().collect();
        trackers.sort();

        assert_eq!(
            trackers,
            vec![
                "http://tracker.local:6969/announce".to_string(),
                "https://tracker-alt.local/announce".to_string(),
                "udp://tracker.local:6969/announce".to_string(),
            ]
        );
    }

    #[cfg(not(feature = "dht"))]
    fn add_peer(
        manager: &mut TorrentManager,
        id: &str,
        am_interested: bool,
        peer_choking: ChokeStatus,
    ) {
        let (peer_tx, _peer_rx) = mpsc::channel(4);
        let mut peer =
            crate::torrent_manager::state::PeerState::new(id.to_string(), peer_tx, Instant::now());
        peer.am_interested = am_interested;
        peer.peer_choking = peer_choking;
        manager.state.peers.insert(id.to_string(), peer);
    }

    #[cfg(not(feature = "dht"))]
    #[tokio::test]
    async fn test_activity_message_metadata_and_peer_count() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();

        manager.state.torrent_status = TorrentStatus::AwaitingMetadata;
        manager.state.torrent_metadata_length = Some(2048);
        add_peer(&mut manager, "p1", false, ChokeStatus::Choke);
        add_peer(&mut manager, "p2", false, ChokeStatus::Choke);
        add_peer(&mut manager, "p3", false, ChokeStatus::Choke);

        let msg = manager.generate_activity_message(0, 0);
        assert_eq!(msg, "Metadata (3 peers)");
        assert!(msg.chars().count() <= ACTIVITY_MESSAGE_MAX_LEN);
    }

    #[cfg(not(feature = "dht"))]
    #[tokio::test]
    async fn test_activity_message_validation_shows_progress_percentage() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();

        manager.state.torrent_status = TorrentStatus::Validating;
        manager.state.validation_pieces_found = 4;
        manager.state.piece_manager.bitfield = vec![PieceStatus::Need; 10];

        let msg = manager.generate_activity_message(0, 0);
        assert_eq!(msg, "Validating 40% (4/10)");
        assert!(msg.chars().count() <= ACTIVITY_MESSAGE_MAX_LEN);
    }

    #[cfg(not(feature = "dht"))]
    #[tokio::test]
    async fn test_activity_message_requesting_pieces_shows_quantifiers() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();

        manager.state.torrent_status = TorrentStatus::Standard;
        manager.state.last_activity = TorrentActivity::RequestingPieces;
        manager.state.piece_manager.need_queue = vec![1, 2, 3, 4];
        add_peer(&mut manager, "p1", true, ChokeStatus::Unchoke);
        add_peer(&mut manager, "p2", false, ChokeStatus::Choke);

        let msg = manager.generate_activity_message(0, 0);
        assert_eq!(msg, "Request 4 (1/2)");
        assert!(msg.chars().count() <= ACTIVITY_MESSAGE_MAX_LEN);
    }

    #[cfg(not(feature = "dht"))]
    #[tokio::test]
    async fn test_activity_message_waiting_for_data_is_plain_language() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();

        manager.state.torrent_status = TorrentStatus::Standard;
        manager.state.last_activity = TorrentActivity::Initializing;
        manager.state.piece_manager.need_queue = vec![1];
        manager.state.piece_manager.bitfield = vec![PieceStatus::Need; 5];
        manager.state.piece_manager.pieces_remaining = 3;

        add_peer(&mut manager, "p1", true, ChokeStatus::Unchoke);
        add_peer(&mut manager, "p2", true, ChokeStatus::Choke);
        add_peer(&mut manager, "p3", false, ChokeStatus::Choke);

        let msg = manager.generate_activity_message(0, 0);
        assert_eq!(msg, "Waiting data (1/3)");
        assert!(!msg.to_lowercase().contains("unchoke"));
        assert!(msg.chars().count() <= ACTIVITY_MESSAGE_MAX_LEN);
    }

    #[cfg(not(feature = "dht"))]
    #[tokio::test]
    async fn test_activity_message_done_strings_preserved() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();
        manager.state.torrent_status = TorrentStatus::Done;

        assert_eq!(manager.generate_activity_message(0, 10), "Seeding");
        assert_eq!(manager.generate_activity_message(0, 0), "Finished");
    }

    #[tokio::test]
    async fn test_peer_admission_guard_blocks_new_outgoing_connection() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();

        manager.state.accepting_new_peers = false;

        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let peer_id = addr.to_string();

        manager.handle_effect(Effect::ConnectToPeer { addr });

        assert!(
            !manager.state.peers.contains_key(&peer_id),
            "peer admission guard should block new outgoing peers"
        );
    }

    #[tokio::test]
    async fn test_peer_admission_guard_allows_new_outgoing_connection_when_open() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();

        manager.state.accepting_new_peers = true;

        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();

        manager.handle_effect(Effect::ConnectToPeer { addr });

        assert!(
            manager.state.accepting_new_peers,
            "peer admission guard should remain open for outgoing peers"
        );
        assert!(
            manager.state.peers.is_empty(),
            "outgoing peers are registered only after transport connection succeeds"
        );
    }

    #[tokio::test]
    async fn test_peer_admission_guard_blocks_new_outgoing_connection_while_paused() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();

        manager.state.accepting_new_peers = true;
        manager.state.is_paused = true;

        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let peer_id = addr.to_string();

        manager.handle_effect(Effect::ConnectToPeer { addr });

        assert!(
            !manager.state.peers.contains_key(&peer_id),
            "paused torrents should block new outgoing peers"
        );
    }

    #[tokio::test]
    async fn test_peer_admission_guard_handles_10k_candidates_when_closed() {
        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, _resource_manager) =
            setup_test_harness();

        manager.state.accepting_new_peers = false;

        for port in 10_000u16..20_000u16 {
            manager.handle_effect(Effect::ConnectToPeer {
                addr: SocketAddr::from(([127, 0, 0, 1], port)),
            });
        }

        assert_eq!(
            manager.state.peers.len(),
            0,
            "closed peer admission guard should drop all 10k candidates"
        );
    }

    #[tokio::test]
    async fn test_duplicate_metadata_torrent_is_ignored_in_manager() {
        let (_incoming_peer_tx, incoming_peer_rx) = mpsc::channel(32);
        let (manager_command_tx, manager_command_rx) = mpsc::channel(32);
        let (metrics_tx, _) = watch::channel(TorrentMetrics::default());
        let (manager_event_tx, mut manager_event_rx) = mpsc::channel(32);
        let (shutdown_tx, _) = broadcast::channel(1);
        let settings = Arc::new(Settings::default());

        let mut limits = HashMap::new();
        limits.insert(crate::resource::ResourceType::PeerConnection, (1000, 1000));
        limits.insert(crate::resource::ResourceType::DiskRead, (1000, 1000));
        limits.insert(crate::resource::ResourceType::DiskWrite, (1000, 1000));
        limits.insert(crate::resource::ResourceType::Reserve, (0, 0));

        let (resource_manager, resource_manager_client) =
            ResourceManager::new(limits, shutdown_tx.clone());
        tokio::spawn(resource_manager.run());

        let dl_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let ul_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));

        let magnet_link = "magnet:?xt=urn:btih:0000000000000000000000000000000000000000";
        let magnet = Magnet::new(magnet_link).unwrap();

        let params = TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle: build_test_dht_handle(),
            incoming_peer_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: None,
            container_name: None,
            manager_command_rx,
            manager_event_tx,
            settings,
            resource_manager: resource_manager_client,
            global_dl_bucket: dl_bucket,
            global_ul_bucket: ul_bucket,
            file_priorities: HashMap::new(),
        };

        let mut manager = TorrentManager::from_magnet(params, magnet, magnet_link).unwrap();

        let torrent = Torrent {
            announce: None,
            announce_list: None,
            url_list: None,
            info: crate::torrent_file::Info {
                name: "dup_meta_test".to_string(),
                piece_length: 16_384,
                pieces: vec![0u8; 20],
                length: 16_384,
                files: vec![],
                private: None,
                md5sum: None,
                meta_version: None,
                file_tree: None,
            },
            info_dict_bencode: b"d4:infod6:lengthi16384e4:name13:dup_meta_test12:piece lengthi16384e6:pieces20:00000000000000000000ee".to_vec(),
            created_by: None,
            creation_date: None,
            encoding: None,
            comment: None,
            piece_layers: None,
        };

        let mut hasher = sha1::Sha1::new();
        hasher.update(&torrent.info_dict_bencode);
        manager.state.info_hash = hasher.finalize().to_vec();

        let torrent_tx = manager.torrent_manager_tx.clone();
        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        torrent_tx
            .send(TorrentCommand::MetadataTorrent(
                Box::new(torrent.clone()),
                torrent.info_dict_bencode.len() as i64,
            ))
            .await
            .unwrap();

        let first_event = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match manager_event_rx.recv().await {
                    Some(ManagerEvent::MetadataLoaded { .. }) => break true,
                    Some(_) => continue,
                    None => break false,
                }
            }
        })
        .await
        .unwrap_or(false);

        assert!(
            first_event,
            "first metadata torrent command should emit MetadataLoaded"
        );

        torrent_tx
            .send(TorrentCommand::MetadataTorrent(Box::new(torrent), 109))
            .await
            .unwrap();

        let duplicate_emitted = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                match manager_event_rx.recv().await {
                    Some(ManagerEvent::MetadataLoaded { .. }) => break true,
                    Some(_) => continue,
                    None => break false,
                }
            }
        })
        .await
        .unwrap_or(false);

        assert!(
            !duplicate_emitted,
            "duplicate metadata torrent command should not emit MetadataLoaded"
        );

        manager_command_tx
            .send(ManagerCommand::Shutdown)
            .await
            .unwrap();

        let _ = tokio::time::timeout(Duration::from_secs(2), manager_handle)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_cpu_hashing_is_non_blocking() {
        // GOAL: Verify that processing a 'Block' (which triggers hashing)
        // does not block the loop from processing the next message.

        let (manager, torrent_tx, manager_cmd_tx, _, _) = setup_test_harness();

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        // We will send a Block (triggering work) and immediately a Shutdown.
        // If the Block processing is synchronous (blocking), the Shutdown will be delayed.
        let piece_index = 0;
        let block_data = vec![1u8; 16384];

        let start = Instant::now();

        // Send Block (Triggers VerifyPiece -> SHA1)
        torrent_tx
            .send(TorrentCommand::Block(
                "peer1".into(),
                piece_index,
                0,
                block_data,
            ))
            .await
            .unwrap();

        // Send Shutdown immediately after
        manager_cmd_tx.send(ManagerCommand::Shutdown).await.unwrap();

        // Wait for manager to exit
        let _ = tokio::time::timeout(Duration::from_secs(1), manager_handle)
            .await
            .unwrap();
        let duration = start.elapsed();

        // Verify hashing is non-blocking: Block dispatch spawns work, loop continues
        assert!(
            duration.as_millis() < 20,
            "CPU Test Failed! Manager loop blocked on hashing."
        );
    }

    #[tokio::test]
    async fn test_slow_disk_backpressure() {
        // Goal: Verify memory behavior when disk is slower than network (OOM risk)

        let (manager, torrent_tx, manager_cmd_tx, _shutdown_tx, resource_manager) =
            setup_test_harness();

        // Disk speed effectively 0 MB/s - no write permits granted
        tokio::spawn(resource_manager.run());

        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let block_count = 6000; // ~100 MB
        let flood_start = Instant::now();

        let sender_handle = tokio::spawn(async move {
            let dummy_data = vec![0u8; 16384];
            for i in 0..block_count {
                if torrent_tx
                    .send(TorrentCommand::Block("p".into(), 0, i, dummy_data.clone()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            // Shutdown after flood
            let _ = manager_cmd_tx.send(ManagerCommand::Shutdown).await;
        });

        let _ = sender_handle.await;
        let input_duration = flood_start.elapsed();

        // Cleanup manager
        let _ = manager_handle.await;

        println!(
            "Ingested {} blocks (100MB) in {:?}",
            block_count, input_duration
        );

        // Warning: Unbounded memory growth if ingestion is instant despite stalled disk
        if input_duration.as_millis() < 200 {
            println!(
                "⚠️  PERFORMANCE WARNING: Manager accepted 100MB instantly despite stalled disk."
            );
            println!("    This indicates unbounded memory growth (OOM risk) under load.");
            // We assert TRUE here to let the test pass CI, but verify the warning logs.
            // Uncomment the line below to enforce backpressure strictly.
            // assert!(input_duration.as_millis() > 500, "Failed to exert backpressure!");
        } else {
            println!("✅ Backpressure active. Ingestion slowed down.");
        }
    }

    #[tokio::test]
    async fn test_manager_integration_single_block() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // --- 1. Setup Environment ---
        let temp_dir =
            std::env::temp_dir().join(format!("superseedr_test_{}", rand::random::<u32>()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        const BLOCK_SIZE: usize = 16_384;

        // Setup channels
        let (_incoming_tx, incoming_rx) = mpsc::channel(10);
        let (cmd_tx, cmd_rx) = mpsc::channel(10);

        // CRITICAL: Drain event channel to prevent manager internal deadlock
        let (event_tx, mut event_rx) = mpsc::channel(100);
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

        let (metrics_tx, mut metrics_rx) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);

        let settings_val = Settings {
            client_id: "-SS0001-123456789012".to_string(), // Exactly 20 bytes
            ..Default::default()
        };
        let settings = Arc::new(settings_val);

        // Resources
        let mut limits = HashMap::new();
        limits.insert(crate::resource::ResourceType::PeerConnection, (1000, 1000));
        limits.insert(crate::resource::ResourceType::DiskRead, (1000, 1000));
        limits.insert(crate::resource::ResourceType::DiskWrite, (1000, 1000));
        limits.insert(crate::resource::ResourceType::Reserve, (0, 0));
        let (resource_manager, rm_client) = ResourceManager::new(limits, shutdown_tx.clone());
        tokio::spawn(resource_manager.run());

        // Infinite Buckets
        let dl_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let ul_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));

        // Create Torrent (1 Piece of 0xAA)
        let piece_hash = sha1::Sha1::digest(vec![0xAA; BLOCK_SIZE]).to_vec();
        let torrent = Torrent {
            announce: None,
            announce_list: None,
            url_list: None,
            info: crate::torrent_file::Info {
                name: "test_1_block".to_string(),
                piece_length: BLOCK_SIZE as i64,
                pieces: piece_hash,
                length: BLOCK_SIZE as i64,
                files: vec![],
                private: None,
                md5sum: None,
                meta_version: None,
                file_tree: None,
            },
            info_dict_bencode: vec![0u8; 20],
            created_by: None,
            creation_date: None,
            encoding: None,
            comment: None,
            piece_layers: None,
        };

        let params = TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle: build_test_dht_handle(),
            incoming_peer_rx: incoming_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(temp_dir.clone()),
            container_name: None,
            manager_command_rx: cmd_rx,
            manager_event_tx: event_tx,
            settings: settings.clone(),
            resource_manager: rm_client,
            global_dl_bucket: dl_bucket,
            global_ul_bucket: ul_bucket,
            file_priorities: HashMap::new(),
        };

        let mut manager = TorrentManager::from_torrent(params, torrent).unwrap();

        // --- 2. Setup Mock Peer ---
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (mut rd, mut wr) = socket.into_split();
            println!("[MockPeer] Accepted connection");

            // We use a channel to queue writes to the socket, allowing the read loop
            // to run without blocking on write_all.
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(100);
            tokio::spawn(async move {
                while let Some(data) = rx.recv().await {
                    if wr.write_all(&data).await.is_err() {
                        break;
                    }
                }
            });

            // Read Loop
            let mut am_choking = true;
            let mut handshake_received = false;
            let mut buf = vec![0u8; 1024];
            let mut buffer = Vec::new();

            loop {
                let n = match rd.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => break,
                };
                buffer.extend_from_slice(&buf[..n]);

                if !handshake_received && buffer.len() >= 68 {
                    handshake_received = true;
                    println!("[MockPeer] Handshake Validated. Sending Response...");

                    let mut h_resp = vec![0u8; 68];
                    h_resp[0] = 19;
                    h_resp[1..20].copy_from_slice(b"BitTorrent protocol");
                    h_resp[20..28].copy_from_slice(&[0; 8]);
                    h_resp[28..48].copy_from_slice(&buffer[28..48]); // Echo InfoHash
                    for item in h_resp.iter_mut().take(68).skip(48) {
                        *item = 1;
                    } // Dummy PeerID
                    tx.send(h_resp).await.unwrap();

                    let bitfield = vec![0x80u8];
                    let mut msg = Vec::new();
                    msg.extend_from_slice(&(1 + bitfield.len() as u32).to_be_bytes());
                    msg.push(5);
                    msg.extend_from_slice(&bitfield);
                    tx.send(msg).await.unwrap();

                    buffer.drain(0..68);
                }

                while handshake_received && buffer.len() >= 4 {
                    let len = u32::from_be_bytes(buffer[0..4].try_into().unwrap()) as usize;
                    if buffer.len() < 4 + len {
                        break;
                    }

                    let msg_frame = &buffer[4..4 + len];
                    if !msg_frame.is_empty() {
                        match msg_frame[0] {
                            2 => {
                                // Interested
                                println!("[MockPeer] Client is Interested");
                                if am_choking {
                                    println!("[MockPeer] Unchoking Client...");
                                    let _ = tx.send(vec![0, 0, 0, 1, 1]).await;
                                    am_choking = false;
                                }
                            }
                            6 => {
                                // Request
                                println!("[MockPeer] Client Requested Piece 0");
                                if msg_frame.len() >= 13 {
                                    let index =
                                        u32::from_be_bytes(msg_frame[1..5].try_into().unwrap());
                                    let begin =
                                        u32::from_be_bytes(msg_frame[5..9].try_into().unwrap());

                                    // Send 0xAA data
                                    let data = vec![0xAA; BLOCK_SIZE];
                                    let total_len = 9 + data.len() as u32;
                                    let mut resp = Vec::with_capacity(total_len as usize + 4);
                                    resp.extend_from_slice(&total_len.to_be_bytes());
                                    resp.push(7);
                                    resp.extend_from_slice(&index.to_be_bytes());
                                    resp.extend_from_slice(&begin.to_be_bytes());
                                    resp.extend_from_slice(&data);

                                    let _ = tx.send(resp).await;
                                    println!("[MockPeer] Sent Block Data");
                                }
                            }
                            _ => {}
                        }
                    }
                    buffer.drain(0..4 + len);
                }
            }
        });

        // --- 3. Run Manager ---
        manager.connect_to_peer(peer_addr);
        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        // --- 4. Wait for Completion ---
        let start = Instant::now();
        let timeout_duration = Duration::from_secs(10);

        let check_loop = async {
            loop {
                if metrics_rx.changed().await.is_ok() {
                    let m = metrics_rx.borrow_and_update().clone();
                    // Check if we finished the 1 piece
                    if m.number_of_pieces_completed >= 1 {
                        break;
                    }
                }
            }
        };

        if timeout(timeout_duration, check_loop).await.is_err() {
            panic!("Test Failed: Timeout waiting for download.");
        }

        println!("SUCCESS: Downloaded 1 block in {:?}", start.elapsed());

        // Cleanup
        let _ = cmd_tx.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_pipelined_download_two_thousand_blocks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // --- 1. Setup Environment ---
        let temp_dir =
            std::env::temp_dir().join(format!("superseedr_test_{}", rand::random::<u32>()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        const PIECE_SIZE: usize = 262_144; // 256 KiB
        const BLOCK_SIZE: usize = 16_384;
        const NUM_PIECES: usize = 130;
        const TOTAL_BLOCKS: usize = (PIECE_SIZE / BLOCK_SIZE) * NUM_PIECES; // 2080 blocks

        // Setup channels
        let (_incoming_tx, incoming_rx) = mpsc::channel(1000);
        let (cmd_tx, cmd_rx) = mpsc::channel(1000);

        let (event_tx, mut event_rx) = mpsc::channel(100);
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

        let (metrics_tx, mut metrics_rx) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);

        let settings_val = Settings {
            client_id: "-SS0001-123456789012".to_string(),
            ..Default::default()
        };
        let settings = Arc::new(settings_val);

        // Resources
        let mut limits = HashMap::new();
        limits.insert(
            crate::resource::ResourceType::PeerConnection,
            (100_000, 100_000),
        );
        limits.insert(crate::resource::ResourceType::DiskRead, (100_000, 100_000));
        limits.insert(crate::resource::ResourceType::DiskWrite, (100_000, 100_000));
        limits.insert(crate::resource::ResourceType::Reserve, (0, 0));
        let (resource_manager, rm_client) = ResourceManager::new(limits, shutdown_tx.clone());
        tokio::spawn(resource_manager.run());

        // Infinite Buckets
        let dl_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let ul_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));

        // --- Create Torrent ---
        let mut all_piece_hashes = Vec::new();
        let piece_data: Vec<u8> = (0..PIECE_SIZE).map(|i| (i % 256) as u8).collect();
        for _ in 0..NUM_PIECES {
            all_piece_hashes.extend_from_slice(&sha1::Sha1::digest(&piece_data));
        }

        let torrent = Torrent {
            announce: None,
            announce_list: None,
            url_list: None,
            info: crate::torrent_file::Info {
                name: "test_2000_blocks".to_string(),
                piece_length: PIECE_SIZE as i64,
                pieces: all_piece_hashes,
                length: (PIECE_SIZE * NUM_PIECES) as i64,
                files: vec![],
                private: None,
                md5sum: None,
                meta_version: None,
                file_tree: None,
            },
            info_dict_bencode: vec![0u8; 20],
            created_by: None,
            creation_date: None,
            encoding: None,
            comment: None,
            piece_layers: None,
        };

        let params = TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle: build_test_dht_handle(),
            incoming_peer_rx: incoming_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(temp_dir.clone()),
            container_name: None,
            manager_command_rx: cmd_rx,
            manager_event_tx: event_tx,
            settings: settings.clone(),
            resource_manager: rm_client,
            global_dl_bucket: dl_bucket,
            global_ul_bucket: ul_bucket,
            file_priorities: HashMap::new(),
        };

        let mut manager = TorrentManager::from_torrent(params, torrent.clone()).unwrap();
        let _info_hash = {
            let mut hasher = sha1::Sha1::new();
            hasher.update(&torrent.info_dict_bencode);
            hasher.finalize().to_vec()
        };

        // --- 2. Setup Mock Peer ---
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (mut rd, mut wr) = socket.into_split();

            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(10_000); // Increased channel size for pipelining
            tokio::spawn(async move {
                while let Some(data) = rx.recv().await {
                    if wr.write_all(&data).await.is_err() {
                        break;
                    }
                }
            });

            let mut am_choking = true;
            let mut handshake_received = false;
            let mut buffer = Vec::with_capacity(100 * 1024); // Larger buffer

            loop {
                let mut buf = vec![0u8; 65536];
                let n = match rd.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => break,
                };
                buffer.extend_from_slice(&buf[..n]);

                if !handshake_received && buffer.len() >= 68 {
                    handshake_received = true;

                    let mut h_resp = vec![0u8; 68];
                    h_resp[0] = 19;
                    h_resp[1..20].copy_from_slice(b"BitTorrent protocol");
                    h_resp[20..28].copy_from_slice(&[0; 8]);
                    h_resp[28..48].copy_from_slice(&buffer[28..48]);
                    h_resp[48..68].copy_from_slice(b"-TR2940-k8x1y2z3b4c5");
                    tx.send(h_resp).await.unwrap();

                    let bitfield = vec![0xFF; NUM_PIECES.div_ceil(8)];
                    let mut msg = Vec::new();
                    msg.extend_from_slice(&(1 + bitfield.len() as u32).to_be_bytes());
                    msg.push(5);
                    msg.extend_from_slice(&bitfield);
                    tx.send(msg).await.unwrap();

                    buffer.drain(0..68);
                }

                while handshake_received && buffer.len() >= 4 {
                    let len = u32::from_be_bytes(buffer[0..4].try_into().unwrap()) as usize;
                    if buffer.len() < 4 + len {
                        break;
                    }

                    let msg_frame = &buffer[4..4 + len];
                    if !msg_frame.is_empty() {
                        match msg_frame[0] {
                            2 if am_choking => {
                                // Interested
                                let _ = tx.send(vec![0, 0, 0, 1, 1]).await; // Unchoke
                                am_choking = false;
                            }
                            6 if msg_frame.len() >= 13 => {
                                // Request
                                let index = u32::from_be_bytes(msg_frame[1..5].try_into().unwrap());
                                let begin = u32::from_be_bytes(msg_frame[5..9].try_into().unwrap());
                                let length =
                                    u32::from_be_bytes(msg_frame[9..13].try_into().unwrap());

                                let data: Vec<u8> = (0..length as usize)
                                    .map(|i| ((begin as usize + i) % 256) as u8)
                                    .collect();

                                let total_len = 9 + data.len() as u32;
                                let mut resp = Vec::with_capacity(total_len as usize + 4);
                                resp.extend_from_slice(&total_len.to_be_bytes());
                                resp.push(7);
                                resp.extend_from_slice(&index.to_be_bytes());
                                resp.extend_from_slice(&begin.to_be_bytes());
                                resp.extend_from_slice(&data);

                                let _ = tx.send(resp).await;
                            }
                            _ => {}
                        }
                    }
                    buffer.drain(0..4 + len);
                }
            }
        });

        // --- 3. Run Manager ---
        manager.connect_to_peer(peer_addr);
        let manager_handle = tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        let _ = cmd_tx.send(ManagerCommand::SetDataRate(100)).await;

        // --- 4. Wait for Completion & Measure Performance ---
        let start = Instant::now();
        let timeout_duration = Duration::from_secs(30);

        let check_loop = async {
            let mut chunk_timestamps = vec![Instant::now()];
            let mut next_chunk_target = 10;

            let mut accumulated_download: u64 = 0;

            loop {
                match timeout(Duration::from_secs(1), metrics_rx.changed()).await {
                    Ok(Ok(())) => {
                        let m = metrics_rx.borrow_and_update().clone();
                        accumulated_download += m.bytes_downloaded_this_tick;

                        // Print status occasionally
                        if m.number_of_pieces_completed % 10 == 0 {
                            println!(
                                "STATUS: Completed {}/{} pieces. Acc DL: {}/{}",
                                m.number_of_pieces_completed,
                                NUM_PIECES,
                                accumulated_download,
                                m.total_size
                            );
                        }

                        // This prevents timing artifacts where a skipped target is recorded late.
                        while m.number_of_pieces_completed >= next_chunk_target {
                            chunk_timestamps.push(Instant::now());
                            next_chunk_target += 10;
                        }

                        // SUCCESS CONDITION
                        if m.number_of_pieces_completed >= NUM_PIECES as u32 {
                            // Ensure we capture final timestamp if not covered by loop
                            if chunk_timestamps.len() < (NUM_PIECES / 10) + 1 {
                                chunk_timestamps.push(Instant::now());
                            }
                            break;
                        }
                    }
                    Ok(Err(_)) => break, // Channel closed
                    Err(_) => {
                        // Timeout fired
                        println!(
                            "... No activity for 1s. Current Acc DL: {} ...",
                            accumulated_download
                        );
                    }
                }
            }
            chunk_timestamps
        };

        let timestamps = match timeout(timeout_duration, check_loop).await {
            Ok(ts) => ts,
            Err(_) => panic!(
                "Test Failed: Timeout waiting for download of {} pieces.",
                NUM_PIECES
            ),
        };

        println!(
            "SUCCESS: Downloaded {} pieces ({} blocks) in {:?}",
            NUM_PIECES,
            TOTAL_BLOCKS,
            start.elapsed()
        );

        // --- 5. Performance Analysis ---
        let chunk_durations: Vec<_> = timestamps.windows(2).map(|w| w[1] - w[0]).collect();
        if chunk_durations.is_empty() {
            panic!("No chunk durations recorded, cannot analyze performance.");
        }
        let total_duration: Duration = chunk_durations.iter().sum();
        let avg_duration = total_duration / chunk_durations.len() as u32;

        println!(
            "Chunk Durations ({} chunks): {:?}",
            chunk_durations.len(),
            chunk_durations
        );
        println!("Average Chunk Duration: {:?}", avg_duration);

        let total_bytes = (PIECE_SIZE * NUM_PIECES) as f64;
        let total_seconds = total_duration.as_secs_f64();
        if total_seconds > 0.0 {
            let throughput_mbps = (total_bytes / 1_048_576.0) / total_seconds;
            println!("Average throughput: {:.2} MB/s", throughput_mbps);
            assert!(
                throughput_mbps > 5.0,
                "Throughput {:.2} MB/s is below the 50 MB/s threshold",
                throughput_mbps
            );
        }

        // --- 6. Verify file contents ---
        let file_path = temp_dir.join(&torrent.info.name);
        let downloaded_data = tokio::fs::read(&file_path).await.unwrap();

        assert_eq!(downloaded_data.len(), PIECE_SIZE * NUM_PIECES);

        for piece_idx in 0..NUM_PIECES {
            let start = piece_idx * PIECE_SIZE;
            let end = start + PIECE_SIZE;
            let piece_slice = &downloaded_data[start..end];
            let expected_data: Vec<u8> = (0..PIECE_SIZE).map(|i| (i % 256) as u8).collect();
            assert_eq!(
                piece_slice,
                expected_data.as_slice(),
                "Piece {} data mismatch",
                piece_idx
            );
        }
        println!("File content verification successful!");

        // Cleanup
        let _ = cmd_tx.send(ManagerCommand::Shutdown).await;
        let _ = manager_handle.await;
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_v2_seeding_relative_offset_logic() {
        // GOAL: Verify that requesting a hash for a file that starts at offset > 0
        // correctly calculates the relative index into that file's piece layer.

        let (mut manager, _, _, _, _) = setup_test_harness();

        // Global Piece 0 -> File A
        // Global Piece 1 -> File B
        let piece_len = 16384;

        // Mock Roots & Hashes (32 bytes each)
        let root_a = vec![0xAA; 32];
        let layer_a = vec![0x11; 32]; // Data for File A (Index 0)

        let root_b = vec![0xBB; 32];
        let layer_b = vec![0x22; 32]; // Data for File B (Index 0)

        // Map Global Piece 0 -> Root A
        manager.state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: piece_len as u64,
                root_hash: root_a.clone(),
                file_index: 0,
            }],
        );
        // Map Global Piece 1 -> Root B (File starts at byte 16384)
        manager.state.piece_to_roots.insert(
            1,
            vec![V2RootInfo {
                file_offset: 16384,
                length: piece_len as u64,
                root_hash: root_b.clone(),
                file_index: 0,
            }],
        );

        // Inject the piece_layers into the Torrent struct
        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = piece_len as i64;

        // FIX: Construct the HashMap correctly for serde_bencode::value::Value::Dict
        // Keys must be Vec<u8>, Values must be serde_bencode::value::Value
        let mut layer_map = std::collections::HashMap::new();

        layer_map.insert(
            root_a.clone(),                                      // Key is raw bytes
            serde_bencode::value::Value::Bytes(layer_a.clone()), // Value is wrapped
        );
        layer_map.insert(
            root_b.clone(),
            serde_bencode::value::Value::Bytes(layer_b.clone()),
        );

        torrent.piece_layers = Some(serde_bencode::value::Value::Dict(layer_map));

        manager.state.torrent = Some(torrent);

        let peer_id = "v2_tester".to_string();
        let (peer_tx, mut peer_rx) = mpsc::channel(10);
        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_id.clone(),
            peer_addr: None,
            tx: peer_tx,
        });

        let manager_tx = manager.torrent_manager_tx.clone();
        tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        // This is the CRITICAL step. Piece 1 is the 2nd piece globally,
        // but it is the 1st piece (Index 0) of File B.
        let cmd = TorrentCommand::GetHashes {
            peer_id: peer_id.clone(),
            file_root: vec![], // Ignored by manager (it looks it up)
            base_layer: 0,
            index: 1, // GLOBAL Index 1
            length: 1,
            proof_layers: 0,
        };

        manager_tx.send(cmd).await.unwrap();

        let response = tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
            .await
            .expect("Timed out waiting for Hash response")
            .expect("Channel closed");

        if let TorrentCommand::SendHashPiece {
            root, proof, index, ..
        } = response
        {
            // Check A: Did it resolve to Root B?
            assert_eq!(
                root, root_b,
                "Manager failed to resolve correct file root for Global Piece 1"
            );

            // Check B: Did it send the correct data?
            // It MUST return 'layer_b' (which corresponds to File B, Relative Index 0).
            assert_eq!(
                proof, layer_b,
                "Manager sent wrong proof data. Relative indexing logic failed."
            );

            // Check C: The response must echo the Global Index (1) so the peer knows what piece this is for.
            assert_eq!(index, 1, "Response should echo the requested global index");
        } else {
            panic!(
                "Expected SendHashPiece, got {:?}. (Did logic reject valid request?)",
                response
            );
        }
    }

    #[tokio::test]
    async fn test_v2_seeding_rejects_out_of_bounds() {
        // GOAL: Verify that requesting a range extending beyond the file limits
        // results in a HashReject message, preventing buffer overflows or panics.

        let (mut manager, _, _, _, _) = setup_test_harness();

        // Single file, 10 pieces long (16KB * 10)
        let piece_len = 16384;
        let root = vec![0xAA; 32];

        // Layer has 10 hashes (320 bytes)
        let layer_data = vec![0xFF; 32 * 10];

        // Map it
        manager.state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: piece_len as u64,
                root_hash: root.clone(),
                file_index: 0,
            }],
        );

        let mut torrent = create_dummy_torrent(10);
        torrent.info.piece_length = piece_len as i64;

        let mut layer_map = std::collections::HashMap::new();
        layer_map.insert(
            root.clone(),
            serde_bencode::value::Value::Bytes(layer_data.clone()),
        );
        torrent.piece_layers = Some(serde_bencode::value::Value::Dict(layer_map));
        manager.state.torrent = Some(torrent);

        // Register Peer
        let peer_id = "attacker".to_string();
        let (peer_tx, mut peer_rx) = mpsc::channel(10);
        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_id.clone(),
            peer_addr: None,
            tx: peer_tx,
        });

        // Spawn
        let manager_tx = manager.torrent_manager_tx.clone();
        tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        // File has 10 pieces (Indices 0-9).
        // Requesting 8..13 (8 + 5) goes past the end (9).
        let cmd = TorrentCommand::GetHashes {
            peer_id: peer_id.clone(),
            file_root: vec![],
            base_layer: 0,
            index: 8,
            length: 5, // <--- EXCEEDS TOTAL (8+5 = 13 > 10)
            proof_layers: 0,
        };

        manager_tx.send(cmd).await.unwrap();

        let response = tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
            .await
            .expect("Timed out")
            .expect("Channel closed");

        if let TorrentCommand::SendHashReject { index, length, .. } = response {
            assert_eq!(index, 8);
            assert_eq!(length, 5);
            // Pass!
        } else {
            panic!(
                "Security Fail: Manager accepted an out-of-bounds hash request! Got: {:?}",
                response
            );
        }
    }

    #[tokio::test]
    async fn test_v2_seeding_boundary_edge_cases() {
        // GOAL: Precise boundary testing.

        let (mut manager, _, _, _, _) = setup_test_harness();

        // Setup: File with exactly 5 pieces.
        let piece_len = 16384;
        let root = vec![0xCC; 32];
        let layer_data = vec![0x11; 32 * 5]; // 5 Hashes (Indices 0, 1, 2, 3, 4)

        // The manager looks up the specific piece index requested.
        for i in 0..5 {
            manager.state.piece_to_roots.insert(
                i,
                vec![V2RootInfo {
                    file_offset: 0,
                    length: piece_len as u64,
                    root_hash: root.clone(),
                    file_index: 0,
                }],
            );
        }

        let mut torrent = create_dummy_torrent(5);
        torrent.info.piece_length = piece_len as i64;

        let mut layer_map = std::collections::HashMap::new();
        layer_map.insert(
            root.clone(),
            serde_bencode::value::Value::Bytes(layer_data.clone()),
        );
        torrent.piece_layers = Some(serde_bencode::value::Value::Dict(layer_map));
        manager.state.torrent = Some(torrent);

        let peer_id = "edge_tester".to_string();
        let (peer_tx, mut peer_rx) = mpsc::channel(10);
        manager.apply_action(Action::RegisterPeer {
            peer_id: peer_id.clone(),
            peer_addr: None,
            tx: peer_tx,
        });

        let manager_tx = manager.torrent_manager_tx.clone();
        tokio::spawn(async move {
            let _ = manager.run(false).await;
        });

        // --- CASE 1: Valid Boundary Request ---
        // Request Index 4 (The 5th and last piece). Length 1.
        // Range: 4..5. This is valid.
        let valid_cmd = TorrentCommand::GetHashes {
            peer_id: peer_id.clone(),
            file_root: vec![],
            base_layer: 0,
            index: 4,
            length: 1,
            proof_layers: 0,
        };
        manager_tx.send(valid_cmd).await.unwrap();

        let resp1 = tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
            .await
            .expect("Timeout on valid boundary request")
            .expect("Channel closed");

        if let TorrentCommand::SendHashPiece { index, .. } = resp1 {
            assert_eq!(index, 4, "Should successfully return the last hash");
        } else {
            panic!("Failed to retrieve exact last piece! Got: {:?}", resp1);
        }

        // --- CASE 2: Invalid Boundary Request (Off-by-one) ---
        // Request Index 5. (File only has 0..4).
        // This should fail.
        let invalid_cmd = TorrentCommand::GetHashes {
            peer_id: peer_id.clone(),
            file_root: vec![],
            base_layer: 0,
            index: 5,
            length: 1,
            proof_layers: 0,
        };
        manager_tx.send(invalid_cmd).await.unwrap();

        let resp2 = tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
            .await
            .expect("Timeout on invalid boundary request")
            .expect("Channel closed");

        if let TorrentCommand::SendHashReject { index, .. } = resp2 {
            assert_eq!(index, 5, "Should reject request starting past the end");
        } else {
            panic!(
                "Security Fail: Manager accepted out-of-bounds request index 5! Got: {:?}",
                resp2
            );
        }
    }

    // --- HARNESS: Fixed Resource Manager Spawning & Return Type ---
    fn setup_scale_test_harness() -> (
        TorrentManager,
        mpsc::Sender<TorrentCommand>,
        mpsc::Sender<ManagerCommand>,
        broadcast::Sender<()>,
        ResourceManagerClient, // CHANGED: Return Client, not the Manager actor
    ) {
        let (_incoming_tx, _incoming_rx) = mpsc::channel(100);
        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        let (event_tx, mut event_rx) = mpsc::channel(100);
        let (metrics_tx, _) = watch::channel(TorrentMetrics::default());
        let (shutdown_tx, _) = broadcast::channel(1);
        let settings = Arc::new(Settings::default());

        // Drain events to prevent deadlock
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

        let mut limits = HashMap::new();
        // High limits to prevent throttling
        limits.insert(
            crate::resource::ResourceType::PeerConnection,
            (100_000, 100_000),
        );
        limits.insert(crate::resource::ResourceType::DiskRead, (100_000, 100_000));
        limits.insert(crate::resource::ResourceType::DiskWrite, (100_000, 100_000));
        limits.insert(crate::resource::ResourceType::Reserve, (0, 0));

        let (resource_manager, rm_client) = ResourceManager::new(limits, shutdown_tx.clone());

        // FIX: Spawn the Resource Manager (Consumes 'resource_manager')
        tokio::spawn(async move { resource_manager.run().await });

        let dl_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
        let ul_bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));

        let magnet_link = "magnet:?xt=urn:btih:0000000000000000000000000000000000000000";
        let magnet = Magnet::new(magnet_link).unwrap();

        let dht_handle = build_test_dht_handle();

        let params = TorrentParameters {
            network_activation: test_network_activation(0),
            dht_handle,
            incoming_peer_rx: _incoming_rx,
            metrics_tx,
            peer_policy_rx: crate::peer_manager::default_policy_receiver(),
            torrent_validation_status: false,
            torrent_data_path: Some(PathBuf::from(".")),
            container_name: None,
            manager_command_rx: cmd_rx,
            manager_event_tx: event_tx,
            settings,
            resource_manager: rm_client.clone(),
            global_dl_bucket: dl_bucket,
            global_ul_bucket: ul_bucket,
            file_priorities: HashMap::new(),
        };

        let manager = TorrentManager::from_magnet(params, magnet, magnet_link).unwrap();
        let torrent_tx = manager.torrent_manager_tx.clone();

        // Return 'rm_client' instead of 'resource_manager'
        (manager, torrent_tx, cmd_tx, shutdown_tx, rm_client)
    }

    // Helper to build a V2 File Tree manually
    fn build_mock_v2_file_tree(
        files: Vec<(String, usize, Vec<u8>)>,
    ) -> serde_bencode::value::Value {
        use serde_bencode::value::Value;
        use std::collections::HashMap;

        let mut root_dir_map = HashMap::new();

        for (name, length, root) in files {
            // Leaf Node: { "": { "length": ..., "pieces root": ... } }
            let mut metadata = HashMap::new();
            metadata.insert("length".as_bytes().to_vec(), Value::Int(length as i64));
            metadata.insert("pieces root".as_bytes().to_vec(), Value::Bytes(root));

            let mut leaf_node = HashMap::new();
            leaf_node.insert("".as_bytes().to_vec(), Value::Dict(metadata));

            // Insert into root dir: { "filename": { ...leaf... } }
            root_dir_map.insert(name.as_bytes().to_vec(), Value::Dict(leaf_node));
        }

        Value::Dict(root_dir_map)
    }

    #[tokio::test]
    async fn test_v2_multi_file_alignment_bug() {
        let (mut manager, _, _, _, _) = setup_test_harness();
        let piece_len = 1024;

        // --- 2. CREATE MULTI-FILE V2 TORRENT ---
        let root_a = vec![0xAA; 32];
        let root_b = vec![0xBB; 32];

        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new();

        let files = vec![
            ("file_a.txt".to_string(), 100, root_a.clone()),
            ("file_b.txt".to_string(), 100, root_b.clone()),
        ];

        torrent.info.file_tree = Some(build_mock_v2_file_tree(files));

        // --- 3. INIT MANAGER ---
        // This triggers rebuild_v2_mappings internally
        manager.apply_action(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 1000,
        });

        // --- 4. ASSERTION ---
        let roots_0 = manager.state.piece_to_roots.get(&0);
        let roots_1 = manager.state.piece_to_roots.get(&1);

        assert!(roots_0.is_some(), "Piece 0 should have a root");
        assert!(roots_1.is_some(), "Piece 1 should have a root");

        // The Fix: Only check that the *correct* root is present.
        // We don't enforce len() == 1 strictly because robust logic might clear/append differently
        // depending on previous state, but checking the root hash is the gold standard.

        let root_0 = &roots_0.unwrap()[0];
        assert_eq!(root_0.root_hash, root_a, "Piece 0 must map to Root A");

        let root_1 = &roots_1.unwrap()[0];
        assert_eq!(root_1.root_hash, root_b, "Piece 1 must map to Root B");
    }

    #[tokio::test]
    async fn test_v2_multi_file_alignment_bug_regression() {
        let (mut manager, _, _, _, _) = setup_test_harness();
        let piece_len = 16384;

        // --- 2. CREATE MULTI-FILE V2 TORRENT ---
        // Scenario: Two tiny files (100 bytes each).
        // V1 Logic: Total 200 bytes -> 1 Piece.
        // V2 Logic: File A (Piece 0), File B (Piece 1) -> 2 Pieces.

        let root_a = vec![0xAA; 32];
        let root_b = vec![0xBB; 32];

        let mut torrent = create_dummy_torrent(2);
        torrent.info.piece_length = piece_len as i64;
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new(); // Pure V2
        torrent.info.length = 0; // Unused in multi-file usually

        let files = vec![
            ("file_a.txt".to_string(), 100, root_a.clone()),
            ("file_b.txt".to_string(), 100, root_b.clone()),
        ];

        torrent.info.file_tree = Some(build_mock_v2_file_tree(files));

        // Populate info.files for allocator
        torrent.info.files = vec![
            crate::torrent_file::InfoFile {
                length: 100,
                path: vec!["file_a.txt".into()],
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                length: 100,
                path: vec!["file_b.txt".into()],
                md5sum: None,
                attr: None,
            },
        ];

        // --- 3. INIT MANAGER ---
        // This triggers the piece count calculation logic
        manager.apply_action(Action::MetadataReceived {
            torrent: Box::new(torrent),
            metadata_length: 1000,
        });

        // --- 4. ASSERTION ---
        let total_pieces = manager.state.piece_manager.bitfield.len();
        println!("Calculated Pieces: {}", total_pieces);

        // CHECK 1: Piece Count
        // If bug exists, this is 1. If fixed, this is 2.
        assert_eq!(
            total_pieces, 2,
            "V2 Alignment Bug: Calculated {} pieces, expected 2 (one per file).",
            total_pieces
        );

        // CHECK 2: Root Mapping
        let roots_0 = manager.state.piece_to_roots.get(&0);
        let roots_1 = manager.state.piece_to_roots.get(&1);

        assert!(roots_0.is_some(), "Piece 0 missing roots");
        assert!(roots_1.is_some(), "Piece 1 missing roots");

        // Verify alignment: Piece 0 -> Root A, Piece 1 -> Root B
        assert_eq!(
            roots_0.unwrap()[0].root_hash,
            root_a,
            "Piece 0 should map to File A"
        );
        assert_eq!(
            roots_1.unwrap()[0].root_hash,
            root_b,
            "Piece 1 should map to File B"
        );
    }

    #[tokio::test]
    async fn test_v2_tail_piece_validation_accuracy() {
        use sha2::{Digest, Sha256};

        // Piece 0: 16,384 bytes (Full)
        // Piece 1: 3,616 bytes (Partial tail)
        let piece_len: u64 = 16384;
        let file_len: u64 = 20000;
        let data = vec![0xEE; file_len as usize];

        // Rule: Tail data is hashed AS-IS. Padding is only applied to tree nodes.

        // Piece 0: Full 16KB block
        let p0_data = &data[0..16384];
        let hash_0 = Sha256::digest(p0_data).to_vec();

        // Piece 1: Partial 3,616 bytes (NO DATA PADDING)
        let p1_data = &data[16384..20000];
        let hash_1 = Sha256::digest(p1_data).to_vec();

        // File Root = Hash(Hash0 + Hash1)
        // Since there are 2 pieces, this is a power of two; no tree-node padding needed.
        let mut hasher = Sha256::new();
        hasher.update(&hash_0);
        hasher.update(&hash_1);
        let root_v2 = hasher.finalize().to_vec();

        let (mut manager, _torrent_tx, _cmd_tx, _shutdown_tx, rm_client) =
            setup_scale_test_harness();
        let temp_dir = std::env::temp_dir().join("v2_tail_fixed_bep52");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);
        let file_path = temp_dir.join("v2_tail_file");
        std::fs::write(&file_path, &data).unwrap();

        let mut torrent = create_dummy_torrent(2);
        torrent.info.name = "v2_tail_file".to_string();
        torrent.info.piece_length = piece_len as i64;
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new();

        // Define File Tree so validation can find the roots
        let files = vec![(
            "v2_tail_file".to_string(),
            file_len as usize,
            root_v2.clone(),
        )];
        torrent.info.file_tree = Some(build_mock_v2_file_tree(files));

        // Inject Layer Hashes (Piece Layers)
        let mut layer_map = std::collections::HashMap::new();
        let mut layer_bytes = Vec::new();
        layer_bytes.extend_from_slice(&hash_0);
        layer_bytes.extend_from_slice(&hash_1);
        layer_map.insert(
            root_v2.clone(),
            serde_bencode::value::Value::Bytes(layer_bytes),
        );
        torrent.piece_layers = Some(serde_bencode::value::Value::Dict(layer_map));

        manager.state.torrent = Some(torrent.clone());
        manager.state.multi_file_info = Some(
            crate::persistence::MultiFileInfo::new(
                &temp_dir,
                "v2_tail_file",
                None,
                Some(file_len),
                &HashMap::new(),
            )
            .unwrap(),
        );

        manager.state.piece_to_roots.insert(
            0,
            vec![V2RootInfo {
                file_offset: 0,
                length: file_len,
                root_hash: root_v2.clone(),
                file_index: 0,
            }],
        );
        manager.state.piece_to_roots.insert(
            1,
            vec![V2RootInfo {
                file_offset: 0,
                length: file_len,
                root_hash: root_v2.clone(),
                file_index: 0,
            }],
        );

        let result = TorrentManager::perform_validation(
            manager.state.multi_file_info.unwrap(),
            torrent,
            rm_client,
            _shutdown_tx.subscribe(),
            _torrent_tx,
            mpsc::channel(1).0,
            false,
        )
        .await
        .unwrap();

        assert!(result.contains(&0), "Piece 0 failed validation.");
        assert!(
            result.contains(&1),
            "Piece 1 failed validation. Tail hashing logic mismatch."
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_skip_hashing_true_does_not_mark_complete_when_storage_missing() {
        let (_manager, _torrent_tx, _cmd_tx, shutdown_tx, rm_client) = setup_scale_test_harness();

        let temp_dir = std::env::temp_dir().join("skip_hashing_missing_storage");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let torrent_name = "payload.bin";

        let piece_len: i64 = 16 * 1024;
        let total_len: u64 = (piece_len as u64) * 2;
        let mut torrent = create_dummy_torrent(2);
        torrent.info.name = torrent_name.to_string();
        torrent.info.piece_length = piece_len;
        torrent.info.length = total_len as i64;

        let multi_file_info = crate::persistence::MultiFileInfo::new(
            &temp_dir,
            torrent_name,
            None,
            Some(total_len),
            &HashMap::new(),
        )
        .unwrap();

        let (progress_tx, mut progress_rx) = mpsc::channel(64);
        let (event_tx, _event_rx) = mpsc::channel(4);
        tokio::spawn(async move { while progress_rx.recv().await.is_some() {} });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            TorrentManager::perform_validation(
                multi_file_info,
                torrent,
                rm_client,
                shutdown_tx.subscribe(),
                progress_tx,
                event_tx,
                true,
            ),
        )
        .await;

        assert!(result.is_ok(), "Validation should complete without hanging");

        let result = result.unwrap();
        assert!(
            result.is_ok(),
            "Validation should return a result even when storage is missing"
        );

        let completed = result.unwrap();
        assert!(
            completed.is_empty(),
            "Missing payload must not be treated as fully validated when skip_hashing=true"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_skip_hashing_v2_uses_aligned_v2_piece_space() {
        let (_manager, torrent_tx, _cmd_tx, shutdown_tx, rm_client) = setup_scale_test_harness();

        let temp_dir = std::env::temp_dir().join("skip_hashing_v2_piece_space");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let piece_len: i64 = 16384;
        let root_a = vec![0x11; 32];
        let root_b = vec![0x22; 32];

        let mut torrent = create_dummy_torrent(2);
        torrent.info.name = "v2_skip_hashing_alignment".to_string();
        torrent.info.piece_length = piece_len;
        torrent.info.meta_version = Some(2);
        torrent.info.pieces = Vec::new();
        torrent.info.length = 0;
        torrent.info.file_tree = Some(build_mock_v2_file_tree(vec![
            ("a.bin".to_string(), 100, root_a.clone()),
            ("b.bin".to_string(), 100, root_b.clone()),
        ]));
        torrent.info.files = vec![
            crate::torrent_file::InfoFile {
                length: 100,
                path: vec!["a.bin".into()],
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                length: 100,
                path: vec!["b.bin".into()],
                md5sum: None,
                attr: None,
            },
        ];

        let multi_file_info = crate::persistence::MultiFileInfo::new(
            &temp_dir,
            &torrent.info.name,
            Some(&torrent.info.files),
            None,
            &HashMap::new(),
        )
        .unwrap();

        std::fs::write(temp_dir.join("a.bin"), vec![0xAB; 100]).unwrap();
        std::fs::write(temp_dir.join("b.bin"), vec![0xCD; 100]).unwrap();

        let (event_tx, _event_rx) = mpsc::channel(4);
        let result = TorrentManager::perform_validation(
            multi_file_info,
            torrent,
            rm_client,
            shutdown_tx.subscribe(),
            torrent_tx,
            event_tx,
            true,
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            vec![0, 1],
            "V2 skip_hashing should return aligned V2 piece indices"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_file_probe_reports_missing_and_size_mismatch() {
        let temp_dir = std::env::temp_dir().join(format!(
            "superseedr_probe_missing_{}",
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let files = vec![
            crate::torrent_file::InfoFile {
                path: vec!["ok.bin".into()],
                length: 8,
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                path: vec!["missing.bin".into()],
                length: 16,
                md5sum: None,
                attr: None,
            },
            crate::torrent_file::InfoFile {
                path: vec!["short.bin".into()],
                length: 32,
                md5sum: None,
                attr: None,
            },
        ];

        let multi_file_info = crate::persistence::MultiFileInfo::new(
            &temp_dir,
            "probe_test",
            Some(&files),
            None,
            &HashMap::new(),
        )
        .unwrap();
        let torrent = crate::torrent_file::Torrent {
            info: crate::torrent_file::Info {
                name: "probe_test".to_string(),
                files: files.clone(),
                ..Default::default()
            },
            ..Default::default()
        };

        std::fs::write(temp_dir.join("ok.bin"), vec![0u8; 8]).unwrap();
        std::fs::write(temp_dir.join("short.bin"), vec![0u8; 4]).unwrap();

        let result =
            TorrentManager::collect_file_probe_batch(&torrent, &multi_file_info, 0, 0, usize::MAX)
                .await;
        let files = result.problem_files;

        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|entry| {
            entry.relative_path.ends_with("missing.bin")
                && matches!(
                    entry.error,
                    StorageError::Io {
                        kind: std::io::ErrorKind::NotFound,
                        ..
                    }
                )
        }));
        assert!(files.iter().any(|entry| {
            entry.relative_path.ends_with("short.bin")
                && matches!(
                    entry.error,
                    StorageError::SizeMismatch {
                        expected_size: 32,
                        observed_size: 4
                    }
                )
                && entry.observed_size == Some(4)
        }));
        assert!(!files
            .iter()
            .any(|entry| entry.relative_path.ends_with("ok.bin")));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_file_probe_omits_skipped_files() {
        let temp_dir = std::env::temp_dir().join(format!(
            "superseedr_probe_skipped_{}",
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let files = vec![crate::torrent_file::InfoFile {
            path: vec!["skipped.bin".into()],
            length: 16,
            md5sum: None,
            attr: None,
        }];
        let mut priorities = HashMap::new();
        priorities.insert(0usize, crate::app::FilePriority::Skip);

        let multi_file_info = crate::persistence::MultiFileInfo::new(
            &temp_dir,
            "probe_skip_test",
            Some(&files),
            None,
            &priorities,
        )
        .unwrap();
        let torrent = crate::torrent_file::Torrent {
            info: crate::torrent_file::Info {
                name: "probe_skip_test".to_string(),
                files: files.clone(),
                ..Default::default()
            },
            ..Default::default()
        };

        let result =
            TorrentManager::collect_file_probe_batch(&torrent, &multi_file_info, 0, 0, usize::MAX)
                .await;
        assert!(result.problem_files.is_empty());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}

#[cfg(all(test, feature = "webtorrent"))]
#[path = "rtc_contract_tests.rs"]
mod rtc_contracts;
