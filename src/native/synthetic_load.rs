// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::TorrentMetrics;
use crate::config::Settings;
use crate::integrations::cli::{
    SyntheticBenchmarkArgs, SyntheticLoadAddMode, SyntheticLoadArgs, SyntheticLoadMode,
    SyntheticTorrentFormat, SyntheticTransport, SyntheticUdpChaosArgs,
};
use crate::networking::protocol::{generate_message, Message};
use crate::networking::shared_udp::{SharedUdpFamily, SharedUdpHandle, SHARED_UDP_CHAOS_ENV};
use crate::networking::PeerTransportKind;
use crate::networking::{
    NetworkHandle, NetworkLease, NetworkSupervisor, PeerConnection, TcpPeerTransport,
    UtpListenerSet, UtpPeerTransport,
};
use crate::resource::{
    ResourceManager, ResourceManagerClient, ResourceManagerSnapshot, ResourceType, ResourceUsage,
};
use crate::token_bucket::TokenBucket;
use crate::torrent_file::{Info, Torrent};
use crate::torrent_manager::merkle::compute_v2_piece_root;
use crate::torrent_manager::IncomingPeerSession;
use crate::torrent_manager::{
    ManagerCommand, ManagerEvent, SyntheticPeerConnectFailure, TorrentManager, TorrentParameters,
};

use chrono::Local;
use serde::Serialize;
use serde_bencode::value::Value;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, ErrorKind, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket};
use tokio::signal;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

const BLOCK_SIZE: u32 = 16_384;
const SYNTHETIC_BYTE: u8 = 0;
const MANAGER_CHANNEL_SIZE: usize = 10_000;
const EVENT_CHANNEL_SIZE: usize = 100_000;
const CLIENT_ID: &str = "SL000000000000000000";
const LEECHER_REQUEST_BURST: usize = 16;
const ORCHESTRATION_IDLE_TICK: Duration = Duration::from_millis(25);
const MAX_TORRENTS_PER_ORCHESTRATION_TICK: usize = 25;
const MAX_PEERS_PER_ORCHESTRATION_TICK: usize = 1_000;
const SYNTHETIC_PEERS_PER_INCOMING_HUB: usize = 8_000;
const MAX_SYNTHETIC_INCOMING_HUBS: usize = 16;
const SYNTHETIC_PEER_TRANSPORT_ENV: &str = "SUPERSEEDR_PEER_TRANSPORT";
#[cfg(not(target_os = "macos"))]
const SYNTHETIC_LOCAL_PORT_BASE: u16 = 10_000;
#[cfg(not(target_os = "macos"))]
const SYNTHETIC_LOCAL_PORT_SPAN: usize = 30_000;
const BENCHMARK_INTERRUPT_ISSUE: &str = "interrupted by Ctrl+C";

type DynError = Box<dyn Error + Send + Sync>;
type IncomingPeerTx = mpsc::Sender<IncomingPeerSession>;
type IncomingRoutes = Arc<Mutex<HashMap<Vec<u8>, IncomingPeerTx>>>;

#[derive(Default)]
struct SyntheticCounters {
    download_bytes: AtomicU64,
    upload_bytes: AtomicU64,
    seeder_requests: AtomicU64,
    leecher_requests: AtomicU64,
    leecher_pieces: AtomicU64,
    connections: AtomicU64,
    disconnects: AtomicU64,
    protocol_errors: AtomicU64,
    synthetic_seeder_errors: AtomicU64,
    incoming_hub_handshake_errors: AtomicU64,
    incoming_hub_route_misses: AtomicU64,
    incoming_hub_route_send_errors: AtomicU64,
    synthetic_leecher_errors: AtomicU64,
    synthetic_leecher_addr_in_use: AtomicU64,
    synthetic_leecher_addr_not_available: AtomicU64,
    synthetic_leecher_connection_refused: AtomicU64,
    synthetic_leecher_timed_out: AtomicU64,
    synthetic_leecher_other_io: AtomicU64,
    synthetic_leecher_non_io: AtomicU64,
    manager_peer_connected: AtomicU64,
    manager_peer_disconnected: AtomicU64,
    outbound_connect_attempts: AtomicU64,
    outbound_connect_established: AtomicU64,
    outbound_connect_failed: AtomicU64,
    outbound_connect_tcp_attempts: AtomicU64,
    outbound_connect_tcp_established: AtomicU64,
    outbound_connect_tcp_failed: AtomicU64,
    outbound_connect_utp_attempts: AtomicU64,
    outbound_connect_utp_established: AtomicU64,
    outbound_connect_utp_failed: AtomicU64,
    outbound_connect_quic_attempts: AtomicU64,
    outbound_connect_quic_established: AtomicU64,
    outbound_connect_quic_failed: AtomicU64,
    outbound_permit_timeout: AtomicU64,
    outbound_permit_manager_shutdown: AtomicU64,
    outbound_permit_queue_full: AtomicU64,
    outbound_connect_timeout: AtomicU64,
    outbound_connection_refused: AtomicU64,
    outbound_connection_reset: AtomicU64,
    outbound_connection_aborted: AtomicU64,
    outbound_addr_in_use: AtomicU64,
    outbound_addr_not_available: AtomicU64,
    outbound_timed_out: AtomicU64,
    outbound_other_io: AtomicU64,
    outbound_session_failed: AtomicU64,
    manager_block_received: AtomicU64,
    manager_block_sent: AtomicU64,
    disk_read_started: AtomicU64,
    disk_read_finished: AtomicU64,
    disk_write_started: AtomicU64,
    disk_write_finished: AtomicU64,
}

#[derive(Clone)]
struct HarnessContext {
    network_handle: NetworkHandle,
    network_activation: crate::networking::NetworkActivationHandle,
    _network_activation_publisher: Arc<crate::networking::NetworkActivationPublisher>,
    event_tx: mpsc::Sender<ManagerEvent>,
    resource_client: ResourceManagerClient,
    global_dl_bucket: Arc<TokenBucket>,
    global_ul_bucket: Arc<TokenBucket>,
    counters: Arc<SyntheticCounters>,
    shutdown_tx: broadcast::Sender<()>,
    client_port: u16,
}

#[derive(Clone)]
struct SyntheticTorrentSpec {
    index: usize,
    name: String,
    total_size: u64,
    piece_size: u64,
    piece_count: usize,
    info_hash: Vec<u8>,
    torrent: Torrent,
}

struct ManagerRuntime {
    command_tx: mpsc::Sender<ManagerCommand>,
    metrics_rx: watch::Receiver<TorrentMetrics>,
    handle: JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>,
}

struct SyntheticRunCleanup {
    managers: Vec<ManagerRuntime>,
    peer_handles: Vec<JoinHandle<()>>,
    harness_shutdown_tx: broadcast::Sender<()>,
    resource_shutdown_tx: broadcast::Sender<()>,
    resource_handle: JoinHandle<()>,
    event_handle: JoinHandle<()>,
    cleaned: bool,
}

impl SyntheticRunCleanup {
    fn new(
        harness_shutdown_tx: broadcast::Sender<()>,
        resource_shutdown_tx: broadcast::Sender<()>,
        resource_handle: JoinHandle<()>,
        event_handle: JoinHandle<()>,
    ) -> Self {
        Self {
            managers: Vec::new(),
            peer_handles: Vec::new(),
            harness_shutdown_tx,
            resource_shutdown_tx,
            resource_handle,
            event_handle,
            cleaned: false,
        }
    }

    async fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;

        shutdown_managers(&mut self.managers).await;
        let _ = self.harness_shutdown_tx.send(());
        let _ = self.resource_shutdown_tx.send(());
        for handle in &self.peer_handles {
            handle.abort();
        }
        for handle in &mut self.peer_handles {
            let _ = handle.await;
        }
        self.resource_handle.abort();
        let _ = (&mut self.resource_handle).await;
        self.event_handle.abort();
        let _ = (&mut self.event_handle).await;
    }

    async fn fail<T>(&mut self, error: impl Into<DynError>) -> Result<T, DynError> {
        let error = error.into();
        self.cleanup().await;
        Err(error)
    }
}

#[derive(Clone, Copy, Default)]
struct OrchestrationProgress {
    active_torrents: usize,
    active_peers: usize,
}

struct OrchestrationBatch {
    managers: Vec<ManagerRuntime>,
    peer_handles: Vec<JoinHandle<()>>,
    progress: OrchestrationProgress,
}

enum OrchestrationUpdate {
    Batch(OrchestrationBatch),
    Done(OrchestrationProgress),
    Error(String),
}

#[derive(Clone)]
struct SyntheticIncomingHub {
    port: u16,
    transport: SyntheticTransport,
    routes: IncomingRoutes,
}

impl SyntheticIncomingHub {
    fn register(&self, info_hash: Vec<u8>, tx: IncomingPeerTx) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.insert(info_hash, tx);
        }
    }

    fn transport_for_peer(&self, peer_index: usize) -> SyntheticTransport {
        match self.transport {
            SyntheticTransport::Tcp => SyntheticTransport::Tcp,
            SyntheticTransport::Utp => SyntheticTransport::Utp,
            SyntheticTransport::All if peer_index.is_multiple_of(2) => SyntheticTransport::Tcp,
            SyntheticTransport::All => SyntheticTransport::Utp,
        }
    }

    fn addr_for_peer(&self, peer_index: usize, transport: SyntheticTransport) -> SocketAddr {
        match transport {
            SyntheticTransport::Tcp => synthetic_single_listener_addr(peer_index, self.port),
            SyntheticTransport::Utp => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port),
            SyntheticTransport::All => {
                self.addr_for_peer(peer_index, self.transport_for_peer(peer_index))
            }
        }
    }
}

#[derive(Clone)]
enum SyntheticSeederHub {
    #[cfg(not(target_os = "macos"))]
    SinglePort {
        port: u16,
    },
    SharedUtp {
        port: u16,
    },
    #[cfg(target_os = "macos")]
    PeerPorts {
        ports: Arc<[u16]>,
    },
    #[cfg(not(target_os = "macos"))]
    MixedSingleTcpSharedUtp {
        tcp_port: u16,
        utp_port: u16,
    },
    #[cfg(target_os = "macos")]
    MixedPeerTcpSharedUtp {
        tcp_ports: Arc<[u16]>,
        utp_port: u16,
    },
}

impl SyntheticSeederHub {
    fn addr_for_peer(&self, peer_index: usize) -> Result<SocketAddr, DynError> {
        match self {
            #[cfg(not(target_os = "macos"))]
            Self::SinglePort { port } => Ok(synthetic_loopback_addr(peer_index, *port)),
            #[cfg(target_os = "macos")]
            Self::PeerPorts { ports } => {
                let port = ports.get(peer_index).copied().ok_or_else(|| {
                    format!("missing synthetic seeder listener for peer index {peer_index}")
                })?;
                Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            }
            Self::SharedUtp { port } => Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *port)),
            #[cfg(not(target_os = "macos"))]
            Self::MixedSingleTcpSharedUtp { tcp_port, utp_port } => {
                if peer_index.is_multiple_of(2) {
                    Ok(synthetic_single_listener_addr(peer_index, *tcp_port))
                } else {
                    Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *utp_port))
                }
            }
            #[cfg(target_os = "macos")]
            Self::MixedPeerTcpSharedUtp {
                tcp_ports,
                utp_port,
            } => {
                if peer_index.is_multiple_of(2) {
                    let port = tcp_ports.get(peer_index).copied().ok_or_else(|| {
                        format!("missing synthetic TCP seeder listener for peer index {peer_index}")
                    })?;
                    Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
                } else {
                    Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *utp_port))
                }
            }
        }
    }

    fn synthetic_peer_key(&self, peer_index: usize) -> Option<String> {
        match self {
            Self::SharedUtp { port } => Some(format!("synthetic-utp-{port}:{peer_index}")),
            #[cfg(not(target_os = "macos"))]
            Self::MixedSingleTcpSharedUtp { utp_port, .. } if !peer_index.is_multiple_of(2) => {
                Some(format!("synthetic-utp-{utp_port}:{peer_index}"))
            }
            #[cfg(target_os = "macos")]
            Self::MixedPeerTcpSharedUtp { utp_port, .. } if !peer_index.is_multiple_of(2) => {
                Some(format!("synthetic-utp-{utp_port}:{peer_index}"))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct RunTopology {
    download_peers: usize,
    upload_peers: usize,
}

#[derive(Clone, Copy)]
struct AddPlan {
    mode: SyntheticLoadAddMode,
    interval: Duration,
    burst_size: usize,
}

impl AddPlan {
    fn from_args(args: &SyntheticLoadArgs) -> Self {
        Self {
            mode: args.add_mode,
            interval: Duration::from_millis(args.add_interval_ms),
            burst_size: args.add_burst_size,
        }
    }

    fn target_added(self, elapsed: Duration, total_torrents: usize) -> usize {
        match self.mode {
            SyntheticLoadAddMode::Upfront => total_torrents,
            SyntheticLoadAddMode::Burst => total_torrents,
            SyntheticLoadAddMode::Staggered => {
                let completed_intervals = elapsed.as_millis() / self.interval.as_millis().max(1);
                let batches_due = completed_intervals as usize + 1;
                batches_due
                    .saturating_mul(self.burst_size)
                    .min(total_torrents)
            }
        }
    }

    fn scheduled_elapsed_for_index(self, index: usize) -> Duration {
        match self.mode {
            SyntheticLoadAddMode::Upfront | SyntheticLoadAddMode::Burst => Duration::ZERO,
            SyntheticLoadAddMode::Staggered => {
                duration_mul(self.interval, index / self.burst_size.max(1))
            }
        }
    }
}

fn duration_mul(duration: Duration, multiplier: usize) -> Duration {
    let millis = duration.as_millis().saturating_mul(multiplier as u128);
    Duration::from_millis(millis.min(u64::MAX as u128) as u64)
}

fn expected_active_peers(
    add_plan: AddPlan,
    peer_plan: AddPlan,
    topology: RunTopology,
    total_torrents: usize,
    elapsed: Duration,
) -> usize {
    let target_torrents = add_plan.target_added(elapsed, total_torrents);
    (0..target_torrents)
        .map(|torrent_index| {
            let added_at = add_plan.scheduled_elapsed_for_index(torrent_index);
            let peer_elapsed = elapsed.checked_sub(added_at).unwrap_or_default();
            let download_peers =
                peer_count_for_torrent(topology.download_peers, total_torrents, torrent_index);
            let upload_peers =
                peer_count_for_torrent(topology.upload_peers, total_torrents, torrent_index);
            peer_plan.target_added(peer_elapsed, download_peers)
                + peer_plan.target_added(peer_elapsed, upload_peers)
        })
        .sum()
}

fn peer_count_for_torrent(peers: usize, total_torrents: usize, torrent_index: usize) -> usize {
    if total_torrents == 0 || torrent_index >= peers {
        return 0;
    }
    1 + (peers - 1 - torrent_index) / total_torrents
}

struct AddContext {
    specs: Arc<[SyntheticTorrentSpec]>,
    topology: RunTopology,
    download_root: PathBuf,
    upload_root: PathBuf,
    download_seeder_hub: Option<SyntheticSeederHub>,
    upload_incoming_hubs: Vec<SyntheticIncomingHub>,
    harness: HarnessContext,
    plan: AddPlan,
    peer_plan: AddPlan,
    peer_ramps: Vec<PeerRamp>,
    leecher_pipeline: usize,
    next_torrent: usize,
}

impl AddContext {
    async fn add_due_torrents(
        &mut self,
        elapsed: Duration,
        managers: &mut Vec<ManagerRuntime>,
        peer_handles: &mut Vec<JoinHandle<()>>,
        max_to_add: usize,
    ) -> Result<(), DynError> {
        let target = self.plan.target_added(elapsed, self.specs.len());
        self.add_until(target, elapsed, managers, peer_handles, max_to_add)
            .await
    }

    async fn add_until(
        &mut self,
        target: usize,
        elapsed: Duration,
        managers: &mut Vec<ManagerRuntime>,
        peer_handles: &mut Vec<JoinHandle<()>>,
        max_to_add: usize,
    ) -> Result<(), DynError> {
        let target = target.min(self.specs.len());
        let mut added = 0usize;
        while self.next_torrent < target && added < max_to_add {
            let spec = &self.specs[self.next_torrent];
            if self.topology.download_peers > 0 {
                let setup = start_download_torrent(
                    spec,
                    self.specs.len(),
                    self.topology.download_peers,
                    &self.download_root,
                    self.download_seeder_hub
                        .as_ref()
                        .ok_or("missing synthetic seeder hub for download side")?,
                    &self.harness,
                    elapsed,
                )
                .await?;
                managers.extend(setup.managers);
                peer_handles.extend(setup.peer_handles);
                self.peer_ramps.extend(setup.peer_ramps);
            }
            if self.topology.upload_peers > 0 {
                let incoming_hub = self
                    .upload_incoming_hubs
                    .get(self.next_torrent % self.upload_incoming_hubs.len().max(1))
                    .cloned()
                    .ok_or("missing synthetic incoming hub for upload side")?;
                let setup = start_upload_torrent(
                    spec,
                    self.specs.len(),
                    self.topology.upload_peers,
                    UploadStartContext {
                        data_root: &self.upload_root,
                        incoming_hub: &incoming_hub,
                        harness: &self.harness,
                        leecher_pipeline: self.leecher_pipeline,
                        added_at: elapsed,
                    },
                )
                .await?;
                managers.extend(setup.managers);
                peer_handles.extend(setup.peer_handles);
                self.peer_ramps.extend(setup.peer_ramps);
            }
            self.next_torrent += 1;
            added += 1;
        }
        Ok(())
    }

    async fn add_due_peers(
        &mut self,
        elapsed: Duration,
        peer_handles: &mut Vec<JoinHandle<()>>,
        max_to_add: usize,
    ) -> Result<(), DynError> {
        let mut remaining = max_to_add;
        for ramp in &mut self.peer_ramps {
            if remaining == 0 {
                break;
            }
            let added = ramp
                .add_due_peers(
                    elapsed,
                    self.peer_plan,
                    &self.harness,
                    peer_handles,
                    remaining,
                )
                .await?;
            remaining = remaining.saturating_sub(added);
        }
        Ok(())
    }

    fn active_peers(&self) -> usize {
        self.peer_ramps.iter().map(PeerRamp::active_peers).sum()
    }

    fn progress(&self) -> OrchestrationProgress {
        OrchestrationProgress {
            active_torrents: self.next_torrent,
            active_peers: self.active_peers(),
        }
    }
}

enum PeerRampRole {
    DownloadSeeder {
        command_tx: mpsc::Sender<ManagerCommand>,
        seeder_hub: SyntheticSeederHub,
    },
    UploadLeecher {
        incoming_hub: SyntheticIncomingHub,
        leecher_pipeline: usize,
    },
}

struct PeerRamp {
    spec: SyntheticTorrentSpec,
    peer_indices: Vec<usize>,
    next_peer: usize,
    added_at: Duration,
    role: PeerRampRole,
}

impl PeerRamp {
    async fn add_due_peers(
        &mut self,
        elapsed: Duration,
        plan: AddPlan,
        harness: &HarnessContext,
        peer_handles: &mut Vec<JoinHandle<()>>,
        max_to_add: usize,
    ) -> Result<usize, DynError> {
        let peer_elapsed = elapsed.checked_sub(self.added_at).unwrap_or_default();
        let target = plan.target_added(peer_elapsed, self.peer_indices.len());
        let mut added = 0usize;
        while self.next_peer < target && added < max_to_add {
            let peer_index = self.peer_indices[self.next_peer];
            match &self.role {
                PeerRampRole::DownloadSeeder {
                    command_tx,
                    seeder_hub,
                } => {
                    let addr = seeder_hub.addr_for_peer(peer_index)?;
                    let command = match seeder_hub.synthetic_peer_key(peer_index) {
                        Some(peer_key) => ManagerCommand::ConnectToSyntheticPeer { addr, peer_key },
                        None => ManagerCommand::ConnectToPeer(addr),
                    };
                    command_tx.send(command).await.map_err(|_| -> DynError {
                        "failed to schedule synthetic peer connection".into()
                    })?;
                }
                PeerRampRole::UploadLeecher {
                    incoming_hub,
                    leecher_pipeline,
                } => {
                    let transport = incoming_hub.transport_for_peer(peer_index);
                    let addr = incoming_hub.addr_for_peer(peer_index, transport);
                    let handle = tokio::spawn(run_synthetic_leecher(
                        harness.network_handle.clone(),
                        self.spec.clone(),
                        peer_index,
                        addr,
                        transport,
                        *leecher_pipeline,
                        harness.counters.clone(),
                        harness.shutdown_tx.subscribe(),
                    ));
                    peer_handles.push(handle);
                }
            }
            self.next_peer += 1;
            added += 1;
        }
        Ok(added)
    }

    fn active_peers(&self) -> usize {
        self.next_peer
    }
}

#[derive(Serialize)]
struct SyntheticSample {
    elapsed_ms: u128,
    phase: &'static str,
    active_torrents: u64,
    active_peers: u64,
    target_torrents: u64,
    target_peers: u64,
    torrent_add_lag: u64,
    peer_add_lag: u64,
    sample_delay_ms: u64,
    download_bytes_total: u64,
    upload_bytes_total: u64,
    download_bps: u64,
    upload_bps: u64,
    manager_download_bps: u64,
    manager_upload_bps: u64,
    completed_pieces: u64,
    total_pieces: u64,
    connected_peers_reported: u64,
    seeder_requests: u64,
    leecher_requests: u64,
    leecher_pieces: u64,
    connections: u64,
    disconnects: u64,
    protocol_errors: u64,
    protocol_error_detail: ProtocolErrorSample,
    manager_peer_connected: u64,
    manager_peer_disconnected: u64,
    outbound_connect: OutboundConnectSample,
    manager_block_received: u64,
    manager_block_sent: u64,
    disk_read_started: u64,
    disk_read_finished: u64,
    disk_write_started: u64,
    disk_write_finished: u64,
    resources: ResourceSampleSet,
}

#[derive(Serialize)]
struct SyntheticSummary {
    run_id: String,
    mode: String,
    transport: String,
    torrent_format: String,
    utp_chaos: Option<String>,
    add_mode: String,
    peer_add_mode: String,
    torrents: usize,
    torrents_added: usize,
    peers_added: usize,
    requested_peers: usize,
    download_peers: usize,
    upload_peers: usize,
    add_interval_ms: u64,
    add_burst_size: usize,
    peer_add_interval_ms: u64,
    peer_add_burst_size: usize,
    size_per_torrent_bytes: u64,
    piece_size_bytes: u64,
    duration_secs: u64,
    warmup_secs: u64,
    measured_secs: f64,
    max_torrent_add_lag: usize,
    max_peer_add_lag: usize,
    max_sample_delay_ms: u64,
    download_bytes: u64,
    upload_bytes: u64,
    avg_download_bps: u64,
    avg_upload_bps: u64,
    avg_download_mbps: f64,
    avg_upload_mbps: f64,
    completed_pieces: u64,
    total_pieces: u64,
    seeder_requests: u64,
    leecher_requests: u64,
    leecher_pieces: u64,
    connections: u64,
    disconnects: u64,
    protocol_errors: u64,
    protocol_error_detail: ProtocolErrorSample,
    manager_peer_connected: u64,
    manager_peer_disconnected: u64,
    outbound_connect: OutboundConnectSample,
    manager_block_received: u64,
    manager_block_sent: u64,
    disk_read_started: u64,
    disk_read_finished: u64,
    disk_write_started: u64,
    disk_write_finished: u64,
    output_dir: PathBuf,
    interrupted: bool,
}

#[derive(Serialize)]
struct BenchmarkSummary {
    run_id: String,
    transport: String,
    torrent_format: String,
    utp_chaos: Option<String>,
    interrupted: bool,
    disk_budget_bytes: u64,
    preferred_size_per_torrent_bytes: u64,
    piece_size_bytes: u64,
    max_torrents: usize,
    max_peers: usize,
    planned_steps: usize,
    keep_output: bool,
    report: BenchmarkReport,
    profiles: Vec<BenchmarkProfileSummary>,
    output_dir: PathBuf,
}

#[derive(Serialize)]
struct BenchmarkReport {
    interrupted: bool,
    runtime_secs: f64,
    runtime: String,
    planned_steps: usize,
    steps_run: usize,
    retry_attempts: usize,
    transient_issue_attempts: usize,
    recovered_after_retry_steps: usize,
    clean_steps: usize,
    issue_steps: usize,
    configured_max_torrents: usize,
    configured_max_peers: usize,
    disk_budget_bytes: u64,
    preferred_size_per_torrent_bytes: u64,
    piece_size_bytes: u64,
    torrent_format: String,
    issue_retries: usize,
    retry_delay_ms: u64,
    peer_connection_limit_policy: String,
    os_limit_note: String,
    scenarios: Vec<BenchmarkScenarioReport>,
}

#[derive(Serialize)]
struct BenchmarkScenarioReport {
    mode: String,
    verdict: String,
    capacity_estimate: String,
    clean_torrents: usize,
    clean_peers: usize,
    clean_disk_working_set_bytes: u64,
    clean_size_per_torrent_bytes: u64,
    first_issue_torrents: Option<usize>,
    first_issue_peers: Option<usize>,
    first_issue: Option<String>,
    likely_bottleneck: String,
    runtime_secs: f64,
    steps_run: usize,
    retry_attempts: usize,
    transient_issue_attempts: usize,
    recovered_after_retry_steps: usize,
    planned_steps: usize,
    peak_download_bps: u64,
    peak_upload_bps: u64,
    observed_disk_read_bytes_per_sec: u64,
    observed_disk_write_bytes_per_sec: u64,
    disk_read_ops_per_sec: f64,
    disk_write_ops_per_sec: f64,
    max_sample_delay_ms: u64,
    protocol_errors: u64,
    outbound_failed: u64,
    outbound_permit_timeout: u64,
    peer_connection_limit: usize,
    disk_read_permits: usize,
    disk_write_permits: usize,
}

#[derive(Serialize)]
struct BenchmarkProfileSummary {
    mode: String,
    planned_steps: usize,
    final_torrents: usize,
    final_peers: usize,
    final_size_per_torrent_bytes: u64,
    final_estimated_disk_bytes: u64,
    metrics: BenchmarkProfileMetrics,
    last_clean: Option<BenchmarkStepSummary>,
    first_issue: Option<BenchmarkStepSummary>,
    steps: Vec<BenchmarkStepSummary>,
}

#[derive(Clone, Serialize)]
struct BenchmarkProfileMetrics {
    steps_run: usize,
    retry_attempts: usize,
    transient_issue_attempts: usize,
    recovered_after_retry_steps: usize,
    final_issue_steps: usize,
    clean_steps: usize,
    issue_steps: usize,
    total_measured_secs: f64,
    total_download_bytes: u64,
    total_upload_bytes: u64,
    max_download_bps: u64,
    max_upload_bps: u64,
    max_sample_delay_ms: u64,
    estimated_disk_high_water_bytes: u64,
    protocol_errors: u64,
    protocol_error_detail: ProtocolErrorSample,
    outbound_failed: u64,
    outbound_permit_timeout: u64,
    outbound_connect: OutboundConnectSample,
    synthetic_leecher_errors: u64,
    seeder_requests: u64,
    leecher_requests: u64,
    leecher_pieces: u64,
    connections: u64,
    disconnects: u64,
    manager_peer_connected: u64,
    manager_peer_disconnected: u64,
    manager_block_received: u64,
    manager_block_sent: u64,
    disk_read_started: u64,
    disk_read_finished: u64,
    disk_write_started: u64,
    disk_write_finished: u64,
    completed_pieces: u64,
    total_pieces: u64,
    data_removed_steps: usize,
    data_kept_steps: usize,
}

#[derive(Clone, Serialize)]
struct BenchmarkStepSummary {
    step: usize,
    planned_steps: usize,
    attempt: usize,
    max_attempts: usize,
    will_retry: bool,
    retry_delay_ms: u64,
    mode: String,
    torrents: usize,
    peers: usize,
    size_per_torrent_bytes: u64,
    estimated_disk_bytes: u64,
    estimated_final_disk_bytes: u64,
    disk_budget_bytes: u64,
    measured_secs: f64,
    wall_secs: f64,
    eta: BenchmarkEta,
    download_bytes: u64,
    upload_bytes: u64,
    avg_download_bps: u64,
    avg_upload_bps: u64,
    avg_download_mbps: f64,
    avg_upload_mbps: f64,
    torrents_added: usize,
    peers_added: usize,
    requested_peers: usize,
    max_peer_add_lag: usize,
    max_sample_delay_ms: u64,
    protocol_errors: u64,
    protocol_error_detail: ProtocolErrorSample,
    outbound_failed: u64,
    outbound_permit_timeout: u64,
    outbound_connect: OutboundConnectSample,
    synthetic_leecher_errors: u64,
    seeder_requests: u64,
    leecher_requests: u64,
    leecher_pieces: u64,
    connections: u64,
    disconnects: u64,
    manager_peer_connected: u64,
    manager_peer_disconnected: u64,
    manager_block_received: u64,
    manager_block_sent: u64,
    disk_read_started: u64,
    disk_read_finished: u64,
    disk_write_started: u64,
    disk_write_finished: u64,
    completed_pieces: u64,
    total_pieces: u64,
    error: Option<String>,
    issues: Vec<String>,
    summary_path: Option<PathBuf>,
    samples_path: Option<PathBuf>,
    data_removed: bool,
}

#[derive(Clone, Default, Serialize)]
struct BenchmarkEta {
    current_scenario_remaining_steps: usize,
    full_benchmark_remaining_steps: usize,
    current_scenario_eta_secs: f64,
    full_benchmark_eta_secs: f64,
    average_step_wall_secs: f64,
    elapsed_wall_secs: f64,
}

#[derive(Clone)]
struct BenchmarkStepTiming {
    wall_secs: f64,
    eta: BenchmarkEta,
}

struct BenchmarkAttemptContext {
    attempt: usize,
    max_attempts: usize,
    will_retry: bool,
    retry_delay_ms: u64,
    timing: BenchmarkStepTiming,
}

#[derive(Clone, Default, Serialize)]
struct OutboundConnectSample {
    attempts: u64,
    established: u64,
    failed: u64,
    by_transport: Vec<OutboundConnectTransportSample>,
    permit_timeout: u64,
    permit_manager_shutdown: u64,
    permit_queue_full: u64,
    connect_timeout: u64,
    connection_refused: u64,
    connection_reset: u64,
    connection_aborted: u64,
    addr_in_use: u64,
    addr_not_available: u64,
    timed_out: u64,
    other_io: u64,
    session_failed: u64,
}

#[derive(Clone, Default, Serialize)]
struct OutboundConnectTransportSample {
    transport: &'static str,
    attempts: u64,
    established: u64,
    failed: u64,
}

#[derive(Clone, Default, Serialize)]
struct ProtocolErrorSample {
    synthetic_seeder: u64,
    incoming_hub_handshake: u64,
    incoming_hub_route_miss: u64,
    incoming_hub_route_send: u64,
    synthetic_leecher: u64,
    synthetic_leecher_addr_in_use: u64,
    synthetic_leecher_addr_not_available: u64,
    synthetic_leecher_connection_refused: u64,
    synthetic_leecher_timed_out: u64,
    synthetic_leecher_other_io: u64,
    synthetic_leecher_non_io: u64,
}

#[derive(Clone)]
struct BenchmarkStepPlan {
    step: usize,
    planned_steps: usize,
    torrents: usize,
    peers: usize,
    size_per_torrent_bytes: u64,
    estimated_disk_bytes: u64,
    estimated_final_disk_bytes: u64,
    disk_budget_bytes: u64,
}

struct BenchmarkRunProgress {
    remaining_planned_steps: usize,
    completed_steps: usize,
    elapsed_wall_secs: f64,
}

impl BenchmarkRunProgress {
    fn new(total_planned_steps: usize) -> Self {
        Self {
            remaining_planned_steps: total_planned_steps,
            completed_steps: 0,
            elapsed_wall_secs: 0.0,
        }
    }

    fn record_step(
        &mut self,
        wall_secs: f64,
        skipped_steps: usize,
        current_scenario_remaining_steps: usize,
        added_retry_attempts: usize,
    ) -> BenchmarkStepTiming {
        self.completed_steps = self.completed_steps.saturating_add(1);
        self.elapsed_wall_secs += wall_secs;
        self.remaining_planned_steps = self
            .remaining_planned_steps
            .saturating_sub(1usize.saturating_add(skipped_steps))
            .saturating_add(added_retry_attempts);
        let average_step_wall_secs = if self.completed_steps == 0 {
            0.0
        } else {
            self.elapsed_wall_secs / self.completed_steps as f64
        };

        BenchmarkStepTiming {
            wall_secs,
            eta: BenchmarkEta {
                current_scenario_remaining_steps,
                full_benchmark_remaining_steps: self.remaining_planned_steps,
                current_scenario_eta_secs: average_step_wall_secs
                    * current_scenario_remaining_steps as f64,
                full_benchmark_eta_secs: average_step_wall_secs
                    * self.remaining_planned_steps as f64,
                average_step_wall_secs,
                elapsed_wall_secs: self.elapsed_wall_secs,
            },
        }
    }
}

#[derive(Default, Serialize)]
struct ResourceSampleSet {
    peer_connection: ResourceSample,
    disk_read: ResourceSample,
    disk_write: ResourceSample,
}

#[derive(Default, Serialize)]
struct ResourceSample {
    limit: usize,
    in_use: usize,
    queued: usize,
    max_queue_size: usize,
}

struct SyntheticTransportEnvGuard {
    previous_peer_transport: Option<String>,
}

struct SharedUdpChaosEnvGuard {
    previous_chaos: Option<String>,
}

impl SyntheticTransportEnvGuard {
    fn new(transport: SyntheticTransport) -> Self {
        let guard = Self {
            previous_peer_transport: std::env::var(SYNTHETIC_PEER_TRANSPORT_ENV).ok(),
        };

        std::env::set_var(SYNTHETIC_PEER_TRANSPORT_ENV, transport.as_str());

        guard
    }
}

impl SharedUdpChaosEnvGuard {
    fn new(chaos: SyntheticUdpChaosArgs) -> Self {
        let guard = Self {
            previous_chaos: std::env::var(SHARED_UDP_CHAOS_ENV).ok(),
        };

        match shared_udp_chaos_env_value(chaos) {
            Some(value) => std::env::set_var(SHARED_UDP_CHAOS_ENV, value),
            None => std::env::remove_var(SHARED_UDP_CHAOS_ENV),
        }

        guard
    }
}

impl Drop for SyntheticTransportEnvGuard {
    fn drop(&mut self) {
        restore_env_var(
            SYNTHETIC_PEER_TRANSPORT_ENV,
            self.previous_peer_transport.as_deref(),
        );
    }
}

impl Drop for SharedUdpChaosEnvGuard {
    fn drop(&mut self) {
        restore_env_var(SHARED_UDP_CHAOS_ENV, self.previous_chaos.as_deref());
    }
}

fn shared_udp_chaos_env_value(chaos: SyntheticUdpChaosArgs) -> Option<String> {
    if chaos.utp_chaos_loss_ppm == 0
        && chaos.utp_chaos_duplicate_ppm == 0
        && chaos.utp_chaos_corrupt_ppm == 0
        && chaos.utp_chaos_reorder_ppm == 0
        && chaos.utp_chaos_max_delay_ms == 0
    {
        return None;
    }

    Some(format!(
        "seed={},loss_ppm={},duplicate_ppm={},corrupt_ppm={},reorder_ppm={},max_delay_ms={}",
        chaos.utp_chaos_seed,
        chaos.utp_chaos_loss_ppm,
        chaos.utp_chaos_duplicate_ppm,
        chaos.utp_chaos_corrupt_ppm,
        chaos.utp_chaos_reorder_ppm,
        chaos.utp_chaos_max_delay_ms,
    ))
}

fn restore_env_var(name: &str, value: Option<&str>) {
    match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

pub async fn run(args: &SyntheticLoadArgs, json_output: bool) -> Result<(), DynError> {
    let (summary, samples_path, summary_path) = run_once(args, json_output, None).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "Synthetic load complete: transport={} format={} down={} up={} samples={} summary={}",
            summary.transport,
            summary.torrent_format,
            format_bps(summary.avg_download_bps),
            format_bps(summary.avg_upload_bps),
            samples_path.display(),
            summary_path.display()
        );
    }

    Ok(())
}

fn benchmark_interrupted(interrupt_rx: &watch::Receiver<bool>) -> bool {
    *interrupt_rx.borrow()
}

fn benchmark_interrupt_requested(interrupt_rx: Option<&watch::Receiver<bool>>) -> bool {
    interrupt_rx.map(benchmark_interrupted).unwrap_or(false)
}

async fn wait_for_benchmark_interrupt(interrupt_rx: &mut watch::Receiver<bool>) -> bool {
    if benchmark_interrupted(interrupt_rx) {
        return true;
    }
    loop {
        if interrupt_rx.changed().await.is_err() {
            return false;
        }
        if *interrupt_rx.borrow_and_update() {
            return true;
        }
    }
}

async fn run_once(
    args: &SyntheticLoadArgs,
    suppress_sample_output: bool,
    interrupt_rx: Option<watch::Receiver<bool>>,
) -> Result<(SyntheticSummary, PathBuf, PathBuf), DynError> {
    let config = ParsedSyntheticConfig::from_args(args)?;
    let _transport_env_guard = SyntheticTransportEnvGuard::new(args.transport);
    let _chaos_env_guard = SharedUdpChaosEnvGuard::new(args.utp_chaos);
    let run_id = Local::now().format("run_%Y%m%d_%H%M%S").to_string();
    let output_dir = args.out.join(&run_id);
    tokio::fs::create_dir_all(&output_dir).await?;
    tokio::fs::create_dir_all(output_dir.join("data")).await?;

    let counters = Arc::new(SyntheticCounters::default());
    let (harness_shutdown_tx, _) = broadcast::channel::<()>(16);
    let (resource_shutdown_tx, _) = broadcast::channel::<()>(1);
    let topology = topology_for(args.mode, args.peers, args.torrents)?;
    let add_plan = AddPlan::from_args(args);
    let peer_plan = AddPlan {
        mode: args.peer_add_mode,
        interval: Duration::from_millis(args.peer_add_interval_ms),
        burst_size: args.peer_add_burst_size,
    };
    let specs: Arc<[SyntheticTorrentSpec]> = build_torrent_specs(
        args.torrents,
        config.size_per_torrent,
        config.piece_size,
        args.torrent_format,
    )?
    .into();
    let (network_handle, _network_supervisor_task) = NetworkSupervisor::spawn_unrestricted()?;
    let network_lease = network_handle.try_lease()?;
    let (client_port, _client_udp_reservation) =
        synthetic_client_port(&network_lease, args.transport).await?;
    let (mut network_activation_publisher, network_activation) =
        crate::networking::NetworkActivationPublisher::channel();
    network_activation_publisher.activate(network_lease.clone(), client_port)?;

    let resource_manager = build_resource_manager(args, topology, resource_shutdown_tx.clone());
    let resource_client = resource_manager.1.clone();
    let resource_handle = tokio::spawn(resource_manager.0.run());

    let (event_tx, event_rx) = mpsc::channel::<ManagerEvent>(EVENT_CHANNEL_SIZE);
    let event_handle = tokio::spawn(collect_manager_events(event_rx, counters.clone()));
    let mut cleanup = SyntheticRunCleanup::new(
        harness_shutdown_tx.clone(),
        resource_shutdown_tx.clone(),
        resource_handle,
        event_handle,
    );

    let rate_limit = synthetic_target_rate_limit(args.target_gbps);
    let global_dl_bucket = Arc::new(TokenBucket::new(rate_limit, rate_limit));
    let global_ul_bucket = Arc::new(TokenBucket::new(rate_limit, rate_limit));
    let harness = HarnessContext {
        network_handle,
        network_activation,
        _network_activation_publisher: Arc::new(network_activation_publisher),
        event_tx,
        resource_client: resource_client.clone(),
        global_dl_bucket,
        global_ul_bucket,
        counters: counters.clone(),
        shutdown_tx: harness_shutdown_tx.clone(),
        client_port,
    };

    let download_dir = output_dir.join("data").join("download");
    let upload_dir = output_dir.join("data").join("upload");
    let download_seeder_hub = if topology.download_peers > 0 {
        let (hub, handle) = match spawn_synthetic_seeder_hub(
            harness.network_handle.clone(),
            specs.clone(),
            counters.clone(),
            harness_shutdown_tx.clone(),
            topology.download_peers,
            args.transport,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return cleanup.fail(error).await,
        };
        cleanup.peer_handles.push(handle);
        Some(hub)
    } else {
        None
    };
    let mut upload_incoming_hubs = Vec::new();
    if topology.upload_peers > 0 {
        let hub_count = topology
            .upload_peers
            .div_ceil(SYNTHETIC_PEERS_PER_INCOMING_HUB)
            .clamp(1, MAX_SYNTHETIC_INCOMING_HUBS);
        for _ in 0..hub_count {
            let (hub, handle) = match spawn_incoming_hub(
                harness.network_handle.clone(),
                counters.clone(),
                harness_shutdown_tx.clone(),
                resource_client.clone(),
                args.transport,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => return cleanup.fail(error).await,
            };
            cleanup.peer_handles.push(handle);
            upload_incoming_hubs.push(hub);
        }
    }
    let mut add_context = AddContext {
        specs: specs.clone(),
        topology,
        download_root: download_dir.clone(),
        upload_root: upload_dir.clone(),
        download_seeder_hub,
        upload_incoming_hubs,
        harness: harness.clone(),
        plan: add_plan,
        peer_plan,
        peer_ramps: Vec::new(),
        leecher_pipeline: args.leecher_pipeline,
        next_torrent: 0,
    };
    if args.add_mode == SyntheticLoadAddMode::Upfront {
        if let Err(error) = add_context
            .add_until(
                args.torrents,
                Duration::ZERO,
                &mut cleanup.managers,
                &mut cleanup.peer_handles,
                usize::MAX,
            )
            .await
        {
            return cleanup.fail(error).await;
        }
        if args.peer_add_mode == SyntheticLoadAddMode::Upfront {
            if let Err(error) = add_context
                .add_due_peers(Duration::ZERO, &mut cleanup.peer_handles, usize::MAX)
                .await
            {
                return cleanup.fail(error).await;
            }
        }
    }
    let mut orchestration_progress = add_context.progress();
    let (orchestration_tx, mut orchestration_rx) = mpsc::unbounded_channel();
    let mut orchestrator_handle = tokio::spawn(run_orchestrator(
        add_context,
        args.duration_secs,
        args.warmup_secs,
        orchestration_tx,
    ));

    let samples_path = output_dir.join("samples.jsonl");
    let sample_file = match File::create(&samples_path) {
        Ok(file) => file,
        Err(error) => return cleanup.fail(error).await,
    };
    let mut sample_writer = BufWriter::new(sample_file);
    let interrupt_snapshot = interrupt_rx.clone();
    let summary_result = sample_loop(
        SampleContext {
            args,
            config: &config,
            topology,
            add_plan,
            peer_plan,
            run_id: &run_id,
            output_dir: &output_dir,
            counters: counters.clone(),
            resource_client: &resource_client,
            managers: &mut cleanup.managers,
            peer_handles: &mut cleanup.peer_handles,
            orchestration_rx: &mut orchestration_rx,
            orchestration_progress: &mut orchestration_progress,
            interrupt_rx,
            json_output: suppress_sample_output,
        },
        &mut sample_writer,
    )
    .await;
    if let Err(error) = sample_writer.flush() {
        cleanup.cleanup().await;
        return Err(error.into());
    }

    let interrupted = match &summary_result {
        Ok(summary) => summary.interrupted,
        Err(_) => false,
    } || interrupt_snapshot
        .as_ref()
        .map(benchmark_interrupted)
        .unwrap_or(false);
    let orchestrator_result = if interrupted {
        orchestrator_handle.abort();
        drain_orchestration_updates(
            &mut orchestration_rx,
            &mut cleanup.managers,
            &mut cleanup.peer_handles,
            &mut orchestration_progress,
        )
        .map(|_| ())
    } else {
        wait_for_orchestrator(
            &mut orchestrator_handle,
            &mut orchestration_rx,
            &mut cleanup.managers,
            &mut cleanup.peer_handles,
            &mut orchestration_progress,
        )
        .await
    };

    cleanup.cleanup().await;

    orchestrator_result?;
    let summary = summary_result?;

    let summary_path = output_dir.join("summary.json");
    tokio::fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?).await?;

    Ok((summary, samples_path, summary_path))
}

pub async fn run_benchmark(
    args: &SyntheticBenchmarkArgs,
    json_output: bool,
) -> Result<(), DynError> {
    let config = ParsedBenchmarkConfig::from_args(args)?;
    let benchmark_started = Instant::now();
    let run_id = Local::now().format("benchmark_%Y%m%d_%H%M%S").to_string();
    let output_dir = args.out.join(&run_id);
    tokio::fs::create_dir_all(&output_dir).await?;
    let (interrupt_tx, interrupt_rx) = watch::channel(false);
    let interrupt_handle = tokio::spawn(async move {
        if signal::ctrl_c().await.is_ok() {
            let _ = interrupt_tx.send(true);
        }
    });

    let modes = [
        SyntheticLoadMode::Download,
        SyntheticLoadMode::Upload,
        SyntheticLoadMode::Swarm,
    ];
    let total_planned_steps = benchmark_total_planned_steps(args, &config, &modes);
    let mut progress = BenchmarkRunProgress::new(total_planned_steps);
    let mut profiles = Vec::new();
    for mode in modes {
        if benchmark_interrupted(&interrupt_rx) {
            break;
        }
        match run_benchmark_profile(
            args,
            &config,
            mode,
            &output_dir,
            json_output,
            &mut progress,
            &interrupt_rx,
        )
        .await
        {
            Ok(profile) => profiles.push(profile),
            Err(error) => {
                profiles.push(benchmark_failed_profile_summary(
                    args,
                    &config,
                    mode,
                    error.to_string(),
                    &mut progress,
                ));
            }
        }
        if benchmark_interrupted(&interrupt_rx) {
            break;
        }
    }
    let interrupted = benchmark_interrupted(&interrupt_rx);
    let runtime_secs = benchmark_started.elapsed().as_secs_f64();
    let report = benchmark_report(
        args,
        &config,
        &profiles,
        total_planned_steps,
        runtime_secs,
        interrupted,
    );

    let summary = BenchmarkSummary {
        run_id,
        transport: args.transport.as_str().to_string(),
        torrent_format: args.torrent_format.as_str().to_string(),
        utp_chaos: shared_udp_chaos_env_value(args.utp_chaos),
        interrupted,
        disk_budget_bytes: config.disk_budget,
        preferred_size_per_torrent_bytes: config.preferred_size_per_torrent,
        piece_size_bytes: config.piece_size,
        max_torrents: args.max_torrents,
        max_peers: args.max_peers,
        planned_steps: total_planned_steps,
        keep_output: args.keep_output,
        report,
        profiles,
        output_dir: output_dir.clone(),
    };

    let summary_path = output_dir.join("benchmark_summary.json");
    let summary_write_error = tokio::fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)
        .await
        .err();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_benchmark_report(&summary, &summary_path);
    }
    if let Some(error) = summary_write_error {
        eprintln!(
            "[Warn] Failed to write benchmark JSON at {}: {}",
            summary_path.display(),
            error
        );
    }

    interrupt_handle.abort();
    Ok(())
}

async fn run_benchmark_profile(
    args: &SyntheticBenchmarkArgs,
    config: &ParsedBenchmarkConfig,
    mode: SyntheticLoadMode,
    output_dir: &Path,
    json_output: bool,
    progress: &mut BenchmarkRunProgress,
    interrupt_rx: &watch::Receiver<bool>,
) -> Result<BenchmarkProfileSummary, DynError> {
    let plans = benchmark_step_plans(args, config, mode)?;
    let final_plan = plans
        .last()
        .cloned()
        .ok_or_else(|| "benchmark generated no steps".to_string())?;
    let mut steps = Vec::new();
    let mut last_clean = None;
    let mut first_issue = None;

    if !json_output {
        println!(
            "Benchmark {}: planned_steps={} final={} torrents / {} peers final_size_per_torrent={} estimated_disk={}/{} budget={}",
            mode_name(mode),
            final_plan.planned_steps,
            format_count(final_plan.torrents),
            format_count(final_plan.peers),
            format_bytes(final_plan.size_per_torrent_bytes),
            format_bytes(final_plan.estimated_disk_bytes),
            format_bytes(config.disk_budget),
            format_bytes(config.disk_budget)
        );
    }

    'plans: for plan in plans {
        if benchmark_interrupted(interrupt_rx) {
            break;
        }
        let max_attempts = args.issue_retries.saturating_add(1).max(1);
        for attempt in 1..=max_attempts {
            if benchmark_interrupted(interrupt_rx) {
                break 'plans;
            }
            let step_out = output_dir.join(mode_name(mode)).join(format!(
                "step_{:02}_{}t_{}p_attempt_{:02}",
                plan.step, plan.torrents, plan.peers, attempt
            ));
            let synthetic_args = benchmark_synthetic_args(
                args,
                mode,
                plan.torrents,
                plan.peers,
                plan.size_per_torrent_bytes,
                step_out,
            );

            if !json_output {
                println!(
                    "Benchmark {} step {}/{} attempt {}/{}: torrents={} peers={} size_per_torrent={} estimated_disk={}/{} budget={}",
                    mode_name(mode),
                    plan.step,
                    plan.planned_steps,
                    attempt,
                    max_attempts,
                    plan.torrents,
                    plan.peers,
                    format_bytes(plan.size_per_torrent_bytes),
                    format_bytes(plan.estimated_disk_bytes),
                    format_bytes(final_plan.estimated_disk_bytes),
                    format_bytes(config.disk_budget)
                );
            }

            let step_started = Instant::now();
            let (summary, samples_path, summary_path) =
                match run_once(&synthetic_args, true, Some(interrupt_rx.clone())).await {
                    Ok(result) => result,
                    Err(error) => {
                        let will_retry =
                            !benchmark_interrupted(interrupt_rx) && attempt < max_attempts;
                        let timing = progress.record_step(
                            step_started.elapsed().as_secs_f64(),
                            if will_retry {
                                0
                            } else {
                                remaining_steps_after_issue(&plan)
                            },
                            if will_retry {
                                remaining_steps_in_current_scenario(&plan).saturating_add(1)
                            } else {
                                0
                            },
                            usize::from(will_retry),
                        );
                        let attempt_context = BenchmarkAttemptContext {
                            attempt,
                            max_attempts,
                            will_retry,
                            retry_delay_ms: args.retry_delay_ms,
                            timing,
                        };
                        let step = benchmark_failed_step_summary(
                            mode,
                            &plan,
                            attempt_context,
                            error.to_string(),
                        );
                        if !json_output {
                            print_benchmark_step_result(&step);
                        }
                        if will_retry {
                            steps.push(step);
                            let mut retry_interrupt_rx = interrupt_rx.clone();
                            if sleep_before_benchmark_retry(
                                args.retry_delay_ms,
                                &mut retry_interrupt_rx,
                            )
                            .await
                            {
                                break 'plans;
                            }
                            continue;
                        }
                        first_issue = Some(step.clone());
                        steps.push(step);
                        break 'plans;
                    }
                };
            let data_removed = if args.keep_output {
                false
            } else {
                remove_run_data_dir(&summary.output_dir).await?
            };
            let issues = benchmark_issues(&summary, args);
            let has_issue = !issues.is_empty();
            let will_retry = has_issue && !summary.interrupted && attempt < max_attempts;
            let timing = progress.record_step(
                step_started.elapsed().as_secs_f64(),
                if has_issue && !will_retry {
                    remaining_steps_after_issue(&plan)
                } else {
                    0
                },
                if will_retry {
                    remaining_steps_in_current_scenario(&plan).saturating_add(1)
                } else if has_issue {
                    0
                } else {
                    remaining_steps_in_current_scenario(&plan)
                },
                usize::from(will_retry),
            );
            let step = benchmark_step_summary(
                &summary,
                &plan,
                BenchmarkAttemptContext {
                    attempt,
                    max_attempts,
                    will_retry,
                    retry_delay_ms: args.retry_delay_ms,
                    timing,
                },
                samples_path,
                summary_path,
                issues,
                data_removed,
            );

            if !json_output {
                print_benchmark_step_result(&step);
            }

            if step.issues.is_empty() {
                last_clean = Some(step.clone());
                steps.push(step);
                break;
            }

            if will_retry {
                steps.push(step);
                let mut retry_interrupt_rx = interrupt_rx.clone();
                if sleep_before_benchmark_retry(args.retry_delay_ms, &mut retry_interrupt_rx).await
                {
                    break 'plans;
                }
                continue;
            }

            first_issue = Some(step.clone());
            steps.push(step);
            break 'plans;
        }
    }
    let metrics = benchmark_profile_metrics(&steps);

    Ok(BenchmarkProfileSummary {
        mode: mode_name(mode).to_string(),
        planned_steps: final_plan.planned_steps,
        final_torrents: final_plan.torrents,
        final_peers: final_plan.peers,
        final_size_per_torrent_bytes: final_plan.size_per_torrent_bytes,
        final_estimated_disk_bytes: final_plan.estimated_disk_bytes,
        metrics,
        last_clean,
        first_issue,
        steps,
    })
}

struct ParsedSyntheticConfig {
    size_per_torrent: u64,
    piece_size: u64,
}

struct ParsedBenchmarkConfig {
    disk_budget: u64,
    preferred_size_per_torrent: u64,
    piece_size: u64,
}

impl ParsedBenchmarkConfig {
    fn from_args(args: &SyntheticBenchmarkArgs) -> Result<Self, DynError> {
        validate_udp_chaos_args(args.utp_chaos)?;
        if args.start_torrents == 0 || args.max_torrents == 0 {
            return Err("benchmark requires torrent counts greater than 0".into());
        }
        if args.start_peers == 0 || args.max_peers == 0 {
            return Err("benchmark requires peer counts greater than 0".into());
        }
        if args.max_steps == 0 {
            return Err("benchmark requires --max-steps greater than 0".into());
        }
        if args.duration_secs == 0 {
            return Err("benchmark requires --duration-secs greater than 0".into());
        }
        if args.metrics_interval_ms == 0 {
            return Err("benchmark requires --metrics-interval-ms greater than 0".into());
        }
        if args.leecher_pipeline == 0 {
            return Err("benchmark requires --leecher-pipeline greater than 0".into());
        }
        if args.peer_add_interval_ms == 0 {
            return Err("benchmark requires --peer-add-interval-ms greater than 0".into());
        }
        if args.peer_add_burst_size == 0 {
            return Err("benchmark requires --peer-add-burst-size greater than 0".into());
        }
        if args.target_gbps <= 0.0 || !args.target_gbps.is_finite() {
            return Err("benchmark requires --target-gbps to be finite and greater than 0".into());
        }

        let disk_budget = parse_size(&args.disk_budget)?;
        let preferred_size_per_torrent = parse_size(&args.size_per_torrent)?;
        let piece_size = parse_size(&args.piece_size)?;
        if piece_size == 0 || piece_size > u32::MAX as u64 {
            return Err("--piece-size must be between 1 byte and u32::MAX".into());
        }
        if preferred_size_per_torrent < piece_size {
            return Err("--size-per-torrent must be at least --piece-size".into());
        }
        let min_download_budget = estimated_disk_bytes(
            SyntheticLoadMode::Download,
            args.start_torrents.min(args.max_torrents),
            piece_size,
        );
        let min_swarm_budget = estimated_disk_bytes(
            SyntheticLoadMode::Swarm,
            args.start_torrents.min(args.max_torrents),
            piece_size,
        );
        if disk_budget < min_download_budget {
            return Err(format!(
                "--disk-budget {} is too small for the first download/upload step; need at least {}",
                format_bytes(disk_budget),
                format_bytes(min_download_budget)
            )
            .into());
        }
        if disk_budget < min_swarm_budget {
            return Err(format!(
                "--disk-budget {} is too small for the first swarm step; need at least {}",
                format_bytes(disk_budget),
                format_bytes(min_swarm_budget)
            )
            .into());
        }

        Ok(Self {
            disk_budget,
            preferred_size_per_torrent,
            piece_size,
        })
    }
}

impl ParsedSyntheticConfig {
    fn from_args(args: &SyntheticLoadArgs) -> Result<Self, DynError> {
        validate_udp_chaos_args(args.utp_chaos)?;
        if args.torrents == 0 {
            return Err("synthetic-load requires --torrents greater than 0".into());
        }
        if args.peers == 0 {
            return Err("synthetic-load requires --peers greater than 0".into());
        }
        if args.duration_secs == 0 {
            return Err("synthetic-load requires --duration-secs greater than 0".into());
        }
        if args.metrics_interval_ms == 0 {
            return Err("synthetic-load requires --metrics-interval-ms greater than 0".into());
        }
        if args.leecher_pipeline == 0 {
            return Err("synthetic-load requires --leecher-pipeline greater than 0".into());
        }
        if args.add_interval_ms == 0 {
            return Err("synthetic-load requires --add-interval-ms greater than 0".into());
        }
        if args.add_burst_size == 0 {
            return Err("synthetic-load requires --add-burst-size greater than 0".into());
        }
        if args.peer_add_interval_ms == 0 {
            return Err("synthetic-load requires --peer-add-interval-ms greater than 0".into());
        }
        if args.peer_add_burst_size == 0 {
            return Err("synthetic-load requires --peer-add-burst-size greater than 0".into());
        }

        let size_per_torrent = parse_size(&args.size_per_torrent)?;
        let piece_size = parse_size(&args.piece_size)?;
        if size_per_torrent == 0 {
            return Err("--size-per-torrent must be greater than 0".into());
        }
        if piece_size == 0 || piece_size > u32::MAX as u64 {
            return Err("--piece-size must be between 1 byte and u32::MAX".into());
        }
        if piece_size > size_per_torrent {
            return Err("--piece-size must not exceed --size-per-torrent".into());
        }

        Ok(Self {
            size_per_torrent,
            piece_size,
        })
    }
}

fn validate_udp_chaos_args(chaos: SyntheticUdpChaosArgs) -> Result<(), DynError> {
    const MAX_PPM: u32 = 1_000_000;
    let values = [
        ("--utp-chaos-loss-ppm", chaos.utp_chaos_loss_ppm),
        ("--utp-chaos-duplicate-ppm", chaos.utp_chaos_duplicate_ppm),
        ("--utp-chaos-corrupt-ppm", chaos.utp_chaos_corrupt_ppm),
        ("--utp-chaos-reorder-ppm", chaos.utp_chaos_reorder_ppm),
    ];
    for (name, value) in values {
        if value > MAX_PPM {
            return Err(format!("{name} must be between 0 and {MAX_PPM}").into());
        }
    }
    Ok(())
}

struct SideSetup {
    managers: Vec<ManagerRuntime>,
    peer_handles: Vec<JoinHandle<()>>,
    peer_ramps: Vec<PeerRamp>,
}

struct UploadStartContext<'a> {
    data_root: &'a Path,
    incoming_hub: &'a SyntheticIncomingHub,
    harness: &'a HarnessContext,
    leecher_pipeline: usize,
    added_at: Duration,
}

async fn start_download_torrent(
    spec: &SyntheticTorrentSpec,
    total_torrents: usize,
    peers: usize,
    data_root: &Path,
    seeder_hub: &SyntheticSeederHub,
    harness: &HarnessContext,
    added_at: Duration,
) -> Result<SideSetup, DynError> {
    tokio::fs::create_dir_all(data_root).await?;

    let manager = build_manager(
        spec,
        data_root.join(format!("torrent_{:04}", spec.index)),
        false,
        harness,
    )?;
    let (manager, command_tx, metrics_rx) = manager;
    let handle = tokio::spawn(async move { manager.run(false).await });
    let peer_indices = peer_indices_for_torrent(peers, total_torrents, spec.index).collect();
    let peer_ramp = PeerRamp {
        spec: spec.clone(),
        peer_indices,
        next_peer: 0,
        added_at,
        role: PeerRampRole::DownloadSeeder {
            command_tx: command_tx.clone(),
            seeder_hub: seeder_hub.clone(),
        },
    };

    Ok(SideSetup {
        managers: vec![ManagerRuntime {
            command_tx,
            metrics_rx,
            handle,
        }],
        peer_handles: Vec::new(),
        peer_ramps: vec![peer_ramp],
    })
}

async fn start_upload_torrent(
    spec: &SyntheticTorrentSpec,
    total_torrents: usize,
    peers: usize,
    context: UploadStartContext<'_>,
) -> Result<SideSetup, DynError> {
    tokio::fs::create_dir_all(context.data_root).await?;

    let torrent_dir = context.data_root.join(format!("torrent_{:04}", spec.index));
    prepare_seed_file(spec, &torrent_dir).await?;
    let (incoming_tx, incoming_rx) = mpsc::channel(MANAGER_CHANNEL_SIZE);
    context
        .incoming_hub
        .register(spec.info_hash.clone(), incoming_tx);

    let manager =
        build_manager_with_incoming(spec, torrent_dir, true, incoming_rx, context.harness)?;
    let (manager, command_tx, metrics_rx) = manager;
    let handle = tokio::spawn(async move { manager.run(false).await });

    let peer_indices = peer_indices_for_torrent(peers, total_torrents, spec.index).collect();
    let peer_ramp = PeerRamp {
        spec: spec.clone(),
        peer_indices,
        next_peer: 0,
        added_at: context.added_at,
        role: PeerRampRole::UploadLeecher {
            incoming_hub: context.incoming_hub.clone(),
            leecher_pipeline: context.leecher_pipeline,
        },
    };

    Ok(SideSetup {
        managers: vec![ManagerRuntime {
            command_tx,
            metrics_rx,
            handle,
        }],
        peer_handles: Vec::new(),
        peer_ramps: vec![peer_ramp],
    })
}

fn build_manager(
    spec: &SyntheticTorrentSpec,
    torrent_data_path: PathBuf,
    validated: bool,
    harness: &HarnessContext,
) -> Result<
    (
        TorrentManager,
        mpsc::Sender<ManagerCommand>,
        watch::Receiver<TorrentMetrics>,
    ),
    DynError,
> {
    let (_incoming_tx, incoming_rx) = mpsc::channel(MANAGER_CHANNEL_SIZE);
    build_manager_with_rx(spec, torrent_data_path, validated, incoming_rx, harness)
}

fn build_manager_with_incoming(
    spec: &SyntheticTorrentSpec,
    torrent_data_path: PathBuf,
    validated: bool,
    incoming_rx: mpsc::Receiver<IncomingPeerSession>,
    harness: &HarnessContext,
) -> Result<
    (
        TorrentManager,
        mpsc::Sender<ManagerCommand>,
        watch::Receiver<TorrentMetrics>,
    ),
    DynError,
> {
    build_manager_with_rx(spec, torrent_data_path, validated, incoming_rx, harness)
}

fn build_manager_with_rx(
    spec: &SyntheticTorrentSpec,
    torrent_data_path: PathBuf,
    validated: bool,
    incoming_rx: mpsc::Receiver<IncomingPeerSession>,
    harness: &HarnessContext,
) -> Result<
    (
        TorrentManager,
        mpsc::Sender<ManagerCommand>,
        watch::Receiver<TorrentMetrics>,
    ),
    DynError,
> {
    let (command_tx, command_rx) = mpsc::channel(MANAGER_CHANNEL_SIZE);
    let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
    let (_peer_policy_tx, peer_policy_rx) =
        watch::channel(Arc::new(crate::peer_manager::PeerPolicy::default()));
    let settings = Arc::new(Settings {
        client_id: CLIENT_ID.to_string(),
        client_port: harness.client_port,
        private_client: false,
        ..Default::default()
    });
    let params = TorrentParameters {
        network_activation: harness.network_activation.clone(),
        dht_handle: crate::dht::service::DhtHandle::disabled(),
        incoming_peer_rx: incoming_rx,
        metrics_tx,
        peer_policy_rx,
        torrent_validation_status: validated,
        torrent_data_path: Some(torrent_data_path),
        container_name: None,
        manager_command_rx: command_rx,
        manager_event_tx: harness.event_tx.clone(),
        settings,
        resource_manager: harness.resource_client.clone(),
        global_dl_bucket: harness.global_dl_bucket.clone(),
        global_ul_bucket: harness.global_ul_bucket.clone(),
        file_priorities: HashMap::new(),
    };

    let manager = TorrentManager::from_torrent(
        params.with_payload(crate::persistence::Payload::native()),
        spec.torrent.clone(),
    )
    .map_err(|message| format!("failed to build synthetic manager: {message}"))?;
    Ok((manager, command_tx, metrics_rx))
}

async fn bind_synthetic_tcp_listener() -> Result<(TcpListener, u16), DynError> {
    let listener = TcpListener::bind(synthetic_listener_bind_addr()).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

async fn synthetic_client_port(
    network_lease: &NetworkLease,
    transport: SyntheticTransport,
) -> Result<(u16, Option<SharedUdpHandle>), DynError> {
    if matches!(transport, SyntheticTransport::Tcp) {
        return Ok((Settings::default().client_port, None));
    }

    let udp = SharedUdpHandle::bind(
        network_lease,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SharedUdpFamily::Ipv4,
    )
    .await?;
    let port = udp.local_addr()?.port();
    Ok((port, Some(udp)))
}

async fn bind_synthetic_utp_listener(
    network_lease: &NetworkLease,
    port: u16,
) -> Result<(UtpListenerSet, u16), DynError> {
    let listener = UtpPeerTransport::bind_listener(network_lease, port).await?;
    let port = listener
        .local_port()
        .ok_or("synthetic uTP listener did not expose a local port")?;
    Ok((listener, port))
}

fn join_synthetic_handles(handles: Vec<JoinHandle<()>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        for handle in handles {
            let _ = handle.await;
        }
    })
}

async fn abort_synthetic_handles(handles: &mut Vec<JoinHandle<()>>) {
    for mut handle in handles.drain(..) {
        handle.abort();
        let _ = (&mut handle).await;
    }
}

async fn spawn_synthetic_seeder_hub(
    network_handle: NetworkHandle,
    specs: Arc<[SyntheticTorrentSpec]>,
    counters: Arc<SyntheticCounters>,
    shutdown_tx: broadcast::Sender<()>,
    peer_slots: usize,
    transport: SyntheticTransport,
) -> Result<(SyntheticSeederHub, JoinHandle<()>), DynError> {
    let network_lease = network_handle.try_lease()?;
    let specs_by_hash: Arc<HashMap<Vec<u8>, SyntheticTorrentSpec>> = Arc::new(
        specs
            .iter()
            .cloned()
            .map(|spec| (spec.info_hash.clone(), spec))
            .collect(),
    );
    let next_peer_id = Arc::new(AtomicU64::new(0));

    #[cfg(target_os = "macos")]
    {
        // macOS does not route unconfigured 127/8 aliases, so give each
        // synthetic seeder a unique localhost listener port instead.
        let listener_count = peer_slots.max(1);
        match transport {
            SyntheticTransport::Tcp => {
                let mut ports = Vec::with_capacity(listener_count);
                let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(listener_count);
                for _ in 0..listener_count {
                    let (listener, port) = match bind_synthetic_tcp_listener().await {
                        Ok(result) => result,
                        Err(error) => {
                            abort_synthetic_handles(&mut handles).await;
                            return Err(error);
                        }
                    };
                    ports.push(port);
                    handles.push(spawn_synthetic_seeder_accept_loop(
                        listener,
                        specs_by_hash.clone(),
                        counters.clone(),
                        shutdown_tx.clone(),
                        next_peer_id.clone(),
                    ));
                }
                Ok((
                    SyntheticSeederHub::PeerPorts {
                        ports: Arc::<[u16]>::from(ports),
                    },
                    join_synthetic_handles(handles),
                ))
            }
            SyntheticTransport::Utp => {
                let (listener, port) = bind_synthetic_utp_listener(&network_lease, 0).await?;
                Ok((
                    SyntheticSeederHub::SharedUtp { port },
                    spawn_synthetic_utp_seeder_accept_loop(
                        listener,
                        specs_by_hash,
                        counters,
                        shutdown_tx,
                        next_peer_id,
                    ),
                ))
            }
            SyntheticTransport::All => {
                let mut tcp_ports = Vec::with_capacity(listener_count);
                let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(listener_count + 1);
                for _ in 0..listener_count {
                    let (tcp_listener, tcp_port) = match bind_synthetic_tcp_listener().await {
                        Ok(result) => result,
                        Err(error) => {
                            abort_synthetic_handles(&mut handles).await;
                            return Err(error);
                        }
                    };
                    tcp_ports.push(tcp_port);
                    handles.push(spawn_synthetic_seeder_accept_loop(
                        tcp_listener,
                        specs_by_hash.clone(),
                        counters.clone(),
                        shutdown_tx.clone(),
                        next_peer_id.clone(),
                    ));
                }
                let (utp_listener, utp_port) =
                    match bind_synthetic_utp_listener(&network_lease, 0).await {
                        Ok(result) => result,
                        Err(error) => {
                            abort_synthetic_handles(&mut handles).await;
                            return Err(error);
                        }
                    };
                handles.push(spawn_synthetic_utp_seeder_accept_loop(
                    utp_listener,
                    specs_by_hash,
                    counters,
                    shutdown_tx,
                    next_peer_id,
                ));
                Ok((
                    SyntheticSeederHub::MixedPeerTcpSharedUtp {
                        tcp_ports: Arc::<[u16]>::from(tcp_ports),
                        utp_port,
                    },
                    join_synthetic_handles(handles),
                ))
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        match transport {
            SyntheticTransport::Tcp => {
                let _ = peer_slots;
                let (listener, port) = bind_synthetic_tcp_listener().await?;
                let handle = spawn_synthetic_seeder_accept_loop(
                    listener,
                    specs_by_hash,
                    counters,
                    shutdown_tx,
                    next_peer_id,
                );
                Ok((SyntheticSeederHub::SinglePort { port }, handle))
            }
            SyntheticTransport::Utp => {
                let _ = peer_slots;
                let (listener, port) = bind_synthetic_utp_listener(&network_lease, 0).await?;
                Ok((
                    SyntheticSeederHub::SharedUtp { port },
                    spawn_synthetic_utp_seeder_accept_loop(
                        listener,
                        specs_by_hash,
                        counters,
                        shutdown_tx,
                        next_peer_id,
                    ),
                ))
            }
            SyntheticTransport::All => {
                let (tcp_listener, tcp_port) = bind_synthetic_tcp_listener().await?;
                let _ = peer_slots;
                let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(2);
                handles.push(spawn_synthetic_seeder_accept_loop(
                    tcp_listener,
                    specs_by_hash.clone(),
                    counters.clone(),
                    shutdown_tx.clone(),
                    next_peer_id.clone(),
                ));
                let (utp_listener, utp_port) =
                    match bind_synthetic_utp_listener(&network_lease, 0).await {
                        Ok(result) => result,
                        Err(error) => {
                            abort_synthetic_handles(&mut handles).await;
                            return Err(error);
                        }
                    };
                handles.push(spawn_synthetic_utp_seeder_accept_loop(
                    utp_listener,
                    specs_by_hash,
                    counters,
                    shutdown_tx,
                    next_peer_id,
                ));
                Ok((
                    SyntheticSeederHub::MixedSingleTcpSharedUtp { tcp_port, utp_port },
                    join_synthetic_handles(handles),
                ))
            }
        }
    }
}

fn spawn_synthetic_seeder_accept_loop(
    listener: TcpListener,
    specs_by_hash: Arc<HashMap<Vec<u8>, SyntheticTorrentSpec>>,
    counters: Arc<SyntheticCounters>,
    shutdown_tx: broadcast::Sender<()>,
    next_peer_id: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut shutdown_rx = shutdown_tx.subscribe();
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            spawn_synthetic_seeder_peer(
                                stream,
                                specs_by_hash.clone(),
                                counters.clone(),
                                shutdown_tx.clone(),
                                next_peer_id.clone(),
                            );
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    })
}

fn spawn_synthetic_utp_seeder_accept_loop(
    listener: UtpListenerSet,
    specs_by_hash: Arc<HashMap<Vec<u8>, SyntheticTorrentSpec>>,
    counters: Arc<SyntheticCounters>,
    shutdown_tx: broadcast::Sender<()>,
    next_peer_id: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut shutdown_rx = shutdown_tx.subscribe();
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok(connection) => {
                            spawn_synthetic_seeder_peer(
                                connection.stream,
                                specs_by_hash.clone(),
                                counters.clone(),
                                shutdown_tx.clone(),
                                next_peer_id.clone(),
                            );
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    })
}

fn spawn_synthetic_seeder_peer<S>(
    mut stream: S,
    specs_by_hash: Arc<HashMap<Vec<u8>, SyntheticTorrentSpec>>,
    counters: Arc<SyntheticCounters>,
    shutdown_tx: broadcast::Sender<()>,
    next_peer_id: Arc<AtomicU64>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    counters.connections.fetch_add(1, Ordering::Relaxed);
    let peer_id = synthetic_peer_id(b'S', next_peer_id.fetch_add(1, Ordering::Relaxed) as usize);
    let counters = counters.clone();
    let mut child_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut handshake = vec![0u8; 68];
        let result: Result<(), DynError> = async {
            stream.read_exact(&mut handshake).await?;
            let info_hash = handshake
                .get(28..48)
                .ok_or("synthetic seeder received short handshake")?;
            let spec = specs_by_hash
                .get(info_hash)
                .ok_or("synthetic seeder received unknown info hash")?;
            run_seeder_connection(
                stream,
                handshake,
                spec,
                peer_id,
                counters.clone(),
                &mut child_shutdown,
            )
            .await
        }
        .await;

        if let Err(error) = result {
            if !is_expected_connection_close(error.as_ref()) {
                counters
                    .synthetic_seeder_errors
                    .fetch_add(1, Ordering::Relaxed);
                counters.protocol_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        counters.disconnects.fetch_add(1, Ordering::Relaxed);
    });
}

#[cfg(not(target_os = "macos"))]
fn synthetic_loopback_addr(peer_index: usize, port: u16) -> SocketAddr {
    let host = (peer_index as u32 % 0x00ff_ffff).saturating_add(1);
    SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(
            127,
            ((host >> 16) & 0xff) as u8,
            ((host >> 8) & 0xff) as u8,
            (host & 0xff) as u8,
        )),
        port,
    )
}

fn synthetic_single_listener_addr(peer_index: usize, port: u16) -> SocketAddr {
    #[cfg(target_os = "macos")]
    {
        let _ = peer_index;
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[cfg(not(target_os = "macos"))]
    {
        synthetic_loopback_addr(peer_index, port)
    }
}

fn synthetic_listener_bind_addr() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "127.0.0.1:0"
    }

    #[cfg(not(target_os = "macos"))]
    {
        "0.0.0.0:0"
    }
}

#[cfg(not(target_os = "macos"))]
fn synthetic_local_addr(peer_index: usize) -> SocketAddr {
    let host = (peer_index / SYNTHETIC_LOCAL_PORT_SPAN) as u32 + 1;
    let port = SYNTHETIC_LOCAL_PORT_BASE
        + (peer_index % SYNTHETIC_LOCAL_PORT_SPAN)
            .try_into()
            .unwrap_or(0);
    SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(
            127,
            ((host >> 16) & 0xff) as u8,
            ((host >> 8) & 0xff) as u8,
            (host & 0xff) as u8,
        )),
        port,
    )
}

fn bind_synthetic_leecher_socket(peer_index: usize) -> Result<TcpSocket, std::io::Error> {
    #[cfg(target_os = "macos")]
    {
        let _ = peer_index;
        let socket = TcpSocket::new_v4()?;
        socket.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
        Ok(socket)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let mut last_error = None;
        for attempt in 0..4 {
            let socket = TcpSocket::new_v4()?;
            let local_addr = synthetic_local_addr(peer_index + attempt * SYNTHETIC_LOCAL_PORT_SPAN);
            match socket.bind(local_addr) {
                Ok(()) => return Ok(socket),
                Err(error) if error.kind() == ErrorKind::AddrInUse => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(
                ErrorKind::AddrInUse,
                "synthetic leecher local ports exhausted",
            )
        }))
    }
}

fn is_expected_connection_close(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    let Some(error) = error.downcast_ref::<std::io::Error>() else {
        return false;
    };
    matches!(
        error.kind(),
        ErrorKind::BrokenPipe
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::UnexpectedEof
    )
}

fn record_synthetic_leecher_error(
    counters: &SyntheticCounters,
    error: &(dyn Error + Send + Sync + 'static),
) {
    counters
        .synthetic_leecher_errors
        .fetch_add(1, Ordering::Relaxed);

    let Some(error) = error.downcast_ref::<std::io::Error>() else {
        counters
            .synthetic_leecher_non_io
            .fetch_add(1, Ordering::Relaxed);
        return;
    };

    match error.kind() {
        ErrorKind::AddrInUse => {
            counters
                .synthetic_leecher_addr_in_use
                .fetch_add(1, Ordering::Relaxed);
        }
        ErrorKind::AddrNotAvailable => {
            counters
                .synthetic_leecher_addr_not_available
                .fetch_add(1, Ordering::Relaxed);
        }
        ErrorKind::ConnectionRefused => {
            counters
                .synthetic_leecher_connection_refused
                .fetch_add(1, Ordering::Relaxed);
        }
        ErrorKind::TimedOut => {
            counters
                .synthetic_leecher_timed_out
                .fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            counters
                .synthetic_leecher_other_io
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn run_seeder_connection<S>(
    stream: S,
    handshake: Vec<u8>,
    spec: &SyntheticTorrentSpec,
    peer_id: Vec<u8>,
    counters: Arc<SyntheticCounters>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> Result<(), DynError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    if handshake.get(28..48) != Some(spec.info_hash.as_slice()) {
        return Err("synthetic seeder received mismatched info hash".into());
    }

    writer
        .write_all(&generate_message(Message::Handshake(
            spec.info_hash.clone(),
            peer_id,
        ))?)
        .await?;
    writer
        .write_all(&generate_message(Message::Bitfield(full_bitfield(
            spec.piece_count,
        )))?)
        .await?;
    writer
        .write_all(&generate_message(Message::Unchoke)?)
        .await?;

    let mut socket_buf = vec![0u8; 64 * 1024];
    let mut parse_buf = Vec::with_capacity(128 * 1024);
    let mut data_block = vec![SYNTHETIC_BYTE; BLOCK_SIZE as usize];

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            read = reader.read(&mut socket_buf) => {
                let n = read?;
                if n == 0 {
                    break;
                }
                parse_buf.extend_from_slice(&socket_buf[..n]);
                while let Some(frame) = take_frame(&mut parse_buf) {
                    match frame_message_id(&frame) {
                        Some(2) => {
                            writer.write_all(&generate_message(Message::Unchoke)?).await?;
                        }
                        Some(6) => {
                            if let Some((index, begin, length)) = parse_request_payload(&frame) {
                                let len = length as usize;
                                if data_block.len() < len {
                                    data_block.resize(len, SYNTHETIC_BYTE);
                                }
                                write_piece_frame(&mut writer, index, begin, &data_block[..len]).await?;
                                counters.seeder_requests.fetch_add(1, Ordering::Relaxed);
                                counters.download_bytes.fetch_add(length as u64, Ordering::Relaxed);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}

async fn spawn_incoming_hub(
    network_handle: NetworkHandle,
    counters: Arc<SyntheticCounters>,
    shutdown_tx: broadcast::Sender<()>,
    resource_client: ResourceManagerClient,
    transport: SyntheticTransport,
) -> Result<(SyntheticIncomingHub, JoinHandle<()>), DynError> {
    let network_lease = network_handle.try_lease()?;
    let routes: IncomingRoutes = Arc::new(Mutex::new(HashMap::new()));
    let (port, handle) = match transport {
        SyntheticTransport::Tcp => {
            let (listener, port) = bind_synthetic_tcp_listener().await?;
            let routes = routes.clone();
            let handle = tokio::spawn(async move {
                let mut shutdown_rx = shutdown_tx.subscribe();
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        accepted = listener.accept() => {
                            let Ok((stream, remote_addr)) = accepted else {
                                break;
                            };
                            let connection = TcpPeerTransport::incoming(stream, remote_addr);
                            spawn_incoming_hub_connection(
                                connection,
                                routes.clone(),
                                resource_client.clone(),
                                counters.clone(),
                            );
                        }
                    }
                }
            });
            (port, handle)
        }
        SyntheticTransport::Utp => {
            let (listener, port) = bind_synthetic_utp_listener(&network_lease, 0).await?;
            let routes = routes.clone();
            let handle = tokio::spawn(async move {
                let mut shutdown_rx = shutdown_tx.subscribe();
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        accepted = listener.accept() => {
                            let Ok(connection) = accepted else {
                                break;
                            };
                            spawn_incoming_hub_connection(
                                connection,
                                routes.clone(),
                                resource_client.clone(),
                                counters.clone(),
                            );
                        }
                    }
                }
            });
            (port, handle)
        }
        SyntheticTransport::All => {
            let (tcp_listener, port) = bind_synthetic_tcp_listener().await?;
            let (utp_listener, _) = bind_synthetic_utp_listener(&network_lease, port).await?;
            let tcp_routes = routes.clone();
            let tcp_counters = counters.clone();
            let tcp_shutdown = shutdown_tx.clone();
            let tcp_resource_client = resource_client.clone();
            let tcp_handle = tokio::spawn(async move {
                let mut shutdown_rx = tcp_shutdown.subscribe();
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        accepted = tcp_listener.accept() => {
                            let Ok((stream, remote_addr)) = accepted else {
                                break;
                            };
                            let connection = TcpPeerTransport::incoming(stream, remote_addr);
                            spawn_incoming_hub_connection(
                                connection,
                                tcp_routes.clone(),
                                tcp_resource_client.clone(),
                                tcp_counters.clone(),
                            );
                        }
                    }
                }
            });
            let utp_routes = routes.clone();
            let utp_resource_client = resource_client.clone();
            let utp_handle = tokio::spawn(async move {
                let mut shutdown_rx = shutdown_tx.subscribe();
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        accepted = utp_listener.accept() => {
                            let Ok(connection) = accepted else {
                                break;
                            };
                            spawn_incoming_hub_connection(
                                connection,
                                utp_routes.clone(),
                                utp_resource_client.clone(),
                                counters.clone(),
                            );
                        }
                    }
                }
            });
            (port, join_synthetic_handles(vec![tcp_handle, utp_handle]))
        }
    };
    let hub = SyntheticIncomingHub {
        port,
        transport,
        routes: routes.clone(),
    };
    Ok((hub, handle))
}

fn spawn_incoming_hub_connection(
    mut connection: PeerConnection,
    routes: IncomingRoutes,
    resource_client: ResourceManagerClient,
    counters: Arc<SyntheticCounters>,
) {
    counters.connections.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(async move {
        let mut handshake = vec![0u8; 68];
        match connection.stream.read_exact(&mut handshake).await {
            Ok(_) => {
                let tx = handshake.get(28..48).and_then(|info_hash| {
                    routes
                        .lock()
                        .ok()
                        .and_then(|routes| routes.get(info_hash).cloned())
                });
                match tx {
                    Some(tx) => {
                        let Ok(permit) = resource_client.acquire_peer_connection().await else {
                            counters
                                .incoming_hub_route_send_errors
                                .fetch_add(1, Ordering::Relaxed);
                            counters.protocol_errors.fetch_add(1, Ordering::Relaxed);
                            return;
                        };
                        if tx.send((connection, handshake, permit)).await.is_err() {
                            counters
                                .incoming_hub_route_send_errors
                                .fetch_add(1, Ordering::Relaxed);
                            counters.protocol_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    None => {
                        counters
                            .incoming_hub_route_misses
                            .fetch_add(1, Ordering::Relaxed);
                        counters.protocol_errors.fetch_add(1, Ordering::Relaxed);
                    }
                };
            }
            Err(error) => {
                if !is_expected_connection_close(&error) {
                    counters
                        .incoming_hub_handshake_errors
                        .fetch_add(1, Ordering::Relaxed);
                    counters.protocol_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_synthetic_leecher(
    network_handle: NetworkHandle,
    spec: SyntheticTorrentSpec,
    peer_index: usize,
    addr: SocketAddr,
    transport: SyntheticTransport,
    pipeline_depth: usize,
    counters: Arc<SyntheticCounters>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let result = run_synthetic_leecher_inner(
        &network_handle,
        spec,
        peer_index,
        addr,
        transport,
        pipeline_depth,
        counters.clone(),
        &mut shutdown_rx,
    )
    .await;

    if let Err(error) = result {
        if !is_expected_connection_close(error.as_ref()) {
            record_synthetic_leecher_error(&counters, error.as_ref());
            counters.protocol_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
    counters.disconnects.fetch_add(1, Ordering::Relaxed);
}

#[allow(clippy::too_many_arguments)]
async fn run_synthetic_leecher_inner(
    network_handle: &NetworkHandle,
    spec: SyntheticTorrentSpec,
    peer_index: usize,
    addr: SocketAddr,
    transport: SyntheticTransport,
    pipeline_depth: usize,
    counters: Arc<SyntheticCounters>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> Result<(), DynError> {
    match transport {
        SyntheticTransport::Tcp => {
            let socket = bind_synthetic_leecher_socket(peer_index)?;
            let stream = socket.connect(addr).await?;
            run_synthetic_leecher_stream(
                stream,
                &spec,
                peer_index,
                pipeline_depth,
                counters,
                shutdown_rx,
            )
            .await
        }
        SyntheticTransport::Utp => {
            let network_lease = network_handle.try_lease()?;
            let connection = UtpPeerTransport::connect_from_port(&network_lease, addr, 0).await?;
            run_synthetic_leecher_stream(
                connection.stream,
                &spec,
                peer_index,
                pipeline_depth,
                counters,
                shutdown_rx,
            )
            .await
        }
        SyntheticTransport::All => Err("synthetic leecher needs a concrete transport".into()),
    }
}

async fn run_synthetic_leecher_stream<S>(
    stream: S,
    spec: &SyntheticTorrentSpec,
    peer_index: usize,
    pipeline_depth: usize,
    counters: Arc<SyntheticCounters>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> Result<(), DynError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    writer
        .write_all(&generate_message(Message::Handshake(
            spec.info_hash.clone(),
            synthetic_peer_id(b'L', peer_index),
        ))?)
        .await?;

    let mut handshake = vec![0u8; 68];
    reader.read_exact(&mut handshake).await?;
    writer
        .write_all(&generate_message(Message::Interested)?)
        .await?;

    let mut next_block = 0u64;
    let total_blocks = spec.total_size.div_ceil(BLOCK_SIZE as u64).max(1);
    let mut in_flight = 0usize;
    let mut unchoked = false;
    let mut socket_buf = vec![0u8; 64 * 1024];
    let mut parse_buf = Vec::with_capacity(256 * 1024);

    loop {
        if unchoked {
            let mut issued = 0usize;
            while in_flight < pipeline_depth && issued < LEECHER_REQUEST_BURST {
                let (piece, begin, len) =
                    block_request_for(spec.total_size, spec.piece_size, next_block);
                write_request_frame(&mut writer, piece, begin, len).await?;
                counters.leecher_requests.fetch_add(1, Ordering::Relaxed);
                in_flight += 1;
                issued += 1;
                next_block = (next_block + 1) % total_blocks;
            }
        }

        tokio::select! {
            _ = shutdown_rx.recv() => break,
            read = reader.read(&mut socket_buf) => {
                let n = read?;
                if n == 0 {
                    break;
                }
                parse_buf.extend_from_slice(&socket_buf[..n]);
                while let Some(frame) = take_frame(&mut parse_buf) {
                    match frame_message_id(&frame) {
                        Some(0) => {
                            unchoked = false;
                            in_flight = 0;
                        }
                        Some(1) => {
                            unchoked = true;
                        }
                        Some(7) => {
                            if let Some(piece_len) = parse_piece_payload_len(&frame) {
                                counters.leecher_pieces.fetch_add(1, Ordering::Relaxed);
                                counters.upload_bytes.fetch_add(piece_len as u64, Ordering::Relaxed);
                                in_flight = in_flight.saturating_sub(1);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}

struct SampleContext<'a> {
    args: &'a SyntheticLoadArgs,
    config: &'a ParsedSyntheticConfig,
    topology: RunTopology,
    add_plan: AddPlan,
    peer_plan: AddPlan,
    run_id: &'a str,
    output_dir: &'a Path,
    counters: Arc<SyntheticCounters>,
    resource_client: &'a ResourceManagerClient,
    managers: &'a mut Vec<ManagerRuntime>,
    peer_handles: &'a mut Vec<JoinHandle<()>>,
    orchestration_rx: &'a mut mpsc::UnboundedReceiver<OrchestrationUpdate>,
    orchestration_progress: &'a mut OrchestrationProgress,
    interrupt_rx: Option<watch::Receiver<bool>>,
    json_output: bool,
}

async fn run_orchestrator(
    mut add_context: AddContext,
    duration_secs: u64,
    warmup_secs: u64,
    update_tx: mpsc::UnboundedSender<OrchestrationUpdate>,
) -> Result<(), String> {
    let total = Duration::from_secs(duration_secs.saturating_add(warmup_secs));
    let start = Instant::now();

    loop {
        let elapsed = start.elapsed();
        let mut managers = Vec::new();
        let mut peer_handles = Vec::new();

        if elapsed < total {
            if let Err(error) = add_context
                .add_due_torrents(
                    elapsed,
                    &mut managers,
                    &mut peer_handles,
                    MAX_TORRENTS_PER_ORCHESTRATION_TICK,
                )
                .await
            {
                let message = error.to_string();
                let _ = update_tx.send(OrchestrationUpdate::Error(message.clone()));
                return Err(message);
            }
            if let Err(error) = add_context
                .add_due_peers(elapsed, &mut peer_handles, MAX_PEERS_PER_ORCHESTRATION_TICK)
                .await
            {
                let message = error.to_string();
                let _ = update_tx.send(OrchestrationUpdate::Error(message.clone()));
                return Err(message);
            }
        }

        let progress = add_context.progress();
        if update_tx
            .send(OrchestrationUpdate::Batch(OrchestrationBatch {
                managers,
                peer_handles,
                progress,
            }))
            .is_err()
        {
            return Ok(());
        }

        if elapsed >= total {
            let _ = update_tx.send(OrchestrationUpdate::Done(progress));
            return Ok(());
        }

        let target_torrents = add_context
            .plan
            .target_added(elapsed, add_context.specs.len());
        let target_peers = expected_active_peers(
            add_context.plan,
            add_context.peer_plan,
            add_context.topology,
            add_context.specs.len(),
            elapsed,
        );
        if progress.active_torrents < target_torrents || progress.active_peers < target_peers {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(ORCHESTRATION_IDLE_TICK).await;
        }
    }
}

fn drain_orchestration_updates(
    orchestration_rx: &mut mpsc::UnboundedReceiver<OrchestrationUpdate>,
    managers: &mut Vec<ManagerRuntime>,
    peer_handles: &mut Vec<JoinHandle<()>>,
    progress: &mut OrchestrationProgress,
) -> Result<bool, DynError> {
    let mut done = false;
    loop {
        match orchestration_rx.try_recv() {
            Ok(OrchestrationUpdate::Batch(batch)) => {
                managers.extend(batch.managers);
                peer_handles.extend(batch.peer_handles);
                *progress = batch.progress;
            }
            Ok(OrchestrationUpdate::Done(final_progress)) => {
                *progress = final_progress;
                done = true;
            }
            Ok(OrchestrationUpdate::Error(error)) => return Err(error.into()),
            Err(mpsc::error::TryRecvError::Empty) => return Ok(done),
            Err(mpsc::error::TryRecvError::Disconnected) => return Ok(done),
        }
    }
}

async fn wait_for_orchestrator(
    orchestrator_handle: &mut JoinHandle<Result<(), String>>,
    orchestration_rx: &mut mpsc::UnboundedReceiver<OrchestrationUpdate>,
    managers: &mut Vec<ManagerRuntime>,
    peer_handles: &mut Vec<JoinHandle<()>>,
    progress: &mut OrchestrationProgress,
) -> Result<(), DynError> {
    match tokio::time::timeout(Duration::from_secs(5), &mut *orchestrator_handle).await {
        Ok(join_result) => match join_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error.into()),
            Err(error) => return Err(format!("synthetic orchestrator failed: {error}").into()),
        },
        Err(_) => {
            orchestrator_handle.abort();
        }
    }
    drain_orchestration_updates(orchestration_rx, managers, peer_handles, progress)?;
    Ok(())
}

async fn sample_loop(
    context: SampleContext<'_>,
    sample_writer: &mut BufWriter<File>,
) -> Result<SyntheticSummary, DynError> {
    let SampleContext {
        args,
        config,
        topology,
        add_plan,
        peer_plan,
        run_id,
        output_dir,
        counters,
        resource_client,
        managers,
        peer_handles,
        orchestration_rx,
        orchestration_progress,
        mut interrupt_rx,
        json_output,
    } = context;

    let warmup = Duration::from_secs(args.warmup_secs);
    let measurement = Duration::from_secs(args.duration_secs);
    let total = warmup + measurement;
    let interval = Duration::from_millis(args.metrics_interval_ms);
    let start = Instant::now();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut prev_time = start;
    let mut prev_download = counters.download_bytes.load(Ordering::Relaxed);
    let mut prev_upload = counters.upload_bytes.load(Ordering::Relaxed);
    let mut last_sample_time = start;
    let mut last_sample_download = prev_download;
    let mut last_sample_upload = prev_upload;
    let mut measurement_baseline: Option<(Instant, u64, u64)> = None;
    let mut sample_count = 0u64;
    let mut max_torrent_add_lag = 0usize;
    let mut max_peer_add_lag = 0usize;
    let mut max_sample_delay_ms = 0u64;
    let mut interrupted = benchmark_interrupt_requested(interrupt_rx.as_ref());

    while start.elapsed() < total {
        if interrupted {
            break;
        }
        if let Some(interrupt_rx) = interrupt_rx.as_mut() {
            tokio::select! {
                _ = ticker.tick() => {}
                was_interrupted = wait_for_benchmark_interrupt(interrupt_rx) => {
                    interrupted = was_interrupted;
                    if interrupted {
                        break;
                    }
                    continue;
                }
            }
        } else {
            ticker.tick().await;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(start);
        drain_orchestration_updates(
            orchestration_rx,
            managers,
            peer_handles,
            orchestration_progress,
        )?;
        let active_torrents = orchestration_progress.active_torrents;
        let active_peers = orchestration_progress.active_peers;
        let target_torrents = add_plan.target_added(elapsed, args.torrents);
        let target_peers =
            expected_active_peers(add_plan, peer_plan, topology, args.torrents, elapsed);
        let torrent_add_lag = target_torrents.saturating_sub(active_torrents);
        let peer_add_lag = target_peers.saturating_sub(active_peers);
        let expected_sample_elapsed = duration_mul(interval, sample_count as usize);
        let sample_delay_ms = elapsed
            .checked_sub(expected_sample_elapsed)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        sample_count = sample_count.saturating_add(1);
        max_torrent_add_lag = max_torrent_add_lag.max(torrent_add_lag);
        max_peer_add_lag = max_peer_add_lag.max(peer_add_lag);
        max_sample_delay_ms = max_sample_delay_ms.max(sample_delay_ms);
        let phase = if elapsed < warmup {
            "warmup"
        } else {
            if measurement_baseline.is_none() {
                measurement_baseline = Some((
                    now,
                    counters.download_bytes.load(Ordering::Relaxed),
                    counters.upload_bytes.load(Ordering::Relaxed),
                ));
            }
            "measure"
        };

        let download_total = counters.download_bytes.load(Ordering::Relaxed);
        let upload_total = counters.upload_bytes.load(Ordering::Relaxed);
        let delta_secs = now.duration_since(prev_time).as_secs_f64().max(0.001);
        let download_bps = bytes_to_bits_per_second(download_total - prev_download, delta_secs);
        let upload_bps = bytes_to_bits_per_second(upload_total - prev_upload, delta_secs);

        let manager_totals = manager_totals(managers);
        let outbound_connect = outbound_connect_sample(&counters);
        let resources = resource_client
            .snapshot()
            .await
            .map(resource_samples)
            .unwrap_or_default();

        let sample = SyntheticSample {
            elapsed_ms: elapsed.as_millis(),
            phase,
            active_torrents: active_torrents as u64,
            active_peers: active_peers as u64,
            target_torrents: target_torrents as u64,
            target_peers: target_peers as u64,
            torrent_add_lag: torrent_add_lag as u64,
            peer_add_lag: peer_add_lag as u64,
            sample_delay_ms,
            download_bytes_total: download_total,
            upload_bytes_total: upload_total,
            download_bps,
            upload_bps,
            manager_download_bps: manager_totals.download_bps,
            manager_upload_bps: manager_totals.upload_bps,
            completed_pieces: manager_totals.completed_pieces,
            total_pieces: manager_totals.total_pieces,
            connected_peers_reported: manager_totals.connected_peers,
            seeder_requests: counters.seeder_requests.load(Ordering::Relaxed),
            leecher_requests: counters.leecher_requests.load(Ordering::Relaxed),
            leecher_pieces: counters.leecher_pieces.load(Ordering::Relaxed),
            connections: counters.connections.load(Ordering::Relaxed),
            disconnects: counters.disconnects.load(Ordering::Relaxed),
            protocol_errors: counters.protocol_errors.load(Ordering::Relaxed),
            protocol_error_detail: protocol_error_sample(&counters),
            manager_peer_connected: counters.manager_peer_connected.load(Ordering::Relaxed),
            manager_peer_disconnected: counters.manager_peer_disconnected.load(Ordering::Relaxed),
            outbound_connect,
            manager_block_received: counters.manager_block_received.load(Ordering::Relaxed),
            manager_block_sent: counters.manager_block_sent.load(Ordering::Relaxed),
            disk_read_started: counters.disk_read_started.load(Ordering::Relaxed),
            disk_read_finished: counters.disk_read_finished.load(Ordering::Relaxed),
            disk_write_started: counters.disk_write_started.load(Ordering::Relaxed),
            disk_write_finished: counters.disk_write_finished.load(Ordering::Relaxed),
            resources,
        };
        writeln!(sample_writer, "{}", serde_json::to_string(&sample)?)?;

        if !json_output {
            println!(
                "[{:>6.1}s {:>7}] torrents={}/{} synthetic_peers={}/{} lag={}/{} connected={} outbound={}/{}/{} down={} up={} pieces={}/{} disk_q={}/{} tick_lag={}ms",
                elapsed.as_secs_f64(),
                phase,
                sample.active_torrents,
                sample.target_torrents,
                sample.active_peers,
                sample.target_peers,
                sample.torrent_add_lag,
                sample.peer_add_lag,
                sample.connected_peers_reported,
                sample.outbound_connect.attempts,
                sample.outbound_connect.established,
                sample.outbound_connect.failed,
                format_bps(download_bps),
                format_bps(upload_bps),
                sample.completed_pieces,
                sample.total_pieces,
                sample.resources.disk_read.queued,
                sample.resources.disk_write.queued,
                sample.sample_delay_ms,
            );
        }

        prev_time = now;
        prev_download = download_total;
        prev_upload = upload_total;
        last_sample_time = now;
        last_sample_download = download_total;
        last_sample_upload = upload_total;
    }

    let (measure_start, base_download, base_upload) =
        measurement_baseline.unwrap_or((start, last_sample_download, last_sample_upload));
    let measured_secs = last_sample_time
        .duration_since(measure_start)
        .as_secs_f64()
        .max(0.001);
    let download_bytes = last_sample_download.saturating_sub(base_download);
    let upload_bytes = last_sample_upload.saturating_sub(base_upload);
    let avg_download_bps = bytes_to_bits_per_second(download_bytes, measured_secs);
    let avg_upload_bps = bytes_to_bits_per_second(upload_bytes, measured_secs);
    let manager_totals = manager_totals(managers);

    Ok(SyntheticSummary {
        run_id: run_id.to_string(),
        mode: mode_name(args.mode).to_string(),
        transport: args.transport.as_str().to_string(),
        torrent_format: args.torrent_format.as_str().to_string(),
        utp_chaos: shared_udp_chaos_env_value(args.utp_chaos),
        add_mode: add_mode_name(add_plan.mode).to_string(),
        peer_add_mode: add_mode_name(peer_plan.mode).to_string(),
        torrents: args.torrents,
        torrents_added: orchestration_progress.active_torrents,
        peers_added: orchestration_progress.active_peers,
        requested_peers: args.peers,
        download_peers: topology.download_peers,
        upload_peers: topology.upload_peers,
        add_interval_ms: add_plan.interval.as_millis() as u64,
        add_burst_size: add_plan.burst_size,
        peer_add_interval_ms: peer_plan.interval.as_millis() as u64,
        peer_add_burst_size: peer_plan.burst_size,
        size_per_torrent_bytes: config.size_per_torrent,
        piece_size_bytes: config.piece_size,
        duration_secs: args.duration_secs,
        warmup_secs: args.warmup_secs,
        measured_secs,
        max_torrent_add_lag,
        max_peer_add_lag,
        max_sample_delay_ms,
        download_bytes,
        upload_bytes,
        avg_download_bps,
        avg_upload_bps,
        avg_download_mbps: avg_download_bps as f64 / 1_000_000.0,
        avg_upload_mbps: avg_upload_bps as f64 / 1_000_000.0,
        completed_pieces: manager_totals.completed_pieces,
        total_pieces: manager_totals.total_pieces,
        seeder_requests: counters.seeder_requests.load(Ordering::Relaxed),
        leecher_requests: counters.leecher_requests.load(Ordering::Relaxed),
        leecher_pieces: counters.leecher_pieces.load(Ordering::Relaxed),
        connections: counters.connections.load(Ordering::Relaxed),
        disconnects: counters.disconnects.load(Ordering::Relaxed),
        protocol_errors: counters.protocol_errors.load(Ordering::Relaxed),
        protocol_error_detail: protocol_error_sample(&counters),
        manager_peer_connected: counters.manager_peer_connected.load(Ordering::Relaxed),
        manager_peer_disconnected: counters.manager_peer_disconnected.load(Ordering::Relaxed),
        outbound_connect: outbound_connect_sample(&counters),
        manager_block_received: counters.manager_block_received.load(Ordering::Relaxed),
        manager_block_sent: counters.manager_block_sent.load(Ordering::Relaxed),
        disk_read_started: counters.disk_read_started.load(Ordering::Relaxed),
        disk_read_finished: counters.disk_read_finished.load(Ordering::Relaxed),
        disk_write_started: counters.disk_write_started.load(Ordering::Relaxed),
        disk_write_finished: counters.disk_write_finished.load(Ordering::Relaxed),
        output_dir: output_dir.to_path_buf(),
        interrupted,
    })
}

async fn shutdown_managers(managers: &mut [ManagerRuntime]) {
    for manager in managers.iter() {
        let _ = manager.command_tx.send(ManagerCommand::Shutdown).await;
    }

    if tokio::time::timeout(Duration::from_secs(5), async {
        for manager in managers.iter_mut() {
            let _ = (&mut manager.handle).await;
        }
    })
    .await
    .is_err()
    {
        for manager in managers.iter_mut() {
            if !manager.handle.is_finished() {
                manager.handle.abort();
            }
        }
        for manager in managers.iter_mut() {
            let _ = (&mut manager.handle).await;
        }
    }
}

async fn collect_manager_events(
    mut event_rx: mpsc::Receiver<ManagerEvent>,
    counters: Arc<SyntheticCounters>,
) {
    while let Some(event) = event_rx.recv().await {
        match event {
            ManagerEvent::PeerConnected { .. } => {
                counters
                    .manager_peer_connected
                    .fetch_add(1, Ordering::Relaxed);
            }
            ManagerEvent::PeerDisconnected { .. } => {
                counters
                    .manager_peer_disconnected
                    .fetch_add(1, Ordering::Relaxed);
            }
            ManagerEvent::PeerConnectAttempted { transport } => {
                counters
                    .outbound_connect_attempts
                    .fetch_add(1, Ordering::Relaxed);
                increment_outbound_connect_attempt(&counters, transport);
            }
            ManagerEvent::PeerConnectEstablished { transport } => {
                counters
                    .outbound_connect_established
                    .fetch_add(1, Ordering::Relaxed);
                increment_outbound_connect_established(&counters, transport);
            }
            ManagerEvent::PeerConnectFailed { transport, reason } => {
                counters
                    .outbound_connect_failed
                    .fetch_add(1, Ordering::Relaxed);
                increment_outbound_connect_failed(&counters, transport);
                match reason {
                    SyntheticPeerConnectFailure::PermitTimeout => {
                        counters
                            .outbound_permit_timeout
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::PermitManagerShutdown => {
                        counters
                            .outbound_permit_manager_shutdown
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::PermitQueueFull => {
                        counters
                            .outbound_permit_queue_full
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::ConnectTimeout => {
                        counters
                            .outbound_connect_timeout
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::ConnectionRefused => {
                        counters
                            .outbound_connection_refused
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::ConnectionReset => {
                        counters
                            .outbound_connection_reset
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::ConnectionAborted => {
                        counters
                            .outbound_connection_aborted
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::AddrInUse => {
                        counters
                            .outbound_addr_in_use
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::AddrNotAvailable => {
                        counters
                            .outbound_addr_not_available
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::TimedOut => {
                        counters.outbound_timed_out.fetch_add(1, Ordering::Relaxed);
                    }
                    SyntheticPeerConnectFailure::OtherIo => {
                        counters.outbound_other_io.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            ManagerEvent::PeerSessionFailed => {
                counters
                    .outbound_session_failed
                    .fetch_add(1, Ordering::Relaxed);
            }
            ManagerEvent::BlockReceived { .. } => {
                counters
                    .manager_block_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            ManagerEvent::BlockSent { .. } => {
                counters.manager_block_sent.fetch_add(1, Ordering::Relaxed);
            }
            ManagerEvent::DiskReadStarted { .. } => {
                counters.disk_read_started.fetch_add(1, Ordering::Relaxed);
            }
            ManagerEvent::DiskReadFinished => {
                counters.disk_read_finished.fetch_add(1, Ordering::Relaxed);
            }
            ManagerEvent::DiskWriteStarted { .. } => {
                counters.disk_write_started.fetch_add(1, Ordering::Relaxed);
            }
            ManagerEvent::DiskWriteFinished { .. } => {
                counters.disk_write_finished.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

fn topology_for(
    mode: SyntheticLoadMode,
    peers: usize,
    torrents: usize,
) -> Result<RunTopology, DynError> {
    let topology = match mode {
        SyntheticLoadMode::Download => RunTopology {
            download_peers: peers,
            upload_peers: 0,
        },
        SyntheticLoadMode::Upload => RunTopology {
            download_peers: 0,
            upload_peers: peers,
        },
        SyntheticLoadMode::Swarm => {
            if peers < 2 {
                return Err("--mode swarm requires at least 2 peers".into());
            }
            let download_peers = peers / 2;
            RunTopology {
                download_peers,
                upload_peers: peers - download_peers,
            }
        }
    };

    if topology.download_peers > 0 && topology.download_peers < torrents {
        return Err(
            "--peers must be at least --torrents for the active download side of this harness"
                .into(),
        );
    }
    if topology.upload_peers > 0 && topology.upload_peers < torrents {
        return Err(
            "--peers must be at least --torrents for the active upload side of this harness".into(),
        );
    }

    Ok(topology)
}

fn benchmark_step_plans(
    args: &SyntheticBenchmarkArgs,
    config: &ParsedBenchmarkConfig,
    mode: SyntheticLoadMode,
) -> Result<Vec<BenchmarkStepPlan>, DynError> {
    let mut torrents = args.start_torrents.min(args.max_torrents);
    let mut peers = args.start_peers.min(args.max_peers);
    let mut plans = Vec::new();

    for step_index in 0..args.max_steps {
        let step_peers = benchmark_step_peers(mode, torrents, peers, args.max_peers)?;
        let size_per_torrent = benchmark_size_per_torrent(config, mode, torrents)?;
        plans.push(BenchmarkStepPlan {
            step: step_index + 1,
            planned_steps: 0,
            torrents,
            peers: step_peers,
            size_per_torrent_bytes: size_per_torrent,
            estimated_disk_bytes: estimated_disk_bytes(mode, torrents, size_per_torrent),
            estimated_final_disk_bytes: 0,
            disk_budget_bytes: config.disk_budget,
        });

        if torrents == args.max_torrents && peers == args.max_peers {
            break;
        }

        let next = next_benchmark_step(mode, torrents, peers, args.max_torrents, args.max_peers);
        if next == (torrents, peers) {
            break;
        }
        torrents = next.0;
        peers = next.1;
    }

    let planned_steps = plans.len();
    let final_estimated_disk_bytes = plans
        .last()
        .map(|plan| plan.estimated_disk_bytes)
        .unwrap_or_default();
    for plan in &mut plans {
        plan.planned_steps = planned_steps;
        plan.estimated_final_disk_bytes = final_estimated_disk_bytes;
    }

    Ok(plans)
}

fn benchmark_total_planned_steps(
    args: &SyntheticBenchmarkArgs,
    config: &ParsedBenchmarkConfig,
    modes: &[SyntheticLoadMode],
) -> usize {
    modes
        .iter()
        .map(|&mode| {
            benchmark_step_plans(args, config, mode)
                .map(|plans| plans.len())
                .unwrap_or(1)
        })
        .sum::<usize>()
        .max(1)
}

fn remaining_steps_in_current_scenario(plan: &BenchmarkStepPlan) -> usize {
    plan.planned_steps.saturating_sub(plan.step)
}

fn remaining_steps_after_issue(plan: &BenchmarkStepPlan) -> usize {
    remaining_steps_in_current_scenario(plan)
}

async fn sleep_before_benchmark_retry(
    retry_delay_ms: u64,
    interrupt_rx: &mut watch::Receiver<bool>,
) -> bool {
    if retry_delay_ms == 0 {
        return benchmark_interrupted(interrupt_rx);
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(retry_delay_ms)) => {
            benchmark_interrupted(interrupt_rx)
        }
        was_interrupted = wait_for_benchmark_interrupt(interrupt_rx) => was_interrupted,
    }
}

fn benchmark_synthetic_args(
    args: &SyntheticBenchmarkArgs,
    mode: SyntheticLoadMode,
    torrents: usize,
    peers: usize,
    size_per_torrent: u64,
    out: PathBuf,
) -> SyntheticLoadArgs {
    SyntheticLoadArgs {
        torrents,
        peers,
        mode,
        add_mode: SyntheticLoadAddMode::Upfront,
        add_interval_ms: 1000,
        add_burst_size: 1,
        peer_add_mode: SyntheticLoadAddMode::Staggered,
        peer_add_interval_ms: args.peer_add_interval_ms,
        peer_add_burst_size: args.peer_add_burst_size,
        size_per_torrent: size_per_torrent.to_string(),
        piece_size: args.piece_size.clone(),
        torrent_format: args.torrent_format,
        duration_secs: args.duration_secs,
        warmup_secs: args.warmup_secs,
        metrics_interval_ms: args.metrics_interval_ms,
        leecher_pipeline: args.leecher_pipeline,
        target_gbps: Some(args.target_gbps),
        transport: args.transport,
        utp_chaos: args.utp_chaos,
        peer_connection_permits: args.peer_connection_permits,
        disk_read_permits: args.disk_read_permits,
        disk_write_permits: args.disk_write_permits,
        out,
    }
}

fn benchmark_size_per_torrent(
    config: &ParsedBenchmarkConfig,
    mode: SyntheticLoadMode,
    torrents: usize,
) -> Result<u64, DynError> {
    let side_multiplier = disk_side_multiplier(mode) as u64;
    let budget_per_torrent = config
        .disk_budget
        .checked_div((torrents as u64).saturating_mul(side_multiplier).max(1))
        .unwrap_or(0);
    let requested = config.preferred_size_per_torrent.min(budget_per_torrent);
    if requested < config.piece_size {
        return Err(format!(
            "{} torrents in {} mode need at least {} of disk budget at piece size {}; current budget is {}",
            torrents,
            mode_name(mode),
            format_bytes(estimated_disk_bytes(mode, torrents, config.piece_size)),
            format_bytes(config.piece_size),
            format_bytes(config.disk_budget)
        )
        .into());
    }

    let pieces = (requested / config.piece_size).max(1);
    Ok(pieces.saturating_mul(config.piece_size))
}

fn disk_side_multiplier(mode: SyntheticLoadMode) -> usize {
    match mode {
        SyntheticLoadMode::Swarm => 2,
        SyntheticLoadMode::Download | SyntheticLoadMode::Upload => 1,
    }
}

fn estimated_disk_bytes(mode: SyntheticLoadMode, torrents: usize, size_per_torrent: u64) -> u64 {
    size_per_torrent
        .saturating_mul(torrents as u64)
        .saturating_mul(disk_side_multiplier(mode) as u64)
}

fn benchmark_step_peers(
    mode: SyntheticLoadMode,
    torrents: usize,
    peers: usize,
    max_peers: usize,
) -> Result<usize, DynError> {
    let min_peers = benchmark_min_peers(mode, torrents);
    if min_peers > max_peers {
        return Err(format!(
            "{} torrents in {} mode need at least {} peers; --max-peers is {}",
            torrents,
            mode_name(mode),
            min_peers,
            max_peers
        )
        .into());
    }
    Ok(peers.max(min_peers).min(max_peers))
}

fn benchmark_min_peers(mode: SyntheticLoadMode, torrents: usize) -> usize {
    match mode {
        SyntheticLoadMode::Swarm => torrents.saturating_mul(2),
        SyntheticLoadMode::Download | SyntheticLoadMode::Upload => torrents,
    }
}

fn next_benchmark_step(
    mode: SyntheticLoadMode,
    torrents: usize,
    peers: usize,
    max_torrents: usize,
    max_peers: usize,
) -> (usize, usize) {
    if torrents < max_torrents {
        let next_torrents = torrents.saturating_mul(2).min(max_torrents);
        (
            next_torrents,
            peers.max(benchmark_min_peers(mode, next_torrents).min(max_peers)),
        )
    } else {
        (torrents, peers.saturating_mul(2).min(max_peers))
    }
}

fn benchmark_issues(summary: &SyntheticSummary, args: &SyntheticBenchmarkArgs) -> Vec<String> {
    let mut issues = Vec::new();
    if summary.interrupted {
        issues.push(BENCHMARK_INTERRUPT_ISSUE.to_string());
        return issues;
    }
    if summary.torrents_added < summary.torrents {
        issues.push(format!(
            "torrent_add_lag: added {}/{}",
            summary.torrents_added, summary.torrents
        ));
    }
    if summary.peers_added < summary.requested_peers {
        issues.push(format!(
            "peer_add_lag: added {}/{}",
            summary.peers_added, summary.requested_peers
        ));
    }
    if summary.max_sample_delay_ms > args.max_sample_delay_ms {
        issues.push(format!(
            "sample_delay: {}ms > {}ms",
            summary.max_sample_delay_ms, args.max_sample_delay_ms
        ));
    }
    if summary.protocol_errors > 0 {
        issues.push(format!("protocol_errors: {}", summary.protocol_errors));
    }
    if summary.outbound_connect.permit_timeout > 0 {
        issues.push(format!(
            "outbound_permit_timeout: {}",
            summary.outbound_connect.permit_timeout
        ));
    }
    if summary.outbound_connect.connect_timeout > 0 {
        issues.push(format!(
            "outbound_connect_timeout: {}",
            summary.outbound_connect.connect_timeout
        ));
    }
    if summary.outbound_connect.connection_refused > 0 {
        issues.push(format!(
            "outbound_connection_refused: {}",
            summary.outbound_connect.connection_refused
        ));
    }
    if summary
        .protocol_error_detail
        .synthetic_leecher_connection_refused
        > 0
    {
        issues.push(format!(
            "synthetic_leecher_connection_refused: {}",
            summary
                .protocol_error_detail
                .synthetic_leecher_connection_refused
        ));
    }
    issues
}

fn benchmark_step_summary(
    summary: &SyntheticSummary,
    plan: &BenchmarkStepPlan,
    attempt: BenchmarkAttemptContext,
    samples_path: PathBuf,
    summary_path: PathBuf,
    issues: Vec<String>,
    data_removed: bool,
) -> BenchmarkStepSummary {
    BenchmarkStepSummary {
        step: plan.step,
        planned_steps: plan.planned_steps,
        attempt: attempt.attempt,
        max_attempts: attempt.max_attempts,
        will_retry: attempt.will_retry,
        retry_delay_ms: if attempt.will_retry {
            attempt.retry_delay_ms
        } else {
            0
        },
        mode: summary.mode.clone(),
        torrents: summary.torrents,
        peers: summary.requested_peers,
        size_per_torrent_bytes: plan.size_per_torrent_bytes,
        estimated_disk_bytes: plan.estimated_disk_bytes,
        estimated_final_disk_bytes: plan.estimated_final_disk_bytes,
        disk_budget_bytes: plan.disk_budget_bytes,
        measured_secs: summary.measured_secs,
        wall_secs: attempt.timing.wall_secs,
        eta: attempt.timing.eta,
        download_bytes: summary.download_bytes,
        upload_bytes: summary.upload_bytes,
        avg_download_bps: summary.avg_download_bps,
        avg_upload_bps: summary.avg_upload_bps,
        avg_download_mbps: summary.avg_download_mbps,
        avg_upload_mbps: summary.avg_upload_mbps,
        torrents_added: summary.torrents_added,
        peers_added: summary.peers_added,
        requested_peers: summary.requested_peers,
        max_peer_add_lag: summary.max_peer_add_lag,
        max_sample_delay_ms: summary.max_sample_delay_ms,
        protocol_errors: summary.protocol_errors,
        protocol_error_detail: summary.protocol_error_detail.clone(),
        outbound_failed: summary.outbound_connect.failed,
        outbound_permit_timeout: summary.outbound_connect.permit_timeout,
        outbound_connect: summary.outbound_connect.clone(),
        synthetic_leecher_errors: summary.protocol_error_detail.synthetic_leecher,
        seeder_requests: summary.seeder_requests,
        leecher_requests: summary.leecher_requests,
        leecher_pieces: summary.leecher_pieces,
        connections: summary.connections,
        disconnects: summary.disconnects,
        manager_peer_connected: summary.manager_peer_connected,
        manager_peer_disconnected: summary.manager_peer_disconnected,
        manager_block_received: summary.manager_block_received,
        manager_block_sent: summary.manager_block_sent,
        disk_read_started: summary.disk_read_started,
        disk_read_finished: summary.disk_read_finished,
        disk_write_started: summary.disk_write_started,
        disk_write_finished: summary.disk_write_finished,
        completed_pieces: summary.completed_pieces,
        total_pieces: summary.total_pieces,
        error: None,
        issues,
        summary_path: Some(summary_path),
        samples_path: Some(samples_path),
        data_removed,
    }
}

fn benchmark_failed_step_summary(
    mode: SyntheticLoadMode,
    plan: &BenchmarkStepPlan,
    attempt: BenchmarkAttemptContext,
    error: String,
) -> BenchmarkStepSummary {
    let issue = format!("runtime_error: {error}");
    BenchmarkStepSummary {
        step: plan.step,
        planned_steps: plan.planned_steps,
        attempt: attempt.attempt,
        max_attempts: attempt.max_attempts,
        will_retry: attempt.will_retry,
        retry_delay_ms: if attempt.will_retry {
            attempt.retry_delay_ms
        } else {
            0
        },
        mode: mode_name(mode).to_string(),
        torrents: plan.torrents,
        peers: plan.peers,
        size_per_torrent_bytes: plan.size_per_torrent_bytes,
        estimated_disk_bytes: plan.estimated_disk_bytes,
        estimated_final_disk_bytes: plan.estimated_final_disk_bytes,
        disk_budget_bytes: plan.disk_budget_bytes,
        measured_secs: 0.0,
        wall_secs: attempt.timing.wall_secs,
        eta: attempt.timing.eta,
        download_bytes: 0,
        upload_bytes: 0,
        avg_download_bps: 0,
        avg_upload_bps: 0,
        avg_download_mbps: 0.0,
        avg_upload_mbps: 0.0,
        torrents_added: 0,
        peers_added: 0,
        requested_peers: plan.peers,
        max_peer_add_lag: plan.peers,
        max_sample_delay_ms: 0,
        protocol_errors: 0,
        protocol_error_detail: ProtocolErrorSample::default(),
        outbound_failed: 0,
        outbound_permit_timeout: 0,
        outbound_connect: OutboundConnectSample::default(),
        synthetic_leecher_errors: 0,
        seeder_requests: 0,
        leecher_requests: 0,
        leecher_pieces: 0,
        connections: 0,
        disconnects: 0,
        manager_peer_connected: 0,
        manager_peer_disconnected: 0,
        manager_block_received: 0,
        manager_block_sent: 0,
        disk_read_started: 0,
        disk_read_finished: 0,
        disk_write_started: 0,
        disk_write_finished: 0,
        completed_pieces: 0,
        total_pieces: 0,
        error: Some(error),
        issues: vec![issue],
        summary_path: None,
        samples_path: None,
        data_removed: false,
    }
}

fn benchmark_failed_profile_summary(
    args: &SyntheticBenchmarkArgs,
    config: &ParsedBenchmarkConfig,
    mode: SyntheticLoadMode,
    error: String,
    progress: &mut BenchmarkRunProgress,
) -> BenchmarkProfileSummary {
    let torrents = args.start_torrents.min(args.max_torrents);
    let min_peers = benchmark_min_peers(mode, torrents);
    let peers = args
        .start_peers
        .min(args.max_peers)
        .max(min_peers.min(args.max_peers));
    let size_per_torrent =
        benchmark_size_per_torrent(config, mode, torrents).unwrap_or(config.piece_size);
    let estimated_disk = estimated_disk_bytes(mode, torrents, size_per_torrent);
    let plan = BenchmarkStepPlan {
        step: 1,
        planned_steps: 1,
        torrents,
        peers,
        size_per_torrent_bytes: size_per_torrent,
        estimated_disk_bytes: estimated_disk,
        estimated_final_disk_bytes: estimated_disk,
        disk_budget_bytes: config.disk_budget,
    };
    let timing = progress.record_step(0.0, 0, 0, 0);
    let step = benchmark_failed_step_summary(
        mode,
        &plan,
        BenchmarkAttemptContext {
            attempt: 1,
            max_attempts: 1,
            will_retry: false,
            retry_delay_ms: 0,
            timing,
        },
        error,
    );
    let steps = vec![step.clone()];

    BenchmarkProfileSummary {
        mode: mode_name(mode).to_string(),
        planned_steps: 1,
        final_torrents: torrents,
        final_peers: peers,
        final_size_per_torrent_bytes: size_per_torrent,
        final_estimated_disk_bytes: estimated_disk,
        metrics: benchmark_profile_metrics(&steps),
        last_clean: None,
        first_issue: Some(step),
        steps,
    }
}

fn benchmark_profile_metrics(steps: &[BenchmarkStepSummary]) -> BenchmarkProfileMetrics {
    let mut metrics = BenchmarkProfileMetrics {
        steps_run: steps.len(),
        retry_attempts: steps.iter().filter(|step| step.attempt > 1).count(),
        transient_issue_attempts: steps
            .iter()
            .filter(|step| !step.issues.is_empty() && step.will_retry)
            .count(),
        recovered_after_retry_steps: steps
            .iter()
            .filter(|step| step.issues.is_empty() && step.attempt > 1)
            .count(),
        final_issue_steps: steps
            .iter()
            .filter(|step| !step.issues.is_empty() && !step.will_retry)
            .count(),
        clean_steps: steps.iter().filter(|step| step.issues.is_empty()).count(),
        issue_steps: steps.iter().filter(|step| !step.issues.is_empty()).count(),
        total_measured_secs: steps.iter().map(|step| step.measured_secs).sum(),
        total_download_bytes: 0,
        total_upload_bytes: 0,
        max_download_bps: 0,
        max_upload_bps: 0,
        max_sample_delay_ms: 0,
        estimated_disk_high_water_bytes: 0,
        protocol_errors: 0,
        protocol_error_detail: ProtocolErrorSample::default(),
        outbound_failed: 0,
        outbound_permit_timeout: 0,
        outbound_connect: OutboundConnectSample::default(),
        synthetic_leecher_errors: 0,
        seeder_requests: 0,
        leecher_requests: 0,
        leecher_pieces: 0,
        connections: 0,
        disconnects: 0,
        manager_peer_connected: 0,
        manager_peer_disconnected: 0,
        manager_block_received: 0,
        manager_block_sent: 0,
        disk_read_started: 0,
        disk_read_finished: 0,
        disk_write_started: 0,
        disk_write_finished: 0,
        completed_pieces: 0,
        total_pieces: 0,
        data_removed_steps: 0,
        data_kept_steps: 0,
    };

    for step in steps {
        metrics.total_download_bytes = metrics
            .total_download_bytes
            .saturating_add(step.download_bytes);
        metrics.total_upload_bytes = metrics.total_upload_bytes.saturating_add(step.upload_bytes);
        metrics.max_download_bps = metrics.max_download_bps.max(step.avg_download_bps);
        metrics.max_upload_bps = metrics.max_upload_bps.max(step.avg_upload_bps);
        metrics.max_sample_delay_ms = metrics.max_sample_delay_ms.max(step.max_sample_delay_ms);
        metrics.estimated_disk_high_water_bytes = metrics
            .estimated_disk_high_water_bytes
            .max(step.estimated_disk_bytes);
        metrics.protocol_errors = metrics.protocol_errors.saturating_add(step.protocol_errors);
        add_protocol_error_sample(
            &mut metrics.protocol_error_detail,
            &step.protocol_error_detail,
        );
        metrics.outbound_failed = metrics.outbound_failed.saturating_add(step.outbound_failed);
        metrics.outbound_permit_timeout = metrics
            .outbound_permit_timeout
            .saturating_add(step.outbound_permit_timeout);
        add_outbound_connect_sample(&mut metrics.outbound_connect, &step.outbound_connect);
        metrics.synthetic_leecher_errors = metrics
            .synthetic_leecher_errors
            .saturating_add(step.synthetic_leecher_errors);
        metrics.seeder_requests = metrics.seeder_requests.saturating_add(step.seeder_requests);
        metrics.leecher_requests = metrics
            .leecher_requests
            .saturating_add(step.leecher_requests);
        metrics.leecher_pieces = metrics.leecher_pieces.saturating_add(step.leecher_pieces);
        metrics.connections = metrics.connections.saturating_add(step.connections);
        metrics.disconnects = metrics.disconnects.saturating_add(step.disconnects);
        metrics.manager_peer_connected = metrics
            .manager_peer_connected
            .saturating_add(step.manager_peer_connected);
        metrics.manager_peer_disconnected = metrics
            .manager_peer_disconnected
            .saturating_add(step.manager_peer_disconnected);
        metrics.manager_block_received = metrics
            .manager_block_received
            .saturating_add(step.manager_block_received);
        metrics.manager_block_sent = metrics
            .manager_block_sent
            .saturating_add(step.manager_block_sent);
        metrics.disk_read_started = metrics
            .disk_read_started
            .saturating_add(step.disk_read_started);
        metrics.disk_read_finished = metrics
            .disk_read_finished
            .saturating_add(step.disk_read_finished);
        metrics.disk_write_started = metrics
            .disk_write_started
            .saturating_add(step.disk_write_started);
        metrics.disk_write_finished = metrics
            .disk_write_finished
            .saturating_add(step.disk_write_finished);
        metrics.completed_pieces = metrics
            .completed_pieces
            .saturating_add(step.completed_pieces);
        metrics.total_pieces = metrics.total_pieces.saturating_add(step.total_pieces);
        if step.data_removed {
            metrics.data_removed_steps += 1;
        } else if step.summary_path.is_some() {
            metrics.data_kept_steps += 1;
        }
    }

    metrics
}

fn add_protocol_error_sample(total: &mut ProtocolErrorSample, sample: &ProtocolErrorSample) {
    total.synthetic_seeder = total
        .synthetic_seeder
        .saturating_add(sample.synthetic_seeder);
    total.incoming_hub_handshake = total
        .incoming_hub_handshake
        .saturating_add(sample.incoming_hub_handshake);
    total.incoming_hub_route_miss = total
        .incoming_hub_route_miss
        .saturating_add(sample.incoming_hub_route_miss);
    total.incoming_hub_route_send = total
        .incoming_hub_route_send
        .saturating_add(sample.incoming_hub_route_send);
    total.synthetic_leecher = total
        .synthetic_leecher
        .saturating_add(sample.synthetic_leecher);
    total.synthetic_leecher_addr_in_use = total
        .synthetic_leecher_addr_in_use
        .saturating_add(sample.synthetic_leecher_addr_in_use);
    total.synthetic_leecher_addr_not_available = total
        .synthetic_leecher_addr_not_available
        .saturating_add(sample.synthetic_leecher_addr_not_available);
    total.synthetic_leecher_connection_refused = total
        .synthetic_leecher_connection_refused
        .saturating_add(sample.synthetic_leecher_connection_refused);
    total.synthetic_leecher_timed_out = total
        .synthetic_leecher_timed_out
        .saturating_add(sample.synthetic_leecher_timed_out);
    total.synthetic_leecher_other_io = total
        .synthetic_leecher_other_io
        .saturating_add(sample.synthetic_leecher_other_io);
    total.synthetic_leecher_non_io = total
        .synthetic_leecher_non_io
        .saturating_add(sample.synthetic_leecher_non_io);
}

fn add_outbound_connect_sample(total: &mut OutboundConnectSample, sample: &OutboundConnectSample) {
    total.attempts = total.attempts.saturating_add(sample.attempts);
    total.established = total.established.saturating_add(sample.established);
    total.failed = total.failed.saturating_add(sample.failed);
    for transport in &sample.by_transport {
        add_outbound_connect_transport_sample(&mut total.by_transport, transport);
    }
    total.permit_timeout = total.permit_timeout.saturating_add(sample.permit_timeout);
    total.permit_manager_shutdown = total
        .permit_manager_shutdown
        .saturating_add(sample.permit_manager_shutdown);
    total.permit_queue_full = total
        .permit_queue_full
        .saturating_add(sample.permit_queue_full);
    total.connect_timeout = total.connect_timeout.saturating_add(sample.connect_timeout);
    total.connection_refused = total
        .connection_refused
        .saturating_add(sample.connection_refused);
    total.connection_reset = total
        .connection_reset
        .saturating_add(sample.connection_reset);
    total.connection_aborted = total
        .connection_aborted
        .saturating_add(sample.connection_aborted);
    total.addr_in_use = total.addr_in_use.saturating_add(sample.addr_in_use);
    total.addr_not_available = total
        .addr_not_available
        .saturating_add(sample.addr_not_available);
    total.timed_out = total.timed_out.saturating_add(sample.timed_out);
    total.other_io = total.other_io.saturating_add(sample.other_io);
    total.session_failed = total.session_failed.saturating_add(sample.session_failed);
}

fn add_outbound_connect_transport_sample(
    total: &mut Vec<OutboundConnectTransportSample>,
    sample: &OutboundConnectTransportSample,
) {
    if let Some(existing) = total
        .iter_mut()
        .find(|existing| existing.transport == sample.transport)
    {
        existing.attempts = existing.attempts.saturating_add(sample.attempts);
        existing.established = existing.established.saturating_add(sample.established);
        existing.failed = existing.failed.saturating_add(sample.failed);
        return;
    }

    total.push(sample.clone());
}

fn benchmark_report(
    args: &SyntheticBenchmarkArgs,
    config: &ParsedBenchmarkConfig,
    profiles: &[BenchmarkProfileSummary],
    planned_steps: usize,
    runtime_secs: f64,
    interrupted: bool,
) -> BenchmarkReport {
    let steps_run = profiles
        .iter()
        .map(|profile| profile.metrics.steps_run)
        .sum();
    let clean_steps = profiles
        .iter()
        .map(|profile| profile.metrics.clean_steps)
        .sum();
    let issue_steps = profiles
        .iter()
        .map(|profile| profile.metrics.issue_steps)
        .sum();
    let retry_attempts = profiles
        .iter()
        .map(|profile| profile.metrics.retry_attempts)
        .sum();
    let transient_issue_attempts = profiles
        .iter()
        .map(|profile| profile.metrics.transient_issue_attempts)
        .sum();
    let recovered_after_retry_steps = profiles
        .iter()
        .map(|profile| profile.metrics.recovered_after_retry_steps)
        .sum();
    let scenarios = profiles
        .iter()
        .map(|profile| benchmark_scenario_report(args, profile))
        .collect();

    BenchmarkReport {
        interrupted,
        runtime_secs,
        runtime: format_duration_secs(runtime_secs),
        planned_steps,
        steps_run,
        retry_attempts,
        transient_issue_attempts,
        recovered_after_retry_steps,
        clean_steps,
        issue_steps,
        configured_max_torrents: args.max_torrents,
        configured_max_peers: args.max_peers,
        disk_budget_bytes: config.disk_budget,
        preferred_size_per_torrent_bytes: config.preferred_size_per_torrent,
        piece_size_bytes: config.piece_size,
        torrent_format: args.torrent_format.as_str().to_string(),
        issue_retries: args.issue_retries,
        retry_delay_ms: args.retry_delay_ms,
        peer_connection_limit_policy: peer_connection_limit_policy(args),
        os_limit_note: os_limit_note(),
        scenarios,
    }
}

fn benchmark_scenario_report(
    args: &SyntheticBenchmarkArgs,
    profile: &BenchmarkProfileSummary,
) -> BenchmarkScenarioReport {
    let clean = profile.last_clean.as_ref();
    let issue = profile.first_issue.as_ref();
    let capacity_step = clean.or(issue).or_else(|| profile.steps.last());
    let clean_torrents = clean.map(|step| step.torrents).unwrap_or_default();
    let clean_peers = clean.map(|step| step.peers).unwrap_or_default();
    let clean_disk_working_set_bytes = clean
        .map(|step| step.estimated_disk_bytes)
        .unwrap_or_default();
    let clean_size_per_torrent_bytes = clean
        .map(|step| step.size_per_torrent_bytes)
        .unwrap_or_default();
    let runtime_secs = profile.steps.iter().map(|step| step.wall_secs).sum::<f64>();
    let measured_secs = profile.metrics.total_measured_secs.max(0.001);
    let disk_read_ops_per_sec = profile.metrics.disk_read_finished as f64 / runtime_secs.max(0.001);
    let disk_write_ops_per_sec =
        profile.metrics.disk_write_finished as f64 / runtime_secs.max(0.001);

    BenchmarkScenarioReport {
        mode: profile.mode.clone(),
        verdict: benchmark_verdict(profile),
        capacity_estimate: benchmark_capacity_estimate(profile),
        clean_torrents,
        clean_peers,
        clean_disk_working_set_bytes,
        clean_size_per_torrent_bytes,
        first_issue_torrents: issue.map(|step| step.torrents),
        first_issue_peers: issue.map(|step| step.peers),
        first_issue: issue.map(|step| step.issues.join("; ")),
        likely_bottleneck: likely_bottleneck(profile),
        runtime_secs,
        steps_run: profile.metrics.steps_run,
        retry_attempts: profile.metrics.retry_attempts,
        transient_issue_attempts: profile.metrics.transient_issue_attempts,
        recovered_after_retry_steps: profile.metrics.recovered_after_retry_steps,
        planned_steps: profile.planned_steps,
        peak_download_bps: profile.metrics.max_download_bps,
        peak_upload_bps: profile.metrics.max_upload_bps,
        observed_disk_read_bytes_per_sec: bytes_per_second(
            profile.metrics.total_upload_bytes,
            measured_secs,
        ),
        observed_disk_write_bytes_per_sec: bytes_per_second(
            profile.metrics.total_download_bytes,
            measured_secs,
        ),
        disk_read_ops_per_sec,
        disk_write_ops_per_sec,
        max_sample_delay_ms: profile.metrics.max_sample_delay_ms,
        protocol_errors: profile.metrics.protocol_errors,
        outbound_failed: profile.metrics.outbound_failed,
        outbound_permit_timeout: profile.metrics.outbound_permit_timeout,
        peer_connection_limit: capacity_step
            .map(|step| effective_peer_connection_limit(step.peers, args.peer_connection_permits))
            .unwrap_or_default(),
        disk_read_permits: args.disk_read_permits,
        disk_write_permits: args.disk_write_permits,
    }
}

fn benchmark_verdict(profile: &BenchmarkProfileSummary) -> String {
    if profile
        .first_issue
        .as_ref()
        .map(step_was_interrupted)
        .unwrap_or(false)
    {
        return "interrupted".to_string();
    }
    match (&profile.last_clean, &profile.first_issue) {
        (Some(clean), None) if clean.step >= profile.planned_steps => {
            "clean_to_configured_limit".to_string()
        }
        (Some(_), None) => "clean_until_stopped".to_string(),
        (Some(_), Some(_)) => "bounded_by_first_issue".to_string(),
        (None, Some(_)) => "failed_first_step".to_string(),
        (None, None) => "no_steps".to_string(),
    }
}

fn benchmark_capacity_estimate(profile: &BenchmarkProfileSummary) -> String {
    if profile
        .first_issue
        .as_ref()
        .map(step_was_interrupted)
        .unwrap_or(false)
    {
        return match &profile.last_clean {
            Some(clean) => format!(
                "clean through {} torrents / {} peers before Ctrl+C",
                clean.torrents, clean.peers
            ),
            None => "interrupted before a clean step completed".to_string(),
        };
    }
    match (&profile.last_clean, &profile.first_issue) {
        (Some(clean), None) if clean.step >= profile.planned_steps => format!(
            "at least {} torrents / {} peers; configured limit reached without benchmark issues",
            clean.torrents, clean.peers
        ),
        (Some(clean), None) => format!(
            "at least {} torrents / {} peers; run ended before a failing step",
            clean.torrents, clean.peers
        ),
        (Some(clean), Some(issue)) => format!(
            "clean through {} torrents / {} peers; first issue at {} torrents / {} peers",
            clean.torrents, clean.peers, issue.torrents, issue.peers
        ),
        (None, Some(issue)) => format!(
            "no clean capacity established; first issue at {} torrents / {} peers",
            issue.torrents, issue.peers
        ),
        (None, None) => "no benchmark steps ran".to_string(),
    }
}

fn step_was_interrupted(step: &BenchmarkStepSummary) -> bool {
    step.issues
        .iter()
        .any(|issue| issue == BENCHMARK_INTERRUPT_ISSUE)
}

fn likely_bottleneck(profile: &BenchmarkProfileSummary) -> String {
    let issue = match profile.first_issue.as_ref() {
        Some(issue) => issue,
        None => return "none detected within configured benchmark limits".to_string(),
    };
    if step_was_interrupted(issue) {
        return "interrupted by user".to_string();
    }
    let joined = issue.issues.join("; ");
    if joined.contains("sample_delay") {
        "scheduler or event-loop lag".to_string()
    } else if joined.contains("outbound_permit_timeout") {
        "peer connection permit pressure".to_string()
    } else if joined.contains("outbound_connect_timeout")
        || joined.contains("outbound_connection_refused")
        || issue.outbound_failed > 0
    {
        "socket/connect pressure".to_string()
    } else if joined.contains("peer_add_lag") || joined.contains("torrent_add_lag") {
        "orchestration could not add torrents or peers fast enough".to_string()
    } else if issue.protocol_errors > 0 || joined.contains("synthetic_leecher") {
        "protocol/session errors".to_string()
    } else if joined.contains("runtime_error") {
        "runtime/setup error".to_string()
    } else {
        format!("benchmark issue: {joined}")
    }
}

fn effective_peer_connection_limit(peers: usize, configured: Option<usize>) -> usize {
    configured.unwrap_or_else(|| peers.saturating_mul(2).saturating_add(128).max(256))
}

fn peer_connection_limit_policy(args: &SyntheticBenchmarkArgs) -> String {
    match args.peer_connection_permits {
        Some(limit) => format!("fixed {limit} peer connection permits"),
        None => "auto per step: max(256, peers * 2 + 128)".to_string(),
    }
}

fn os_limit_note() -> String {
    if cfg!(windows) {
        "Windows has no POSIX ulimit; benchmark reports harness peer permits and socket/connect failures instead".to_string()
    } else {
        "POSIX file-descriptor ulimit is not sampled by this harness; compare this report with `ulimit -n` when diagnosing socket ceilings".to_string()
    }
}

async fn remove_run_data_dir(output_dir: &Path) -> Result<bool, DynError> {
    let data_dir = output_dir.join("data");
    if tokio::fs::try_exists(&data_dir).await? {
        tokio::fs::remove_dir_all(&data_dir).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn print_benchmark_report(summary: &BenchmarkSummary, summary_path: &Path) {
    if summary.interrupted {
        print_interrupted_benchmark_report(summary, summary_path);
        return;
    }

    println!();
    println!("Benchmark Summary");
    println!("=================");
    println!(
        "Finished in {}. Ran {}/{} steps: {} passed, {} stopped.",
        summary.report.runtime,
        summary.report.steps_run,
        summary.report.planned_steps,
        summary.report.clean_steps,
        summary.report.issue_steps
    );
    println!(
        "Target: transport={} | format={} | up to {} torrents / {} peers | disk budget={} | torrent size={} | piece size={}",
        summary.transport,
        summary.torrent_format,
        summary.report.configured_max_torrents,
        summary.report.configured_max_peers,
        format_bytes(summary.report.disk_budget_bytes),
        format_bytes(summary.report.preferred_size_per_torrent_bytes),
        format_bytes(summary.report.piece_size_bytes)
    );
    if summary.report.retry_attempts > 0 || summary.report.recovered_after_retry_steps > 0 {
        println!(
            "Retries: {} attempts, {} recovered",
            summary.report.retry_attempts, summary.report.recovered_after_retry_steps
        );
    }
    println!("Details JSON: {}", summary_path.display());

    println!();
    println!("Results");
    println!("-------");
    for scenario in &summary.report.scenarios {
        print_benchmark_scenario_report(scenario);
    }
}

fn print_interrupted_benchmark_report(summary: &BenchmarkSummary, summary_path: &Path) {
    println!();
    println!("Benchmark Report (interrupted)");
    println!("==============================");
    println!(
        "Stopped by Ctrl+C after {}. Ran {}/{} steps: {} passed, {} stopped.",
        summary.report.runtime,
        summary.report.steps_run,
        summary.report.planned_steps,
        summary.report.clean_steps,
        summary.report.issue_steps
    );
    println!("Partial JSON: {}", summary_path.display());
    println!("Transport: {}", summary.transport);
    println!("Torrent format: {}", summary.torrent_format);
    println!();
    println!("Partial Results");
    println!("---------------");
    for scenario in &summary.report.scenarios {
        println!(
            "{}: {} | torrent capacity {} | peer capacity {} | down {} | up {} | reason {}",
            scenario.mode,
            human_benchmark_verdict(&scenario.verdict),
            human_benchmark_torrent_capacity(scenario),
            human_benchmark_peer_capacity(scenario),
            format_bps(scenario.peak_download_bps),
            format_bps(scenario.peak_upload_bps),
            scenario.first_issue.as_deref().unwrap_or("none")
        );
    }
}

fn print_benchmark_scenario_report(scenario: &BenchmarkScenarioReport) {
    println!(
        "{}: {}",
        scenario.mode,
        human_benchmark_verdict(&scenario.verdict)
    );
    println!("  Estimate");
    println!(
        "    Torrent capacity  {}",
        human_benchmark_torrent_capacity(scenario)
    );
    println!(
        "    Peer capacity     {}",
        human_benchmark_peer_capacity(scenario)
    );
    println!("  Peak speed");
    println!(
        "    Download          {}",
        format_bps(scenario.peak_download_bps),
    );
    println!(
        "    Upload            {}",
        format_bps(scenario.peak_upload_bps)
    );
    if let Some(issue) = &scenario.first_issue {
        println!("  First issue");
        println!(
            "    At                {}",
            human_benchmark_issue_at(scenario)
        );
        println!("    Reason            {}", truncate_issue(issue, 120));
        println!("    Cause             {}", scenario.likely_bottleneck);
    } else if scenario.max_sample_delay_ms > 0 {
        println!(
            "  Max sample lag      {}ms",
            format_count(scenario.max_sample_delay_ms)
        );
    }
    println!();
}

fn human_benchmark_verdict(verdict: &str) -> &'static str {
    match verdict {
        "clean_to_configured_limit" => "passed target",
        "clean_until_stopped" => "passed until stopped",
        "bounded_by_first_issue" => "found a limit",
        "failed_first_step" => "stopped early",
        "interrupted" => "interrupted",
        "no_steps" => "no steps ran",
        _ => "finished",
    }
}

fn human_benchmark_torrent_capacity(scenario: &BenchmarkScenarioReport) -> String {
    if scenario.clean_torrents > 0 {
        human_count(scenario.clean_torrents, "torrent", "torrents")
    } else if let Some(torrents) = scenario.first_issue_torrents {
        format!(
            "unknown (first issue at {})",
            human_count(torrents, "torrent", "torrents")
        )
    } else {
        "unknown; no completed step".to_string()
    }
}

fn human_benchmark_peer_capacity(scenario: &BenchmarkScenarioReport) -> String {
    if scenario.clean_peers > 0 {
        human_count(scenario.clean_peers, "peer", "peers")
    } else if let Some(peers) = scenario.first_issue_peers {
        format!(
            "unknown (first issue at {})",
            human_count(peers, "peer", "peers")
        )
    } else {
        "unknown; no completed step".to_string()
    }
}

fn human_benchmark_issue_at(scenario: &BenchmarkScenarioReport) -> String {
    format!(
        "{} / {}",
        human_optional_count(scenario.first_issue_torrents, "torrent", "torrents"),
        human_optional_count(scenario.first_issue_peers, "peer", "peers")
    )
}

fn print_benchmark_step_result(step: &BenchmarkStepSummary) {
    let status = if step.issues.is_empty() {
        "ok"
    } else if step.will_retry {
        "retry"
    } else {
        "stop"
    };
    println!(
        "  -> step {}/{} {}{}: {} | down {} | up {} | lag {}ms | wall {}",
        step.step,
        step.planned_steps,
        status,
        benchmark_attempt_suffix(step),
        benchmark_step_topology(step),
        format_bps(step.avg_download_bps),
        format_bps(step.avg_upload_bps),
        format_count(step.max_sample_delay_ms),
        format_duration_secs(step.wall_secs),
    );
    println!("     eta: {}", benchmark_eta_summary(step));
    if !step.issues.is_empty() {
        println!("     reason: {}", compact_issue_list(&step.issues));
        if step.will_retry {
            println!(
                "     retrying in {}",
                format_duration_secs(step.retry_delay_ms as f64 / 1000.0)
            );
        }
    }
}

fn benchmark_eta_summary(step: &BenchmarkStepSummary) -> String {
    let mode_steps = step.eta.current_scenario_remaining_steps;
    let full_steps = step.eta.full_benchmark_remaining_steps;
    if mode_steps == 0 && full_steps == 0 {
        return "done".to_string();
    }

    let mode_eta = if mode_steps == 0 {
        "this mode done".to_string()
    } else {
        format!(
            "this mode {} ({})",
            format_duration_secs(step.eta.current_scenario_eta_secs),
            format_step_count(mode_steps)
        )
    };
    let full_eta = if full_steps == 0 {
        "full run done".to_string()
    } else {
        format!(
            "full run {} ({})",
            format_duration_secs(step.eta.full_benchmark_eta_secs),
            format_step_count(full_steps)
        )
    };
    format!("{mode_eta}, {full_eta}")
}

fn format_step_count(steps: usize) -> String {
    if steps == 1 {
        "1 step".to_string()
    } else {
        format!("{steps} steps")
    }
}

fn benchmark_attempt_suffix(step: &BenchmarkStepSummary) -> String {
    if step.max_attempts > 1 {
        format!(" attempt {}/{}", step.attempt, step.max_attempts)
    } else {
        String::new()
    }
}

fn benchmark_step_topology(step: &BenchmarkStepSummary) -> String {
    format!(
        "torrents {} | peers {}",
        benchmark_progress_count(step.torrents_added, step.torrents),
        benchmark_progress_count(step.peers_added, step.requested_peers)
    )
}

fn benchmark_progress_count(added: usize, target: usize) -> String {
    if added == target {
        format_count(target)
    } else {
        format!("{}/{}", format_count(added), format_count(target))
    }
}

fn human_optional_count(count: Option<usize>, singular: &str, plural: &str) -> String {
    count
        .map(|count| human_count(count, singular, plural))
        .unwrap_or_else(|| "unknown".to_string())
}

fn human_count(count: usize, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{} {noun}", format_count(count))
}

fn format_count(count: impl std::fmt::Display) -> String {
    let digits = count.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted.chars().rev().collect()
}

fn compact_issue_list(issues: &[String]) -> String {
    let shown = issues
        .iter()
        .take(2)
        .map(|issue| truncate_issue(issue, 120))
        .collect::<Vec<_>>();
    if issues.len() > shown.len() {
        format!(
            "{} (+{} more)",
            shown.join("; "),
            issues.len() - shown.len()
        )
    } else {
        shown.join("; ")
    }
}

fn truncate_issue(issue: &str, max_chars: usize) -> String {
    let mut chars = issue.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn build_torrent_specs(
    torrents: usize,
    size_per_torrent: u64,
    piece_size: u64,
    torrent_format: SyntheticTorrentFormat,
) -> Result<Vec<SyntheticTorrentSpec>, DynError> {
    let v1_pieces = if synthetic_format_has_v1(torrent_format) {
        Some(build_synthetic_v1_piece_hashes(
            size_per_torrent,
            piece_size,
        )?)
    } else {
        None
    };
    let v2_material = if synthetic_format_has_v2(torrent_format) {
        Some(build_synthetic_v2_material(size_per_torrent, piece_size)?)
    } else {
        None
    };

    let mut specs = Vec::with_capacity(torrents);
    for index in 0..torrents {
        let name = format!("synthetic-torrent-{index:04}.bin");
        let piece_count = size_per_torrent.div_ceil(piece_size) as usize;
        let pieces = v1_pieces.clone().unwrap_or_default();
        let file_tree = v2_material
            .as_ref()
            .map(|material| build_synthetic_v2_file_tree(&name, size_per_torrent, &material.root));

        let info = Info {
            piece_length: piece_size as i64,
            pieces,
            private: None,
            files: Vec::new(),
            name: name.clone(),
            length: size_per_torrent as i64,
            md5sum: None,
            meta_version: synthetic_format_has_v2(torrent_format).then_some(2),
            file_tree,
        };
        let info_dict_bencode = serde_bencode::to_bytes(&info)?;
        let info_hash = synthetic_info_hash(torrent_format, &info_dict_bencode);
        let torrent = Torrent {
            info_dict_bencode,
            info,
            announce: None,
            announce_list: None,
            url_list: None,
            creation_date: Some(0),
            comment: None,
            created_by: Some("superseedr synthetic load harness".to_string()),
            encoding: None,
            piece_layers: v2_material.as_ref().map(build_synthetic_v2_piece_layers),
        };
        specs.push(SyntheticTorrentSpec {
            index,
            name,
            total_size: size_per_torrent,
            piece_size,
            piece_count,
            info_hash,
            torrent,
        });
    }
    Ok(specs)
}

struct SyntheticV2Material {
    root: Vec<u8>,
    piece_layer: Vec<u8>,
}

fn synthetic_format_has_v1(torrent_format: SyntheticTorrentFormat) -> bool {
    matches!(
        torrent_format,
        SyntheticTorrentFormat::V1 | SyntheticTorrentFormat::Hybrid
    )
}

fn synthetic_format_has_v2(torrent_format: SyntheticTorrentFormat) -> bool {
    matches!(
        torrent_format,
        SyntheticTorrentFormat::V2 | SyntheticTorrentFormat::Hybrid
    )
}

fn synthetic_info_hash(
    torrent_format: SyntheticTorrentFormat,
    info_dict_bencode: &[u8],
) -> Vec<u8> {
    if torrent_format == SyntheticTorrentFormat::V2 {
        Sha256::digest(info_dict_bencode)[0..20].to_vec()
    } else {
        Sha1::digest(info_dict_bencode).to_vec()
    }
}

fn build_synthetic_v1_piece_hashes(
    size_per_torrent: u64,
    piece_size: u64,
) -> Result<Vec<u8>, DynError> {
    let piece_count = size_per_torrent.div_ceil(piece_size) as usize;
    let mut pieces = Vec::with_capacity(piece_count * 20);
    for piece_index in 0..piece_count {
        let piece_start = piece_index as u64 * piece_size;
        let len = piece_size.min(size_per_torrent.saturating_sub(piece_start)) as usize;
        pieces.extend_from_slice(&Sha1::digest(vec![SYNTHETIC_BYTE; len]));
    }
    Ok(pieces)
}

fn build_synthetic_v2_material(
    size_per_torrent: u64,
    piece_size: u64,
) -> Result<SyntheticV2Material, DynError> {
    Ok(SyntheticV2Material {
        root: build_synthetic_v2_file_root(size_per_torrent)?,
        piece_layer: build_synthetic_v2_piece_layer(size_per_torrent, piece_size)?,
    })
}

fn build_synthetic_v2_piece_layer(
    size_per_torrent: u64,
    piece_size: u64,
) -> Result<Vec<u8>, DynError> {
    let piece_count = size_per_torrent.div_ceil(piece_size) as usize;
    let hashing_context_len = if size_per_torrent <= piece_size {
        usize::try_from(size_per_torrent)?
    } else {
        usize::try_from(piece_size)?
    };
    let mut layer = Vec::with_capacity(piece_count * 32);
    for piece_index in 0..piece_count {
        let piece_start = piece_index as u64 * piece_size;
        let len = piece_size.min(size_per_torrent.saturating_sub(piece_start)) as usize;
        let piece_data = vec![SYNTHETIC_BYTE; len];
        layer.extend_from_slice(&compute_v2_piece_root(&piece_data, hashing_context_len));
    }
    Ok(layer)
}

fn build_synthetic_v2_file_root(size_per_torrent: u64) -> Result<Vec<u8>, DynError> {
    let full_block = vec![SYNTHETIC_BYTE; BLOCK_SIZE as usize];
    let full_block_hash: [u8; 32] = Sha256::digest(&full_block).into();
    let full_blocks = size_per_torrent / BLOCK_SIZE as u64;
    let tail_len = (size_per_torrent % BLOCK_SIZE as u64) as usize;
    let leaf_count = size_per_torrent
        .div_ceil(BLOCK_SIZE as u64)
        .max(1)
        .next_power_of_two() as usize;
    let mut layer = Vec::with_capacity(leaf_count);

    for _ in 0..full_blocks {
        layer.push(full_block_hash);
    }
    if tail_len > 0 {
        let tail = vec![SYNTHETIC_BYTE; tail_len];
        layer.push(Sha256::digest(tail).into());
    }
    while layer.len() < leaf_count {
        layer.push([0u8; 32]);
    }

    while layer.len() > 1 {
        layer = layer
            .chunks(2)
            .map(|pair| {
                let mut hasher = Sha256::new();
                hasher.update(pair[0]);
                hasher.update(pair[1]);
                hasher.finalize().into()
            })
            .collect();
    }

    Ok(layer[0].to_vec())
}

fn build_synthetic_v2_file_tree(name: &str, length: u64, root: &[u8]) -> Value {
    let mut metadata = HashMap::new();
    metadata.insert("length".as_bytes().to_vec(), Value::Int(length as i64));
    metadata.insert(
        "pieces root".as_bytes().to_vec(),
        Value::Bytes(root.to_vec()),
    );

    let mut leaf = HashMap::new();
    leaf.insert(Vec::new(), Value::Dict(metadata));

    let mut tree = HashMap::new();
    tree.insert(name.as_bytes().to_vec(), Value::Dict(leaf));
    Value::Dict(tree)
}

fn build_synthetic_v2_piece_layers(material: &SyntheticV2Material) -> Value {
    let mut layers = HashMap::new();
    layers.insert(
        material.root.clone(),
        Value::Bytes(material.piece_layer.clone()),
    );
    Value::Dict(layers)
}

async fn prepare_seed_file(
    spec: &SyntheticTorrentSpec,
    torrent_dir: &Path,
) -> Result<(), DynError> {
    tokio::fs::create_dir_all(torrent_dir).await?;
    let path = torrent_dir.join(&spec.name);
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .await?;
    file.set_len(spec.total_size).await?;
    Ok(())
}

fn full_bitfield(piece_count: usize) -> Vec<u8> {
    let mut bitfield = vec![0u8; piece_count.div_ceil(8)];
    for piece_index in 0..piece_count {
        let byte_index = piece_index / 8;
        let bit_index = 7 - (piece_index % 8);
        bitfield[byte_index] |= 1 << bit_index;
    }
    bitfield
}

fn synthetic_peer_id(role: u8, index: usize) -> Vec<u8> {
    let mut id = [b'0'; 20];
    id[0] = role;
    let suffix = format!("{index:019}");
    id[1..].copy_from_slice(suffix.as_bytes());
    id.to_vec()
}

fn take_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buffer.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes(buffer[0..4].try_into().ok()?) as usize;
    if buffer.len() < 4 + len {
        return None;
    }
    Some(buffer.drain(0..4 + len).collect())
}

fn frame_message_id(frame: &[u8]) -> Option<u8> {
    if frame.len() <= 4 {
        None
    } else {
        Some(frame[4])
    }
}

fn parse_request_payload(frame: &[u8]) -> Option<(u32, u32, u32)> {
    if frame.len() != 17 || frame_message_id(frame) != Some(6) {
        return None;
    }
    let index = u32::from_be_bytes(frame[5..9].try_into().ok()?);
    let begin = u32::from_be_bytes(frame[9..13].try_into().ok()?);
    let length = u32::from_be_bytes(frame[13..17].try_into().ok()?);
    Some((index, begin, length))
}

fn parse_piece_payload_len(frame: &[u8]) -> Option<usize> {
    if frame.len() < 13 || frame_message_id(frame) != Some(7) {
        return None;
    }
    Some(frame.len() - 13)
}

async fn write_piece_frame<W>(
    writer: &mut W,
    piece: u32,
    begin: u32,
    data: &[u8],
) -> Result<(), DynError>
where
    W: AsyncWrite + Unpin,
{
    let len = (9 + data.len()) as u32;
    let mut header = [0u8; 13];
    header[0..4].copy_from_slice(&len.to_be_bytes());
    header[4] = 7;
    header[5..9].copy_from_slice(&piece.to_be_bytes());
    header[9..13].copy_from_slice(&begin.to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(data).await?;
    Ok(())
}

async fn write_request_frame<W>(
    writer: &mut W,
    piece: u32,
    begin: u32,
    length: u32,
) -> Result<(), DynError>
where
    W: AsyncWrite + Unpin,
{
    let mut frame = [0u8; 17];
    frame[0..4].copy_from_slice(&13u32.to_be_bytes());
    frame[4] = 6;
    frame[5..9].copy_from_slice(&piece.to_be_bytes());
    frame[9..13].copy_from_slice(&begin.to_be_bytes());
    frame[13..17].copy_from_slice(&length.to_be_bytes());
    writer.write_all(&frame).await?;
    Ok(())
}

fn block_request_for(total_size: u64, piece_size: u64, block_index: u64) -> (u32, u32, u32) {
    let global_offset = (block_index * BLOCK_SIZE as u64) % total_size;
    let piece = (global_offset / piece_size) as u32;
    let begin = (global_offset % piece_size) as u32;
    let remaining_piece = piece_size - begin as u64;
    let remaining_total = total_size - global_offset;
    let len = (BLOCK_SIZE as u64)
        .min(remaining_piece)
        .min(remaining_total)
        .max(1) as u32;
    (piece, begin, len)
}

struct ManagerTotals {
    download_bps: u64,
    upload_bps: u64,
    completed_pieces: u64,
    total_pieces: u64,
    connected_peers: u64,
}

fn manager_totals(managers: &[ManagerRuntime]) -> ManagerTotals {
    let mut totals = ManagerTotals {
        download_bps: 0,
        upload_bps: 0,
        completed_pieces: 0,
        total_pieces: 0,
        connected_peers: 0,
    };
    for manager in managers {
        let metrics = manager.metrics_rx.borrow();
        totals.download_bps = totals
            .download_bps
            .saturating_add(metrics.download_speed_bps);
        totals.upload_bps = totals.upload_bps.saturating_add(metrics.upload_speed_bps);
        totals.completed_pieces = totals
            .completed_pieces
            .saturating_add(metrics.number_of_pieces_completed as u64);
        totals.total_pieces = totals
            .total_pieces
            .saturating_add(metrics.number_of_pieces_total as u64);
        totals.connected_peers = totals
            .connected_peers
            .saturating_add(metrics.number_of_successfully_connected_peers as u64);
    }
    totals
}

fn outbound_connect_sample(counters: &SyntheticCounters) -> OutboundConnectSample {
    OutboundConnectSample {
        attempts: counters.outbound_connect_attempts.load(Ordering::Relaxed),
        established: counters
            .outbound_connect_established
            .load(Ordering::Relaxed),
        failed: counters.outbound_connect_failed.load(Ordering::Relaxed),
        by_transport: outbound_connect_transport_samples(counters),
        permit_timeout: counters.outbound_permit_timeout.load(Ordering::Relaxed),
        permit_manager_shutdown: counters
            .outbound_permit_manager_shutdown
            .load(Ordering::Relaxed),
        permit_queue_full: counters.outbound_permit_queue_full.load(Ordering::Relaxed),
        connect_timeout: counters.outbound_connect_timeout.load(Ordering::Relaxed),
        connection_refused: counters.outbound_connection_refused.load(Ordering::Relaxed),
        connection_reset: counters.outbound_connection_reset.load(Ordering::Relaxed),
        connection_aborted: counters.outbound_connection_aborted.load(Ordering::Relaxed),
        addr_in_use: counters.outbound_addr_in_use.load(Ordering::Relaxed),
        addr_not_available: counters.outbound_addr_not_available.load(Ordering::Relaxed),
        timed_out: counters.outbound_timed_out.load(Ordering::Relaxed),
        other_io: counters.outbound_other_io.load(Ordering::Relaxed),
        session_failed: counters.outbound_session_failed.load(Ordering::Relaxed),
    }
}

fn outbound_connect_transport_samples(
    counters: &SyntheticCounters,
) -> Vec<OutboundConnectTransportSample> {
    [
        (
            PeerTransportKind::Tcp,
            counters
                .outbound_connect_tcp_attempts
                .load(Ordering::Relaxed),
            counters
                .outbound_connect_tcp_established
                .load(Ordering::Relaxed),
            counters.outbound_connect_tcp_failed.load(Ordering::Relaxed),
        ),
        (
            PeerTransportKind::Utp,
            counters
                .outbound_connect_utp_attempts
                .load(Ordering::Relaxed),
            counters
                .outbound_connect_utp_established
                .load(Ordering::Relaxed),
            counters.outbound_connect_utp_failed.load(Ordering::Relaxed),
        ),
        (
            PeerTransportKind::Quic,
            counters
                .outbound_connect_quic_attempts
                .load(Ordering::Relaxed),
            counters
                .outbound_connect_quic_established
                .load(Ordering::Relaxed),
            counters
                .outbound_connect_quic_failed
                .load(Ordering::Relaxed),
        ),
    ]
    .into_iter()
    .filter(|(_, attempts, established, failed)| *attempts > 0 || *established > 0 || *failed > 0)
    .map(
        |(transport, attempts, established, failed)| OutboundConnectTransportSample {
            transport: transport.as_scheme(),
            attempts,
            established,
            failed,
        },
    )
    .collect()
}

fn increment_outbound_connect_attempt(counters: &SyntheticCounters, transport: PeerTransportKind) {
    match transport {
        PeerTransportKind::Tcp => &counters.outbound_connect_tcp_attempts,
        PeerTransportKind::Utp => &counters.outbound_connect_utp_attempts,
        PeerTransportKind::Quic => &counters.outbound_connect_quic_attempts,
        PeerTransportKind::WebRtc => return,
    }
    .fetch_add(1, Ordering::Relaxed);
}

fn increment_outbound_connect_established(
    counters: &SyntheticCounters,
    transport: PeerTransportKind,
) {
    match transport {
        PeerTransportKind::Tcp => &counters.outbound_connect_tcp_established,
        PeerTransportKind::Utp => &counters.outbound_connect_utp_established,
        PeerTransportKind::Quic => &counters.outbound_connect_quic_established,
        PeerTransportKind::WebRtc => return,
    }
    .fetch_add(1, Ordering::Relaxed);
}

fn increment_outbound_connect_failed(counters: &SyntheticCounters, transport: PeerTransportKind) {
    match transport {
        PeerTransportKind::Tcp => &counters.outbound_connect_tcp_failed,
        PeerTransportKind::Utp => &counters.outbound_connect_utp_failed,
        PeerTransportKind::Quic => &counters.outbound_connect_quic_failed,
        PeerTransportKind::WebRtc => return,
    }
    .fetch_add(1, Ordering::Relaxed);
}

fn protocol_error_sample(counters: &SyntheticCounters) -> ProtocolErrorSample {
    ProtocolErrorSample {
        synthetic_seeder: counters.synthetic_seeder_errors.load(Ordering::Relaxed),
        incoming_hub_handshake: counters
            .incoming_hub_handshake_errors
            .load(Ordering::Relaxed),
        incoming_hub_route_miss: counters.incoming_hub_route_misses.load(Ordering::Relaxed),
        incoming_hub_route_send: counters
            .incoming_hub_route_send_errors
            .load(Ordering::Relaxed),
        synthetic_leecher: counters.synthetic_leecher_errors.load(Ordering::Relaxed),
        synthetic_leecher_addr_in_use: counters
            .synthetic_leecher_addr_in_use
            .load(Ordering::Relaxed),
        synthetic_leecher_addr_not_available: counters
            .synthetic_leecher_addr_not_available
            .load(Ordering::Relaxed),
        synthetic_leecher_connection_refused: counters
            .synthetic_leecher_connection_refused
            .load(Ordering::Relaxed),
        synthetic_leecher_timed_out: counters.synthetic_leecher_timed_out.load(Ordering::Relaxed),
        synthetic_leecher_other_io: counters.synthetic_leecher_other_io.load(Ordering::Relaxed),
        synthetic_leecher_non_io: counters.synthetic_leecher_non_io.load(Ordering::Relaxed),
    }
}

fn resource_samples(snapshot: ResourceManagerSnapshot) -> ResourceSampleSet {
    ResourceSampleSet {
        peer_connection: resource_sample(snapshot.resources.get(&ResourceType::PeerConnection)),
        disk_read: resource_sample(snapshot.resources.get(&ResourceType::DiskRead)),
        disk_write: resource_sample(snapshot.resources.get(&ResourceType::DiskWrite)),
    }
}

fn resource_sample(usage: Option<&ResourceUsage>) -> ResourceSample {
    usage
        .map(|usage| ResourceSample {
            limit: usage.limit,
            in_use: usage.in_use,
            queued: usage.queued,
            max_queue_size: usage.max_queue_size,
        })
        .unwrap_or_default()
}

fn build_resource_manager_limits(
    args: &SyntheticLoadArgs,
    topology: RunTopology,
) -> HashMap<ResourceType, (usize, usize)> {
    let active_peers = topology.download_peers + topology.upload_peers;
    let peer_limit = args
        .peer_connection_permits
        .unwrap_or_else(|| active_peers.saturating_mul(2).saturating_add(128).max(256));
    let mut limits = HashMap::new();
    limits.insert(ResourceType::Reserve, (0, 0));
    limits.insert(ResourceType::PeerConnection, (peer_limit, peer_limit * 2));
    limits.insert(
        ResourceType::DiskRead,
        (args.disk_read_permits, args.disk_read_permits * 4),
    );
    limits.insert(
        ResourceType::DiskWrite,
        (args.disk_write_permits, args.disk_write_permits * 4),
    );
    limits
}

fn parse_size(raw: &str) -> Result<u64, DynError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("size value must not be empty".into());
    }
    let split_at = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let number: f64 = trimmed[..split_at].parse()?;
    let unit = trimmed[split_at..].trim().to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0,
        "m" | "mb" => 1_000_000.0,
        "g" | "gb" => 1_000_000_000.0,
        "t" | "tb" => 1_000_000_000_000.0,
        "ki" | "kib" => 1024.0,
        "mi" | "mib" => 1024.0 * 1024.0,
        "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "ti" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(format!("unsupported size unit in '{raw}'").into()),
    };
    let bytes = number * multiplier;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("invalid size value '{raw}'").into());
    }
    Ok(bytes.round() as u64)
}

fn gbps_to_bytes_per_second(gbps: f64) -> f64 {
    if gbps <= 0.0 || !gbps.is_finite() {
        0.0
    } else {
        gbps * 1_000_000_000.0 / 8.0
    }
}

fn synthetic_target_rate_limit(target_gbps: Option<f64>) -> f64 {
    target_gbps
        .map(gbps_to_bytes_per_second)
        .unwrap_or(f64::INFINITY)
}

fn bytes_to_bits_per_second(bytes: u64, secs: f64) -> u64 {
    ((bytes as f64 * 8.0) / secs.max(0.001)) as u64
}

fn bytes_per_second(bytes: u64, secs: f64) -> u64 {
    (bytes as f64 / secs.max(0.001)) as u64
}

fn format_bps(bits_per_second: u64) -> String {
    let bps = bits_per_second as f64;
    if bps >= 1_000_000_000.0 {
        format!("{:.2}Gbps", bps / 1_000_000_000.0)
    } else if bps >= 1_000_000.0 {
        format!("{:.1}Mbps", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.1}Kbps", bps / 1_000.0)
    } else {
        format!("{}bps", bits_per_second)
    }
}

fn format_bytes(bytes: u64) -> String {
    let value = bytes as f64;
    if value >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2}GiB", value / (1024.0 * 1024.0 * 1024.0))
    } else if value >= 1024.0 * 1024.0 {
        format!("{:.2}MiB", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.2}KiB", value / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn format_duration_secs(secs: f64) -> String {
    if !secs.is_finite() {
        return "unknown".to_string();
    }
    let secs = secs.max(0.0);
    if secs > 0.0 && secs < 1.0 {
        return format!("{:.0}ms", secs * 1000.0);
    }
    let total_secs = secs.ceil() as u64;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn mode_name(mode: SyntheticLoadMode) -> &'static str {
    match mode {
        SyntheticLoadMode::Download => "download",
        SyntheticLoadMode::Upload => "upload",
        SyntheticLoadMode::Swarm => "swarm",
    }
}

fn add_mode_name(mode: SyntheticLoadAddMode) -> &'static str {
    match mode {
        SyntheticLoadAddMode::Upfront => "upfront",
        SyntheticLoadAddMode::Burst => "burst",
        SyntheticLoadAddMode::Staggered => "staggered",
    }
}

fn peer_indices_for_torrent(
    peers: usize,
    total_torrents: usize,
    torrent_index: usize,
) -> impl Iterator<Item = usize> {
    (torrent_index..peers).step_by(total_torrents)
}

fn build_resource_manager(
    args: &SyntheticLoadArgs,
    topology: RunTopology,
    shutdown_tx: broadcast::Sender<()>,
) -> (ResourceManager, ResourceManagerClient) {
    ResourceManager::new(build_resource_manager_limits(args, topology), shutdown_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staggered_add_plan_advances_by_burst_size() {
        let plan = AddPlan {
            mode: SyntheticLoadAddMode::Staggered,
            interval: Duration::from_millis(500),
            burst_size: 2,
        };

        assert_eq!(plan.target_added(Duration::ZERO, 5), 2);
        assert_eq!(plan.target_added(Duration::from_millis(499), 5), 2);
        assert_eq!(plan.target_added(Duration::from_millis(500), 5), 4);
        assert_eq!(plan.target_added(Duration::from_millis(1000), 5), 5);
    }

    #[test]
    fn peer_indices_partition_peers_across_torrents() {
        let partitions: Vec<Vec<usize>> = (0..3)
            .map(|torrent_index| peer_indices_for_torrent(8, 3, torrent_index).collect())
            .collect();

        assert_eq!(partitions, vec![vec![0, 3, 6], vec![1, 4, 7], vec![2, 5]]);
    }

    #[test]
    fn default_utp_chaos_does_not_set_shared_udp_env() {
        assert!(shared_udp_chaos_env_value(SyntheticUdpChaosArgs::default()).is_none());
    }

    #[test]
    fn omitted_synthetic_target_rate_is_unlimited() {
        assert!(synthetic_target_rate_limit(None).is_infinite());
        assert_eq!(synthetic_target_rate_limit(Some(0.0)), 0.0);
        assert_eq!(synthetic_target_rate_limit(Some(8.0)), 1_000_000_000.0);
    }

    #[test]
    fn utp_chaos_args_build_reproducible_env_spec() {
        let chaos = SyntheticUdpChaosArgs {
            utp_chaos_seed: 42,
            utp_chaos_loss_ppm: 1_000,
            utp_chaos_duplicate_ppm: 2_000,
            utp_chaos_corrupt_ppm: 3_000,
            utp_chaos_reorder_ppm: 4_000,
            utp_chaos_max_delay_ms: 50,
        };

        assert_eq!(
            shared_udp_chaos_env_value(chaos),
            Some(
                "seed=42,loss_ppm=1000,duplicate_ppm=2000,corrupt_ppm=3000,reorder_ppm=4000,max_delay_ms=50"
                    .to_string()
            )
        );
    }

    #[test]
    fn utp_chaos_validation_rejects_invalid_ppm() {
        let chaos = SyntheticUdpChaosArgs {
            utp_chaos_loss_ppm: 1_000_001,
            ..SyntheticUdpChaosArgs::default()
        };

        assert!(validate_udp_chaos_args(chaos).is_err());
    }

    #[test]
    fn shared_utp_seeder_hub_uses_unique_synthetic_peer_keys() {
        let hub = SyntheticSeederHub::SharedUtp { port: 34_567 };

        assert_eq!(hub.addr_for_peer(0).unwrap(), hub.addr_for_peer(1).unwrap());
        assert_ne!(hub.synthetic_peer_key(0), hub.synthetic_peer_key(1));
        assert!(hub.synthetic_peer_key(0).is_some());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn mixed_shared_utp_seeder_hub_keys_only_utp_peers() {
        let hub = SyntheticSeederHub::MixedSingleTcpSharedUtp {
            tcp_port: 23_456,
            utp_port: 34_567,
        };

        assert!(hub.synthetic_peer_key(0).is_none());
        assert_eq!(
            hub.synthetic_peer_key(1),
            Some("synthetic-utp-34567:1".to_string())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mixed_macos_shared_utp_seeder_hub_keys_only_utp_peers() {
        let hub = SyntheticSeederHub::MixedPeerTcpSharedUtp {
            tcp_ports: Arc::<[u16]>::from(vec![23_456, 23_457]),
            utp_port: 34_567,
        };

        assert!(hub.synthetic_peer_key(0).is_none());
        assert_eq!(
            hub.synthetic_peer_key(1),
            Some("synthetic-utp-34567:1".to_string())
        );
    }

    #[test]
    fn expected_active_peers_tracks_staggered_torrent_and_peer_plans() {
        let add_plan = AddPlan {
            mode: SyntheticLoadAddMode::Staggered,
            interval: Duration::from_millis(500),
            burst_size: 2,
        };
        let peer_plan = AddPlan {
            mode: SyntheticLoadAddMode::Staggered,
            interval: Duration::from_millis(250),
            burst_size: 1,
        };
        let topology = RunTopology {
            download_peers: 6,
            upload_peers: 0,
        };

        assert_eq!(
            expected_active_peers(add_plan, peer_plan, topology, 3, Duration::ZERO),
            2
        );
        assert_eq!(
            expected_active_peers(add_plan, peer_plan, topology, 3, Duration::from_millis(250)),
            4
        );
        assert_eq!(
            expected_active_peers(add_plan, peer_plan, topology, 3, Duration::from_millis(500)),
            5
        );
    }

    #[test]
    fn expected_connection_close_filters_transport_teardown() {
        let closed: DynError = Box::new(std::io::Error::new(ErrorKind::BrokenPipe, "closed"));
        assert!(is_expected_connection_close(closed.as_ref()));

        let reset: DynError = Box::new(std::io::Error::new(ErrorKind::ConnectionReset, "reset"));
        assert!(is_expected_connection_close(reset.as_ref()));

        let malformed: DynError =
            Box::new(std::io::Error::new(ErrorKind::InvalidData, "bad frame"));
        assert!(!is_expected_connection_close(malformed.as_ref()));

        let semantic_error: DynError = "mismatched synthetic info hash".into();
        assert!(!is_expected_connection_close(semantic_error.as_ref()));
    }

    #[test]
    fn build_torrent_specs_generates_v1_metainfo_by_default() {
        let spec = build_torrent_specs(1, 32 * 1024, 16 * 1024, SyntheticTorrentFormat::V1)
            .expect("build v1 synthetic spec")
            .pop()
            .expect("one spec");

        assert_eq!(spec.piece_count, 2);
        assert_eq!(spec.torrent.info.pieces.len(), 40);
        assert_eq!(spec.torrent.info.meta_version, None);
        assert!(spec.torrent.info.file_tree.is_none());
        assert!(spec.torrent.piece_layers.is_none());
        assert_eq!(
            spec.info_hash,
            Sha1::digest(&spec.torrent.info_dict_bencode).to_vec()
        );
    }

    #[test]
    fn build_torrent_specs_generates_pure_v2_metainfo() {
        let spec = build_torrent_specs(1, 32 * 1024, 16 * 1024, SyntheticTorrentFormat::V2)
            .expect("build v2 synthetic spec")
            .pop()
            .expect("one spec");
        let roots = spec.torrent.get_v2_roots();
        let root = roots.first().expect("v2 file root").2.clone();
        let layer = spec
            .torrent
            .get_layer_hashes(&root)
            .expect("v2 piece layer");

        assert_eq!(spec.piece_count, 2);
        assert!(spec.torrent.info.pieces.is_empty());
        assert_eq!(spec.torrent.info.meta_version, Some(2));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].0, spec.name);
        assert_eq!(roots[0].1, 32 * 1024);
        assert_eq!(layer.len(), 64);
        assert_eq!(
            spec.info_hash,
            Sha256::digest(&spec.torrent.info_dict_bencode)[0..20].to_vec()
        );

        let piece_hash = spec
            .torrent
            .get_v2_hash_layer(1, 0, 32 * 1024, 1, &root)
            .expect("piece hash from layer");
        assert_eq!(
            piece_hash,
            compute_v2_piece_root(&vec![SYNTHETIC_BYTE; 16 * 1024], 16 * 1024).to_vec()
        );
    }

    #[test]
    fn build_torrent_specs_generates_hybrid_metainfo() {
        let spec = build_torrent_specs(1, 32 * 1024, 16 * 1024, SyntheticTorrentFormat::Hybrid)
            .expect("build hybrid synthetic spec")
            .pop()
            .expect("one spec");

        assert_eq!(spec.piece_count, 2);
        assert_eq!(spec.torrent.info.pieces.len(), 40);
        assert_eq!(spec.torrent.info.meta_version, Some(2));
        assert!(spec.torrent.info.file_tree.is_some());
        assert!(spec.torrent.piece_layers.is_some());
        assert_eq!(
            spec.info_hash,
            Sha1::digest(&spec.torrent.info_dict_bencode).to_vec()
        );
    }

    #[test]
    fn outbound_connect_sample_tracks_transport_breakdown() {
        let counters = SyntheticCounters::default();
        counters
            .outbound_connect_attempts
            .fetch_add(2, Ordering::Relaxed);
        counters
            .outbound_connect_established
            .fetch_add(1, Ordering::Relaxed);
        counters
            .outbound_connect_failed
            .fetch_add(1, Ordering::Relaxed);
        increment_outbound_connect_attempt(&counters, PeerTransportKind::Tcp);
        increment_outbound_connect_attempt(&counters, PeerTransportKind::Quic);
        increment_outbound_connect_established(&counters, PeerTransportKind::Tcp);
        increment_outbound_connect_failed(&counters, PeerTransportKind::Quic);

        let sample = outbound_connect_sample(&counters);

        assert_eq!(sample.attempts, 2);
        assert_eq!(sample.established, 1);
        assert_eq!(sample.failed, 1);
        assert_eq!(sample.by_transport.len(), 2);

        let tcp = sample
            .by_transport
            .iter()
            .find(|transport| transport.transport == "tcp")
            .expect("tcp transport sample");
        assert_eq!(tcp.attempts, 1);
        assert_eq!(tcp.established, 1);
        assert_eq!(tcp.failed, 0);

        let quic = sample
            .by_transport
            .iter()
            .find(|transport| transport.transport == "quic")
            .expect("quic transport sample");
        assert_eq!(quic.attempts, 1);
        assert_eq!(quic.established, 0);
        assert_eq!(quic.failed, 1);
    }

    fn benchmark_args() -> SyntheticBenchmarkArgs {
        SyntheticBenchmarkArgs {
            start_torrents: 10,
            start_peers: 10,
            max_torrents: 10,
            max_peers: 20,
            max_steps: 1,
            disk_budget: "20MiB".to_string(),
            size_per_torrent: "8MiB".to_string(),
            piece_size: "1MiB".to_string(),
            torrent_format: SyntheticTorrentFormat::V1,
            duration_secs: 1,
            warmup_secs: 0,
            metrics_interval_ms: 1000,
            leecher_pipeline: 1,
            target_gbps: 1.0,
            transport: SyntheticTransport::Tcp,
            utp_chaos: SyntheticUdpChaosArgs::default(),
            peer_add_interval_ms: 1000,
            peer_add_burst_size: 1,
            peer_connection_permits: None,
            disk_read_permits: 256,
            disk_write_permits: 256,
            max_sample_delay_ms: 5000,
            issue_retries: 2,
            retry_delay_ms: 1000,
            keep_output: false,
            out: PathBuf::from("tmp/synthetic-benchmark-test"),
        }
    }

    fn benchmark_scenario_report_stub(
        clean_torrents: usize,
        clean_peers: usize,
        first_issue_torrents: Option<usize>,
        first_issue_peers: Option<usize>,
    ) -> BenchmarkScenarioReport {
        BenchmarkScenarioReport {
            mode: "download".to_string(),
            verdict: "bounded_by_first_issue".to_string(),
            capacity_estimate: String::new(),
            clean_torrents,
            clean_peers,
            clean_disk_working_set_bytes: 0,
            clean_size_per_torrent_bytes: 0,
            first_issue_torrents,
            first_issue_peers,
            first_issue: None,
            likely_bottleneck: String::new(),
            runtime_secs: 0.0,
            steps_run: 0,
            retry_attempts: 0,
            transient_issue_attempts: 0,
            recovered_after_retry_steps: 0,
            planned_steps: 0,
            peak_download_bps: 0,
            peak_upload_bps: 0,
            observed_disk_read_bytes_per_sec: 0,
            observed_disk_write_bytes_per_sec: 0,
            disk_read_ops_per_sec: 0.0,
            disk_write_ops_per_sec: 0.0,
            max_sample_delay_ms: 0,
            protocol_errors: 0,
            outbound_failed: 0,
            outbound_permit_timeout: 0,
            peer_connection_limit: 0,
            disk_read_permits: 0,
            disk_write_permits: 0,
        }
    }

    #[test]
    fn benchmark_capacity_helpers_report_explicit_clean_capacity() {
        let report = benchmark_scenario_report_stub(1000, 2000, Some(1000), Some(4000));

        assert_eq!(human_benchmark_torrent_capacity(&report), "1,000 torrents");
        assert_eq!(human_benchmark_peer_capacity(&report), "2,000 peers");
        assert_eq!(
            human_benchmark_issue_at(&report),
            "1,000 torrents / 4,000 peers"
        );
    }

    #[test]
    fn benchmark_capacity_helpers_explain_missing_clean_step() {
        let report = benchmark_scenario_report_stub(0, 0, Some(10), Some(100));

        assert_eq!(
            human_benchmark_torrent_capacity(&report),
            "unknown (first issue at 10 torrents)"
        );
        assert_eq!(
            human_benchmark_peer_capacity(&report),
            "unknown (first issue at 100 peers)"
        );
    }

    #[test]
    fn benchmark_progress_count_formats_partial_counts_without_abbreviations() {
        assert_eq!(benchmark_progress_count(1000, 1000), "1,000");
        assert_eq!(benchmark_progress_count(400, 1000), "400/1,000");
    }

    #[test]
    fn benchmark_size_per_torrent_clamps_to_disk_budget() {
        let args = benchmark_args();
        let config = ParsedBenchmarkConfig::from_args(&args).unwrap();

        let download_size =
            benchmark_size_per_torrent(&config, SyntheticLoadMode::Download, 10).unwrap();
        let swarm_size = benchmark_size_per_torrent(&config, SyntheticLoadMode::Swarm, 10).unwrap();

        assert_eq!(download_size, 2 * 1024 * 1024);
        assert_eq!(swarm_size, 1024 * 1024);
        assert!(
            estimated_disk_bytes(SyntheticLoadMode::Download, 10, download_size)
                <= config.disk_budget
        );
        assert!(
            estimated_disk_bytes(SyntheticLoadMode::Swarm, 10, swarm_size) <= config.disk_budget
        );
    }

    #[test]
    fn benchmark_step_peers_enforces_swarm_peer_floor() {
        assert_eq!(
            benchmark_step_peers(SyntheticLoadMode::Swarm, 10, 3, 20).unwrap(),
            20
        );

        let error = benchmark_step_peers(SyntheticLoadMode::Swarm, 10, 3, 19)
            .unwrap_err()
            .to_string();
        assert!(error.contains("need at least 20 peers"));
    }

    #[test]
    fn next_benchmark_step_scales_torrents_before_peers() {
        assert_eq!(
            next_benchmark_step(SyntheticLoadMode::Download, 10, 10, 40, 100),
            (20, 20)
        );
        assert_eq!(
            next_benchmark_step(SyntheticLoadMode::Swarm, 10, 10, 40, 100),
            (20, 40)
        );
        assert_eq!(
            next_benchmark_step(SyntheticLoadMode::Download, 40, 10, 40, 100),
            (40, 20)
        );
    }
}
