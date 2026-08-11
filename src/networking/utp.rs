// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    future, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, LazyLock, Mutex as StdMutex, Weak,
    },
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{
        self as tokio_io, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf,
    },
    sync::{mpsc, watch},
    time::{self, Instant},
};

use crate::networking::runtime::NetworkLease;
use crate::networking::shared_udp::{
    family_for_addr, SharedUdpDatagram, SharedUdpFamily, SharedUdpHandle, SharedUdpKey,
    SharedUdpProtocol,
};
use crate::networking::transport::{PeerConnection, PeerConnectionDirection, PeerEndpoint};

const HEADER_LEN: usize = 20;
const UTP_VERSION: u8 = 1;
const TYPE_DATA: u8 = 0;
const TYPE_FIN: u8 = 1;
const TYPE_STATE: u8 = 2;
const TYPE_RESET: u8 = 3;
const TYPE_SYN: u8 = 4;
const EXT_NONE: u8 = 0;
const EXT_SELECTIVE_ACK: u8 = 1;

const MIN_PACKET_SIZE: usize = 150;
const MAX_PACKET_SIZE: usize = 2_560;
const NETWORK_MAX_PACKET_SIZE: usize = 1_200;
const STREAM_BUFFER: usize = 256 * 1024;
const RECEIVE_WINDOW: u32 = STREAM_BUFFER as u32;
const MAX_INFLIGHT_PACKETS: usize = 64;
const UTP_ACCEPT_QUEUE_CAPACITY: usize = 128;
const UTP_SESSION_QUEUE_CAPACITY: usize = 2_048;
const CONNECT_RETRIES: usize = 4;
const CONNECT_RETRY_TIMEOUT: Duration = Duration::from_millis(400);
const ENDPOINT_BIND_RETRY_ATTEMPTS: usize = 16;
const ENDPOINT_BIND_RETRY_DELAY: Duration = Duration::from_millis(1);
const INITIAL_RETRANSMIT_TIMEOUT: Duration = Duration::from_secs(1);
const MIN_RETRANSMIT_TIMEOUT: Duration = Duration::from_millis(500);
const DELAYED_ACK_DELAY: Duration = Duration::from_millis(5);
const DELAYED_ACK_PACKET_THRESHOLD: u8 = 4;
const MAX_RETRANSMITS: u8 = 8;
const DELAY_TARGET_MICROSECONDS: u32 = 100_000;
const BASE_DELAY_WINDOW: Duration = Duration::from_secs(120);
const BASE_DELAY_BUCKET: Duration = Duration::from_secs(1);
const MAX_CWND_INCREASE_BYTES_PER_RTT: f64 = 3_000.0;
const LOSS_WINDOW_FACTOR: f64 = 0.5;
const SACK_EXTENSION_BYTES: usize = 4;
const MAX_OUT_OF_ORDER_PACKETS: usize = 256;
const UTP_CONNECT_LOG_ENV: &str = "SUPERSEEDR_LOG_UTP_CONNECT";
const UTP_TUNING_ENV: &str = "SUPERSEEDR_UTP_TUNING";
const CWND_REDUCE_TIMER: Duration = Duration::from_millis(100);

/// Homegrown BEP 29/uTP transport.
///
/// This supports stream-like connections over the shared UDP runtime with packet
/// reliability, selective ACK handling, adaptive retransmit timeouts, and
/// LEDBAT-style delay-based congestion control.
pub struct UtpPeerTransport;

impl UtpPeerTransport {
    #[allow(dead_code)]
    pub async fn connect(
        network_lease: &NetworkLease,
        remote_addr: SocketAddr,
    ) -> io::Result<PeerConnection> {
        Self::connect_from_port(network_lease, remote_addr, 0).await
    }

    pub async fn connect_from_port(
        network_lease: &NetworkLease,
        remote_addr: SocketAddr,
        local_port: u16,
    ) -> io::Result<PeerConnection> {
        let endpoint = UtpEndpoint::bind(
            network_lease,
            SocketAddr::new(bind_ip_for(remote_addr), local_port),
        )
        .await?;

        let start = Instant::now();
        let receive_connection_id = random_connection_id();
        let send_connection_id = receive_connection_id.wrapping_add(1);
        let initial_seq_nr = 1;
        let (mut incoming_packets, session_guard) =
            endpoint.register_session(remote_addr, receive_connection_id)?;
        let max_packet_size = max_packet_size_for(remote_addr);
        let tuning = utp_tuning_config(max_packet_size);

        let syn = UtpPacket {
            packet_type: TYPE_SYN,
            connection_id: receive_connection_id,
            timestamp_microseconds: timestamp_microseconds(start),
            timestamp_difference_microseconds: 0,
            wnd_size: RECEIVE_WINDOW,
            seq_nr: initial_seq_nr,
            ack_nr: 0,
            selective_ack: Vec::new(),
            payload: Vec::new(),
        };
        let syn_bytes = syn.encode();

        let state = loop {
            endpoint.send_bytes(remote_addr, &syn_bytes).await?;

            match time::timeout(CONNECT_RETRY_TIMEOUT, incoming_packets.recv()).await {
                Ok(Some(packet)) => {
                    if packet.packet_type == TYPE_RESET {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionReset,
                            "uTP peer reset during connect",
                        ));
                    }
                    if packet.packet_type == TYPE_STATE
                        && packet.connection_id == receive_connection_id
                        && packet.ack_nr == initial_seq_nr
                    {
                        break UtpDriverState {
                            send_connection_id,
                            receive_connection_id,
                            next_send_seq_nr: initial_seq_nr.wrapping_add(1),
                            last_remote_seq_nr: packet.seq_nr.wrapping_sub(1),
                            reply_delay_microseconds: timestamp_microseconds(start)
                                .wrapping_sub(packet.timestamp_microseconds),
                            remote_window_bytes: packet.wnd_size as usize,
                            max_window_bytes: tuning.initial_window_bytes,
                            packet_size: max_packet_size,
                            max_packet_size,
                            rtt_microseconds: None,
                            rtt_var_microseconds: 0.0,
                            retransmit_timeout: INITIAL_RETRANSMIT_TIMEOUT,
                            consecutive_timeouts: 0,
                            last_ack_nr_seen: packet.ack_nr,
                            duplicate_ack_count: 0,
                            delay_history: DelayHistory::default(),
                            delay_sample_filter: DelaySampleFilter::default(),
                            tuning,
                            slow_start: tuning.slow_start,
                            slow_start_threshold_bytes: None,
                            loss_seq_nr: initial_seq_nr,
                            next_loss_reduction_at: None,
                            start,
                        };
                    }
                }
                Ok(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "uTP shared UDP session closed during connect",
                    ));
                }
                Err(_) => {}
            }

            if elapsed_connect_attempts(start) >= CONNECT_RETRIES {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "uTP connect timed out",
                ));
            }
        };

        let (client_stream, driver_stream) = tokio_io::duplex(STREAM_BUFFER);
        let local_receive_buffered_bytes = Arc::new(AtomicUsize::new(0));
        let io = UtpSessionIo {
            endpoint,
            remote_addr,
            incoming_packets,
            local_receive_buffered_bytes: local_receive_buffered_bytes.clone(),
            _session_guard: session_guard,
        };
        tokio::spawn(async move {
            if let Err(error) = run_utp_driver(io, driver_stream, state).await {
                if utp_transport_log_enabled() {
                    tracing::info!(%remote_addr, %error, "uTP driver stopped");
                }
                tracing::debug!(%remote_addr, %error, "uTP driver stopped");
            }
        });

        let client_stream = TrackedUtpStream::new(client_stream, local_receive_buffered_bytes);
        Ok(PeerConnection::new(
            client_stream,
            PeerEndpoint::utp(remote_addr),
            remote_addr,
            PeerConnectionDirection::Outgoing,
        )
        .with_network_lease(network_lease))
    }

    pub async fn bind_listener(
        network_lease: &NetworkLease,
        port: u16,
    ) -> io::Result<UtpListenerSet> {
        UtpListenerSet::bind(network_lease, port).await
    }
}

#[derive(Clone)]
pub struct UtpListenerSet {
    ipv4: Option<UtpListener>,
    ipv6: Option<UtpListener>,
}

impl UtpListenerSet {
    async fn bind(network_lease: &NetworkLease, port: u16) -> io::Result<Self> {
        let ipv4 = match UtpListener::bind(
            network_lease,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        )
        .await
        {
            Ok(listener) => Some(listener),
            Err(error) => {
                tracing::warn!(%error, "IPv4 uTP listener bind failed");
                None
            }
        };
        let ipv6_port = match (port, ipv4.as_ref().and_then(UtpListener::local_port)) {
            (0, Some(bound_port)) => bound_port,
            _ => port,
        };

        let ipv6 = match UtpListener::bind(
            network_lease,
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), ipv6_port),
        )
        .await
        {
            Ok(listener) => Some(listener),
            Err(error) => {
                tracing::warn!(%error, "IPv6 uTP listener bind failed");
                None
            }
        };

        if ipv4.is_none() && ipv6.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "failed to bind IPv4 or IPv6 uTP listener",
            ));
        }

        Ok(Self { ipv4, ipv6 })
    }

    pub async fn accept(&self) -> io::Result<PeerConnection> {
        match (&self.ipv4, &self.ipv6) {
            (Some(ipv4), Some(ipv6)) => {
                tokio::select! {
                    res = ipv4.accept() => res,
                    res = ipv6.accept() => res,
                }
            }
            (Some(ipv4), None) => ipv4.accept().await,
            (None, Some(ipv6)) => ipv6.accept().await,
            (None, None) => Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no uTP listener is currently bound",
            )),
        }
    }

    pub fn local_port(&self) -> Option<u16> {
        self.ipv4
            .as_ref()
            .or(self.ipv6.as_ref())
            .and_then(UtpListener::local_port)
    }
}

#[derive(Clone)]
pub struct UtpListener {
    endpoint: UtpEndpoint,
    accept_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<PeerConnection>>>,
}

impl UtpListener {
    async fn bind(network_lease: &NetworkLease, bind_addr: SocketAddr) -> io::Result<Self> {
        let endpoint = UtpEndpoint::bind(network_lease, bind_addr).await?;
        let (accept_tx, accept_rx) = mpsc::channel(UTP_ACCEPT_QUEUE_CAPACITY);
        endpoint.set_accept_sender(accept_tx)?;
        Ok(Self {
            endpoint,
            accept_rx: Arc::new(tokio::sync::Mutex::new(accept_rx)),
        })
    }

    async fn accept(&self) -> io::Result<PeerConnection> {
        self.accept_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionAborted, "uTP listener closed"))
    }

    fn local_port(&self) -> Option<u16> {
        self.endpoint.local_addr().ok().map(|addr| addr.port())
    }
}

#[derive(Clone)]
struct UtpEndpoint {
    inner: Arc<UtpEndpointInner>,
}

struct UtpEndpointInner {
    udp: SharedUdpHandle,
    sessions: StdMutex<HashMap<UtpSessionKey, mpsc::Sender<UtpPacket>>>,
    inbound_syn_responses: StdMutex<HashMap<UtpSessionKey, UtpPacket>>,
    accept_tx: StdMutex<Option<mpsc::Sender<PeerConnection>>>,
    shutdown_tx: watch::Sender<bool>,
    task: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

struct UtpSessionIo {
    endpoint: UtpEndpoint,
    remote_addr: SocketAddr,
    incoming_packets: mpsc::Receiver<UtpPacket>,
    local_receive_buffered_bytes: Arc<AtomicUsize>,
    _session_guard: UtpSessionGuard,
}

struct TrackedUtpStream {
    inner: DuplexStream,
    local_receive_buffered_bytes: Arc<AtomicUsize>,
}

struct UtpSessionGuard {
    endpoint: Weak<UtpEndpointInner>,
    key: UtpSessionKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UtpSessionKey {
    remote_addr: SocketAddr,
    connection_id: u16,
}

static UTP_ENDPOINT_REGISTRY: LazyLock<StdMutex<HashMap<SharedUdpKey, Weak<UtpEndpointInner>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

impl TrackedUtpStream {
    fn new(inner: DuplexStream, local_receive_buffered_bytes: Arc<AtomicUsize>) -> Self {
        Self {
            inner,
            local_receive_buffered_bytes,
        }
    }
}

impl AsyncRead for TrackedUtpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled_before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let bytes_read = buf.filled().len().saturating_sub(filled_before);
            if bytes_read > 0 {
                saturating_atomic_sub(&self.local_receive_buffered_bytes, bytes_read);
            }
        }
        result
    }
}

impl AsyncWrite for TrackedUtpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl UtpEndpoint {
    async fn bind(network_lease: &NetworkLease, bind_addr: SocketAddr) -> io::Result<Self> {
        let family = family_for_addr(bind_addr);
        let requested_key = SharedUdpKey::new(network_lease.generation_id(), bind_addr, family);
        let mut attempt = 0usize;

        loop {
            if let Some(endpoint) = lookup_utp_endpoint(&requested_key) {
                return Ok(endpoint);
            }

            let udp = SharedUdpHandle::bind(network_lease, bind_addr, family).await?;
            let actual_key = udp.key();
            if let Some(endpoint) = lookup_utp_endpoint(&actual_key) {
                return Ok(endpoint);
            }

            let datagram_rx = match udp.subscribe(SharedUdpProtocol::Utp) {
                Ok(datagram_rx) => datagram_rx,
                Err(error)
                    if error.kind() == io::ErrorKind::AddrInUse
                        && requested_key.bind_addr().port() != 0
                        && attempt < ENDPOINT_BIND_RETRY_ATTEMPTS =>
                {
                    attempt += 1;
                    time::sleep(ENDPOINT_BIND_RETRY_DELAY).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let inner = Arc::new(UtpEndpointInner {
                udp,
                sessions: StdMutex::new(HashMap::new()),
                inbound_syn_responses: StdMutex::new(HashMap::new()),
                accept_tx: StdMutex::new(None),
                shutdown_tx,
                task: StdMutex::new(None),
            });
            let task = spawn_utp_demux_task(Arc::downgrade(&inner), datagram_rx, shutdown_rx);
            *inner.task.lock().expect("uTP endpoint task lock") = Some(task);

            if requested_key.bind_addr().port() != 0 {
                register_utp_endpoint(requested_key, &inner);
            }
            register_utp_endpoint(actual_key, &inner);

            return Ok(Self { inner });
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.udp.local_addr()
    }

    fn set_accept_sender(&self, accept_tx: mpsc::Sender<PeerConnection>) -> io::Result<()> {
        let mut guard = self.inner.accept_tx.lock().expect("uTP accept sender lock");
        if guard.as_ref().is_some_and(|sender| !sender.is_closed()) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "uTP listener already registered for endpoint",
            ));
        }
        *guard = Some(accept_tx);
        Ok(())
    }

    fn register_session(
        &self,
        remote_addr: SocketAddr,
        connection_id: u16,
    ) -> io::Result<(mpsc::Receiver<UtpPacket>, UtpSessionGuard)> {
        let key = UtpSessionKey {
            remote_addr,
            connection_id,
        };
        let (tx, rx) = mpsc::channel(UTP_SESSION_QUEUE_CAPACITY);
        let mut sessions = self.inner.sessions.lock().expect("uTP session map lock");
        if sessions.get(&key).is_some_and(|sender| !sender.is_closed()) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "uTP session already registered",
            ));
        }
        sessions.insert(key, tx);
        Ok((
            rx,
            UtpSessionGuard {
                endpoint: Arc::downgrade(&self.inner),
                key,
            },
        ))
    }

    async fn send_packet(&self, remote_addr: SocketAddr, packet: &UtpPacket) -> io::Result<usize> {
        self.send_bytes(remote_addr, &packet.encode()).await
    }

    async fn send_bytes(&self, remote_addr: SocketAddr, bytes: &[u8]) -> io::Result<usize> {
        self.inner.udp.send_to(bytes, remote_addr).await
    }
}

impl Drop for UtpEndpointInner {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl Drop for UtpSessionGuard {
    fn drop(&mut self) {
        let Some(endpoint) = self.endpoint.upgrade() else {
            return;
        };
        if let Ok(mut sessions) = endpoint.sessions.lock() {
            sessions.remove(&self.key);
        };
        if let Ok(mut inbound_syn_responses) = endpoint.inbound_syn_responses.lock() {
            inbound_syn_responses.remove(&self.key);
        };
    }
}

impl UtpSessionIo {
    async fn send(&self, packet: &UtpPacket) -> io::Result<usize> {
        self.endpoint.send_packet(self.remote_addr, packet).await
    }

    async fn send_payload_packet(
        &self,
        state: &UtpDriverState,
        packet_type: u8,
        seq_nr: u16,
        payload: &[u8],
        out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
    ) -> io::Result<usize> {
        let bytes = encode_utp_packet(UtpPacketView {
            packet_type,
            connection_id: state.send_connection_id,
            timestamp_microseconds: timestamp_microseconds(state.start),
            timestamp_difference_microseconds: timestamp_difference_microseconds(state),
            wnd_size: advertised_window(out_of_order_payloads, self.local_receive_buffered_bytes()),
            seq_nr,
            ack_nr: state.ack_nr(),
            selective_ack: &[],
            payload,
        });
        self.endpoint.send_bytes(self.remote_addr, &bytes).await
    }

    async fn recv(&mut self) -> io::Result<UtpPacket> {
        self.incoming_packets.recv().await.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "uTP shared UDP session closed",
            )
        })
    }

    fn local_receive_buffered_bytes(&self) -> usize {
        self.local_receive_buffered_bytes.load(Ordering::Relaxed)
    }
}

fn lookup_utp_endpoint(key: &SharedUdpKey) -> Option<UtpEndpoint> {
    let mut registry = UTP_ENDPOINT_REGISTRY
        .lock()
        .expect("uTP endpoint registry lock");
    match registry.get(key).and_then(Weak::upgrade) {
        Some(inner) => Some(UtpEndpoint { inner }),
        None => {
            registry.remove(key);
            None
        }
    }
}

fn register_utp_endpoint(key: SharedUdpKey, inner: &Arc<UtpEndpointInner>) {
    let mut registry = UTP_ENDPOINT_REGISTRY
        .lock()
        .expect("uTP endpoint registry lock");
    registry.insert(key, Arc::downgrade(inner));
}

fn spawn_utp_demux_task(
    endpoint: Weak<UtpEndpointInner>,
    mut datagram_rx: mpsc::Receiver<SharedUdpDatagram>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                datagram = datagram_rx.recv() => {
                    let Some(datagram) = datagram else {
                        break;
                    };
                    let Some(endpoint) = endpoint.upgrade() else {
                        break;
                    };
                    dispatch_utp_datagram(&endpoint, datagram).await;
                }
            }
        }

        if let Some(endpoint) = endpoint.upgrade() {
            endpoint
                .sessions
                .lock()
                .expect("uTP session map lock")
                .clear();
            endpoint
                .inbound_syn_responses
                .lock()
                .expect("uTP inbound SYN response map lock")
                .clear();
            *endpoint.accept_tx.lock().expect("uTP accept sender lock") = None;
        }
    })
}

async fn dispatch_utp_datagram(endpoint: &Arc<UtpEndpointInner>, datagram: SharedUdpDatagram) {
    let Ok(packet) = decode_packet(&datagram.payload) else {
        return;
    };
    if packet.packet_type == TYPE_SYN {
        handle_inbound_syn(endpoint, datagram.source, packet).await;
        return;
    }

    let key = UtpSessionKey {
        remote_addr: datagram.source,
        connection_id: packet.connection_id,
    };
    let sender = endpoint
        .sessions
        .lock()
        .expect("uTP session map lock")
        .get(&key)
        .cloned();
    if let Some(sender) = sender {
        if let Err(error) = sender.try_send(packet) {
            tracing::debug!(
                ?key,
                ?error,
                "dropping uTP packet because session queue is full or closed"
            );
        }
    }
}

async fn handle_inbound_syn(
    endpoint: &Arc<UtpEndpointInner>,
    remote_addr: SocketAddr,
    syn: UtpPacket,
) {
    let accept_tx = endpoint
        .accept_tx
        .lock()
        .expect("uTP accept sender lock")
        .as_ref()
        .filter(|sender| !sender.is_closed())
        .cloned();
    let Some(accept_tx) = accept_tx else {
        return;
    };

    let endpoint = UtpEndpoint {
        inner: endpoint.clone(),
    };
    let receive_connection_id = syn.connection_id.wrapping_add(1);
    let session_key = UtpSessionKey {
        remote_addr,
        connection_id: receive_connection_id,
    };
    let (incoming_packets, session_guard) =
        match endpoint.register_session(remote_addr, receive_connection_id) {
            Ok(registration) => registration,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                let response = endpoint
                    .inner
                    .inbound_syn_responses
                    .lock()
                    .expect("uTP inbound SYN response map lock")
                    .get(&session_key)
                    .cloned();
                if let Some(response) = response {
                    let _ = endpoint.send_packet(remote_addr, &response).await;
                }
                return;
            }
            Err(_) => return,
        };
    let accept_permit = match accept_tx.try_reserve() {
        Ok(permit) => permit,
        Err(error) => {
            tracing::debug!(
                %remote_addr,
                ?error,
                "dropping inbound uTP SYN because accept queue is full or closed"
            );
            send_reset_for_syn(&endpoint, remote_addr, &syn).await;
            return;
        }
    };

    let start = Instant::now();
    let server_seq_nr = random_connection_id();
    let max_packet_size = max_packet_size_for(remote_addr);
    let tuning = utp_tuning_config(max_packet_size);
    let mut state = UtpDriverState {
        send_connection_id: syn.connection_id,
        receive_connection_id,
        next_send_seq_nr: server_seq_nr,
        last_remote_seq_nr: syn.seq_nr,
        reply_delay_microseconds: timestamp_microseconds(start)
            .wrapping_sub(syn.timestamp_microseconds),
        remote_window_bytes: syn.wnd_size as usize,
        max_window_bytes: tuning.initial_window_bytes,
        packet_size: max_packet_size,
        max_packet_size,
        rtt_microseconds: None,
        rtt_var_microseconds: 0.0,
        retransmit_timeout: INITIAL_RETRANSMIT_TIMEOUT,
        consecutive_timeouts: 0,
        last_ack_nr_seen: syn.ack_nr,
        duplicate_ack_count: 0,
        delay_history: DelayHistory::default(),
        delay_sample_filter: DelaySampleFilter::default(),
        tuning,
        slow_start: tuning.slow_start,
        slow_start_threshold_bytes: None,
        loss_seq_nr: server_seq_nr.wrapping_sub(1),
        next_loss_reduction_at: None,
        start,
    };
    state.record_received_packet(&syn);

    let (app_stream, driver_stream) = tokio_io::duplex(STREAM_BUFFER);
    let local_receive_buffered_bytes = Arc::new(AtomicUsize::new(0));
    let io = UtpSessionIo {
        endpoint,
        remote_addr,
        incoming_packets,
        local_receive_buffered_bytes: local_receive_buffered_bytes.clone(),
        _session_guard: session_guard,
    };
    let state_packet = UtpPacket {
        packet_type: TYPE_STATE,
        connection_id: state.send_connection_id,
        timestamp_microseconds: timestamp_microseconds(state.start),
        timestamp_difference_microseconds: timestamp_difference_microseconds(&state),
        wnd_size: RECEIVE_WINDOW,
        seq_nr: server_seq_nr,
        ack_nr: state.last_remote_seq_nr,
        selective_ack: Vec::new(),
        payload: Vec::new(),
    };
    io.endpoint
        .inner
        .inbound_syn_responses
        .lock()
        .expect("uTP inbound SYN response map lock")
        .insert(session_key, state_packet.clone());
    if io.send(&state_packet).await.is_err() {
        return;
    }

    tokio::spawn(async move {
        if let Err(error) = run_utp_driver(io, driver_stream, state).await {
            if utp_transport_log_enabled() {
                tracing::info!(%remote_addr, %error, "inbound uTP driver stopped");
            }
            tracing::debug!(%remote_addr, %error, "inbound uTP driver stopped");
        }
    });

    let app_stream = TrackedUtpStream::new(app_stream, local_receive_buffered_bytes);
    let connection = PeerConnection::new(
        app_stream,
        PeerEndpoint::utp(remote_addr),
        remote_addr,
        PeerConnectionDirection::Incoming,
    );
    accept_permit.send(connection);
}

async fn send_reset_for_syn(endpoint: &UtpEndpoint, remote_addr: SocketAddr, syn: &UtpPacket) {
    let packet = UtpPacket {
        packet_type: TYPE_RESET,
        connection_id: syn.connection_id,
        timestamp_microseconds: 0,
        timestamp_difference_microseconds: 0,
        wnd_size: RECEIVE_WINDOW,
        seq_nr: random_connection_id(),
        ack_nr: syn.seq_nr,
        selective_ack: Vec::new(),
        payload: Vec::new(),
    };
    let _ = endpoint.send_packet(remote_addr, &packet).await;
}

fn bind_ip_for(remote_addr: SocketAddr) -> IpAddr {
    match family_for_addr(remote_addr) {
        SharedUdpFamily::Ipv4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SharedUdpFamily::Ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

fn max_packet_size_for(remote_addr: SocketAddr) -> usize {
    if remote_addr.ip().is_loopback() {
        MAX_PACKET_SIZE
    } else {
        NETWORK_MAX_PACKET_SIZE
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UtpPacket {
    packet_type: u8,
    connection_id: u16,
    timestamp_microseconds: u32,
    timestamp_difference_microseconds: u32,
    wnd_size: u32,
    seq_nr: u16,
    ack_nr: u16,
    selective_ack: Vec<u8>,
    payload: Vec<u8>,
}

impl UtpPacket {
    fn encode(&self) -> Vec<u8> {
        encode_utp_packet(UtpPacketView {
            packet_type: self.packet_type,
            connection_id: self.connection_id,
            timestamp_microseconds: self.timestamp_microseconds,
            timestamp_difference_microseconds: self.timestamp_difference_microseconds,
            wnd_size: self.wnd_size,
            seq_nr: self.seq_nr,
            ack_nr: self.ack_nr,
            selective_ack: &self.selective_ack,
            payload: &self.payload,
        })
    }

    #[cfg(test)]
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        decode_packet(bytes)
    }
}

fn decode_packet(bytes: &[u8]) -> io::Result<UtpPacket> {
    if bytes.len() < HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "uTP packet shorter than header",
        ));
    }

    let packet_type = bytes[0] >> 4;
    let version = bytes[0] & 0x0f;
    if version != UTP_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported uTP packet version",
        ));
    }
    if packet_type > TYPE_SYN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported uTP packet type",
        ));
    }
    let (payload_offset, selective_ack) = parse_extension_chain(bytes, bytes[1])?;

    Ok(UtpPacket {
        packet_type,
        connection_id: u16::from_be_bytes([bytes[2], bytes[3]]),
        timestamp_microseconds: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        timestamp_difference_microseconds: u32::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
        ]),
        wnd_size: u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        seq_nr: u16::from_be_bytes([bytes[16], bytes[17]]),
        ack_nr: u16::from_be_bytes([bytes[18], bytes[19]]),
        selective_ack,
        payload: bytes[payload_offset..].to_vec(),
    })
}

#[allow(dead_code)]
pub(crate) fn decode_packet_for_fuzzing(bytes: &[u8]) -> bool {
    decode_packet(bytes).is_ok()
}

#[allow(dead_code)]
pub(crate) fn roundtrip_packet_for_fuzzing(bytes: &[u8]) {
    let mut input = UtpFuzzInput::new(bytes);
    let selective_ack_len = usize::from(input.next_u8() % 4) * SACK_EXTENSION_BYTES;
    let mut selective_ack = input.take(selective_ack_len);
    selective_ack.resize(selective_ack_len, 0);
    let packet = UtpPacket {
        packet_type: input.next_u8() % (TYPE_SYN + 1),
        connection_id: input.next_u16(),
        timestamp_microseconds: input.next_u32(),
        timestamp_difference_microseconds: input.next_u32(),
        wnd_size: input.next_u32(),
        seq_nr: input.next_u16(),
        ack_nr: input.next_u16(),
        selective_ack,
        payload: input.take(input.remaining().min(MAX_PACKET_SIZE)),
    };

    let decoded = decode_packet(&packet.encode()).expect("structured uTP packet decodes");
    assert_eq!(decoded, packet);
}

#[allow(dead_code)]
struct UtpFuzzInput<'a> {
    bytes: &'a [u8],
    offset: usize,
}

#[allow(dead_code)]
impl<'a> UtpFuzzInput<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn next_u8(&mut self) -> u8 {
        let byte = self.bytes.get(self.offset).copied().unwrap_or_default();
        self.offset = self.offset.saturating_add(1);
        byte
    }

    fn next_u16(&mut self) -> u16 {
        u16::from_be_bytes([self.next_u8(), self.next_u8()])
    }

    fn next_u32(&mut self) -> u32 {
        u32::from_be_bytes([
            self.next_u8(),
            self.next_u8(),
            self.next_u8(),
            self.next_u8(),
        ])
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn take(&mut self, len: usize) -> Vec<u8> {
        let start = self.offset.min(self.bytes.len());
        let end = start.saturating_add(len).min(self.bytes.len());
        let data = self.bytes[start..end].to_vec();
        self.offset = end;
        data
    }
}

struct UtpPacketView<'a> {
    packet_type: u8,
    connection_id: u16,
    timestamp_microseconds: u32,
    timestamp_difference_microseconds: u32,
    wnd_size: u32,
    seq_nr: u16,
    ack_nr: u16,
    selective_ack: &'a [u8],
    payload: &'a [u8],
}

fn encode_utp_packet(packet: UtpPacketView<'_>) -> Vec<u8> {
    let extension_len = if packet.selective_ack.is_empty() {
        0
    } else {
        2 + packet.selective_ack.len()
    };
    let mut bytes = Vec::with_capacity(HEADER_LEN + extension_len + packet.payload.len());
    bytes.push((packet.packet_type << 4) | UTP_VERSION);
    bytes.push(if packet.selective_ack.is_empty() {
        EXT_NONE
    } else {
        EXT_SELECTIVE_ACK
    });
    bytes.extend_from_slice(&packet.connection_id.to_be_bytes());
    bytes.extend_from_slice(&packet.timestamp_microseconds.to_be_bytes());
    bytes.extend_from_slice(&packet.timestamp_difference_microseconds.to_be_bytes());
    bytes.extend_from_slice(&packet.wnd_size.to_be_bytes());
    bytes.extend_from_slice(&packet.seq_nr.to_be_bytes());
    bytes.extend_from_slice(&packet.ack_nr.to_be_bytes());
    if !packet.selective_ack.is_empty() {
        bytes.push(EXT_NONE);
        bytes.push(packet.selective_ack.len() as u8);
        bytes.extend_from_slice(packet.selective_ack);
    }
    bytes.extend_from_slice(packet.payload);
    bytes
}

fn parse_extension_chain(bytes: &[u8], first_extension: u8) -> io::Result<(usize, Vec<u8>)> {
    let mut extension = first_extension;
    let mut offset = HEADER_LEN;
    let mut selective_ack = Vec::new();

    while extension != EXT_NONE {
        if bytes.len() < offset + 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "uTP extension header truncated",
            ));
        }

        let next_extension = bytes[offset];
        let extension_len = bytes[offset + 1] as usize;
        offset += 2;

        if bytes.len() < offset + extension_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "uTP extension body truncated",
            ));
        }

        if extension == EXT_SELECTIVE_ACK {
            selective_ack = bytes[offset..offset + extension_len].to_vec();
        }

        offset += extension_len;
        extension = next_extension;
    }

    Ok((offset, selective_ack))
}

struct UtpDriverState {
    send_connection_id: u16,
    receive_connection_id: u16,
    next_send_seq_nr: u16,
    last_remote_seq_nr: u16,
    reply_delay_microseconds: u32,
    remote_window_bytes: usize,
    max_window_bytes: f64,
    packet_size: usize,
    max_packet_size: usize,
    rtt_microseconds: Option<f64>,
    rtt_var_microseconds: f64,
    retransmit_timeout: Duration,
    consecutive_timeouts: u8,
    last_ack_nr_seen: u16,
    duplicate_ack_count: u8,
    delay_history: DelayHistory,
    delay_sample_filter: DelaySampleFilter,
    tuning: UtpTuningConfig,
    slow_start: bool,
    slow_start_threshold_bytes: Option<f64>,
    loss_seq_nr: u16,
    next_loss_reduction_at: Option<Instant>,
    start: Instant,
}

#[derive(Clone)]
struct SentPacket {
    packet_type: u8,
    seq_nr: u16,
    payload: Vec<u8>,
    sent_at: Instant,
    retransmits: u8,
}

#[derive(Debug)]
struct AckedPacket {
    payload_len: usize,
    rtt_sample: Option<Duration>,
}

#[derive(Default)]
struct AckOutcome {
    acked_packets: Vec<AckedPacket>,
    fast_retransmit: Vec<u16>,
    advanced_ack: bool,
}

#[derive(Default)]
struct DelayedAckState {
    pending_packets: u8,
    deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessIncomingOutcome {
    Continue,
    RemoteEof,
}

struct IncomingPayloadState<'a> {
    out_of_order_payloads: &'a mut BTreeMap<u16, Vec<u8>>,
    delayed_ack: &'a mut DelayedAckState,
    remote_fin_seq_nr: &'a mut Option<u16>,
}

impl DelayedAckState {
    fn has_pending(&self) -> bool {
        self.pending_packets > 0
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn queue(&mut self) {
        self.pending_packets = self.pending_packets.saturating_add(1);
        self.deadline
            .get_or_insert_with(|| Instant::now() + DELAYED_ACK_DELAY);
    }

    fn should_flush(&self) -> bool {
        self.pending_packets >= DELAYED_ACK_PACKET_THRESHOLD
    }

    fn clear(&mut self) {
        self.pending_packets = 0;
        self.deadline = None;
    }
}

#[derive(Default)]
struct DelayHistory {
    buckets: VecDeque<DelayBucket>,
}

#[derive(Default)]
struct DelaySampleFilter {
    samples: [u32; 3],
    len: usize,
    next: usize,
}

struct DelayBucket {
    started_at: Instant,
    min_delay_microseconds: u32,
}

#[derive(Clone, Copy)]
enum UtpTuningMode {
    Production,
    Legacy,
}

#[derive(Clone, Copy)]
struct UtpTuningConfig {
    initial_window_bytes: f64,
    minimum_window_bytes: f64,
    slow_start: bool,
    saturated_only_ledbat: bool,
    delay_sample_filter: bool,
    use_in_flight_window_factor: bool,
    loss_reduce_interval: Option<Duration>,
}

static UTP_TUNING_MODE: LazyLock<UtpTuningMode> = LazyLock::new(UtpTuningMode::from_env);

impl DelaySampleFilter {
    fn record(&mut self, sample: u32) -> u32 {
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % self.samples.len();
        self.len = (self.len + 1).min(self.samples.len());
        self.samples[..self.len]
            .iter()
            .copied()
            .min()
            .unwrap_or(sample)
    }
}

impl UtpTuningMode {
    fn from_env() -> Self {
        match std::env::var(UTP_TUNING_ENV) {
            Ok(value)
                if value.eq_ignore_ascii_case("legacy")
                    || value.eq_ignore_ascii_case("baseline") =>
            {
                Self::Legacy
            }
            _ => Self::Production,
        }
    }
}

impl UtpTuningConfig {
    const fn legacy(max_packet_size: usize) -> Self {
        Self {
            initial_window_bytes: max_packet_size as f64,
            minimum_window_bytes: 0.0,
            slow_start: false,
            saturated_only_ledbat: false,
            delay_sample_filter: false,
            use_in_flight_window_factor: false,
            loss_reduce_interval: None,
        }
    }

    const fn production(max_packet_size: usize) -> Self {
        Self {
            initial_window_bytes: max_packet_size as f64,
            minimum_window_bytes: max_packet_size as f64,
            slow_start: true,
            saturated_only_ledbat: true,
            delay_sample_filter: true,
            use_in_flight_window_factor: true,
            loss_reduce_interval: Some(CWND_REDUCE_TIMER),
        }
    }
}

fn utp_tuning_config(max_packet_size: usize) -> UtpTuningConfig {
    match *UTP_TUNING_MODE {
        UtpTuningMode::Production => UtpTuningConfig::production(max_packet_size),
        UtpTuningMode::Legacy => UtpTuningConfig::legacy(max_packet_size),
    }
}

impl UtpDriverState {
    fn ack_nr(&self) -> u16 {
        self.last_remote_seq_nr
    }

    fn accepts_remote_payload_sequence(&self, seq_nr: u16) -> bool {
        seq_nr == self.last_remote_seq_nr.wrapping_add(1)
    }

    fn record_remote_payload_sequence(&mut self, seq_nr: u16) {
        self.last_remote_seq_nr = seq_nr;
    }

    fn record_received_packet(&mut self, packet: &UtpPacket) {
        let now = timestamp_microseconds(self.start);
        self.reply_delay_microseconds = now.wrapping_sub(packet.timestamp_microseconds);
        self.remote_window_bytes = packet.wnd_size as usize;
    }

    fn send_window_bytes(&self) -> usize {
        let local_window = self.max_window_bytes.max(MIN_PACKET_SIZE as f64) as usize;
        local_window.min(self.remote_window_bytes)
    }

    async fn apply_ack_outcome(
        &mut self,
        io: &UtpSessionIo,
        unacked_packets: &mut VecDeque<SentPacket>,
        out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
        outcome: AckOutcome,
        packet: &UtpPacket,
        in_flight_before_ack: usize,
    ) -> io::Result<()> {
        let count_duplicate_acks = packet.packet_type == TYPE_STATE;
        if outcome.advanced_ack {
            self.duplicate_ack_count = 0;
            self.last_ack_nr_seen = packet.ack_nr;
        } else if count_duplicate_acks && packet.ack_nr == self.last_ack_nr_seen {
            self.duplicate_ack_count = self.duplicate_ack_count.saturating_add(1);
            if self.duplicate_ack_count >= 3 {
                let lost_seq_nr = packet.ack_nr.wrapping_add(1);
                if retransmit_packet(
                    io,
                    self,
                    unacked_packets,
                    out_of_order_payloads,
                    lost_seq_nr,
                )
                .await?
                {
                    self.on_packet_loss(lost_seq_nr);
                }
                self.duplicate_ack_count = 0;
            }
        } else {
            self.duplicate_ack_count = 0;
            self.last_ack_nr_seen = packet.ack_nr;
        }

        if !outcome.fast_retransmit.is_empty() {
            let mut first_retransmitted_seq_nr = None;
            for seq_nr in outcome.fast_retransmit {
                if retransmit_packet(io, self, unacked_packets, out_of_order_payloads, seq_nr)
                    .await?
                {
                    if self.tuning.loss_reduce_interval.is_some() {
                        self.on_packet_loss(seq_nr);
                    } else {
                        first_retransmitted_seq_nr.get_or_insert(seq_nr);
                    }
                }
            }
            if let Some(seq_nr) = first_retransmitted_seq_nr {
                self.on_packet_loss(seq_nr);
            }
        }

        let acked_payload_bytes: usize = outcome
            .acked_packets
            .iter()
            .map(|packet| packet.payload_len)
            .sum();
        for acked in &outcome.acked_packets {
            if let Some(sample) = acked.rtt_sample {
                self.update_rtt(sample);
            }
        }
        if acked_payload_bytes > 0 {
            self.update_congestion_window(
                packet.timestamp_difference_microseconds,
                acked_payload_bytes,
                in_flight_before_ack,
            );
        }

        Ok(())
    }

    fn update_rtt(&mut self, sample: Duration) {
        let sample_us = sample.as_micros() as f64;
        match self.rtt_microseconds {
            Some(rtt) => {
                let delta = rtt - sample_us;
                self.rtt_var_microseconds += (delta.abs() - self.rtt_var_microseconds) / 4.0;
                self.rtt_microseconds = Some(rtt + (sample_us - rtt) / 8.0);
            }
            None => {
                self.rtt_microseconds = Some(sample_us);
                self.rtt_var_microseconds = sample_us / 2.0;
            }
        }

        let rtt = self.rtt_microseconds.unwrap_or(sample_us);
        let timeout_us =
            (rtt + self.rtt_var_microseconds * 4.0).max(MIN_RETRANSMIT_TIMEOUT.as_micros() as f64);
        self.retransmit_timeout = Duration::from_micros(timeout_us as u64);
        self.consecutive_timeouts = 0;
    }

    fn update_congestion_window(
        &mut self,
        delay_sample_microseconds: u32,
        acked_payload_bytes: usize,
        in_flight_before_ack: usize,
    ) {
        let now = Instant::now();
        self.delay_history.record(now, delay_sample_microseconds);
        let Some(base_delay) = self.delay_history.base_delay() else {
            return;
        };

        let measured_delay = delay_sample_microseconds.saturating_sub(base_delay);
        let our_delay = if self.tuning.delay_sample_filter {
            self.delay_sample_filter.record(measured_delay)
        } else {
            measured_delay
        };

        if self.slow_start && our_delay >= DELAY_TARGET_MICROSECONDS {
            self.slow_start_threshold_bytes = Some(self.max_window_bytes * LOSS_WINDOW_FACTOR);
            self.slow_start = false;
        }

        let off_target = DELAY_TARGET_MICROSECONDS as f64 - our_delay as f64;
        let delay_factor = off_target / DELAY_TARGET_MICROSECONDS as f64;
        let window_factor = if self.tuning.use_in_flight_window_factor {
            acked_payload_bytes as f64 / in_flight_before_ack.max(acked_payload_bytes) as f64
        } else {
            acked_payload_bytes as f64 / self.max_window_bytes.max(acked_payload_bytes as f64)
        };
        let linear_gain = MAX_CWND_INCREASE_BYTES_PER_RTT * delay_factor * window_factor;
        let cwnd_saturated =
            in_flight_before_ack.saturating_add(self.packet_size) > self.max_window_bytes as usize;
        let scaled_gain = if self.tuning.saturated_only_ledbat && !cwnd_saturated {
            0.0
        } else if self.slow_start {
            let exponential_gain = acked_payload_bytes as f64;
            if self
                .slow_start_threshold_bytes
                .is_some_and(|threshold| self.max_window_bytes + exponential_gain > threshold)
            {
                self.slow_start = false;
                linear_gain
            } else {
                exponential_gain.max(linear_gain)
            }
        } else {
            linear_gain
        };
        self.max_window_bytes =
            (self.max_window_bytes + scaled_gain).max(self.tuning.minimum_window_bytes);

        self.packet_size = if our_delay > DELAY_TARGET_MICROSECONDS {
            MIN_PACKET_SIZE
        } else {
            self.max_packet_size
        };
    }

    fn on_packet_loss(&mut self, seq_nr: u16) {
        if let Some(interval) = self.tuning.loss_reduce_interval {
            let now = Instant::now();
            if seq_lte(seq_nr, self.loss_seq_nr)
                || self
                    .next_loss_reduction_at
                    .is_some_and(|deadline| now < deadline)
            {
                return;
            }
            self.next_loss_reduction_at = Some(now + interval);
            self.loss_seq_nr = self.next_send_seq_nr;
        }

        self.max_window_bytes =
            (self.max_window_bytes * LOSS_WINDOW_FACTOR).max(MIN_PACKET_SIZE as f64);
        self.max_window_bytes = self.max_window_bytes.max(self.tuning.minimum_window_bytes);
        if self.slow_start {
            self.slow_start_threshold_bytes = Some(self.max_window_bytes);
            self.slow_start = false;
        }
        self.packet_size = MIN_PACKET_SIZE;
    }
}

impl DelayHistory {
    fn record(&mut self, now: Instant, delay_microseconds: u32) {
        match self.buckets.back_mut() {
            Some(bucket) if now.duration_since(bucket.started_at) < BASE_DELAY_BUCKET => {
                bucket.min_delay_microseconds =
                    bucket.min_delay_microseconds.min(delay_microseconds);
            }
            _ => self.buckets.push_back(DelayBucket {
                started_at: now,
                min_delay_microseconds: delay_microseconds,
            }),
        }

        while self
            .buckets
            .front()
            .is_some_and(|bucket| now.duration_since(bucket.started_at) > BASE_DELAY_WINDOW)
        {
            self.buckets.pop_front();
        }
    }

    fn base_delay(&self) -> Option<u32> {
        self.buckets
            .iter()
            .map(|bucket| bucket.min_delay_microseconds)
            .min()
    }
}

async fn run_utp_driver(
    mut io: UtpSessionIo,
    local_stream: DuplexStream,
    mut state: UtpDriverState,
) -> io::Result<()> {
    let (mut local_reader, mut local_writer) = tokio_io::split(local_stream);
    let mut local_buf = vec![0_u8; MAX_PACKET_SIZE];
    let mut pending_payloads: VecDeque<Vec<u8>> = VecDeque::new();
    let mut unacked_packets: VecDeque<SentPacket> = VecDeque::new();
    let mut out_of_order_payloads: BTreeMap<u16, Vec<u8>> = BTreeMap::new();
    let mut delayed_ack = DelayedAckState::default();
    let mut local_eof = false;
    let mut local_fin_sent = false;
    let mut remote_fin_seq_nr = None;
    let mut remote_eof_delivered = false;
    loop {
        flush_pending_payloads(
            &io,
            &mut state,
            &mut pending_payloads,
            &mut unacked_packets,
            &out_of_order_payloads,
        )
        .await?;

        if local_eof && pending_payloads.is_empty() && unacked_packets.is_empty() {
            flush_delayed_ack(&io, &state, &out_of_order_payloads, &mut delayed_ack).await?;
            if local_fin_sent {
                return Ok(());
            }
            send_fin_packet(
                &io,
                &mut state,
                &mut unacked_packets,
                &out_of_order_payloads,
            )
            .await?;
            local_fin_sent = true;
        }

        let ack_deadline = delayed_ack.deadline();
        let retransmit_deadline = next_retransmit_deadline(&state, &unacked_packets);
        tokio::select! {
            read_result = local_reader.read(&mut local_buf), if !local_eof && !remote_eof_delivered && pending_payloads.len() < MAX_INFLIGHT_PACKETS => {
                let bytes_read = read_result?;
                if bytes_read == 0 {
                    local_eof = true;
                } else {
                    pending_payloads.push_back(local_buf[..bytes_read].to_vec());
                }
            }
            recv_result = io.recv() => {
                let packet = recv_result?;
                if process_incoming_packet(
                    &io,
                    &mut local_writer,
                    &mut state,
                    &mut unacked_packets,
                    IncomingPayloadState {
                        out_of_order_payloads: &mut out_of_order_payloads,
                        delayed_ack: &mut delayed_ack,
                        remote_fin_seq_nr: &mut remote_fin_seq_nr,
                    },
                    packet,
                ).await? == ProcessIncomingOutcome::RemoteEof {
                    remote_eof_delivered = true;
                    local_eof = true;
                }
            }
            _ = async move {
                if let Some(deadline) = retransmit_deadline {
                    time::sleep_until(deadline).await;
                } else {
                    future::pending::<()>().await;
                }
            }, if retransmit_deadline.is_some() => {
                retransmit_due_packets(&io, &mut state, &mut unacked_packets, &out_of_order_payloads).await?;
            }
            _ = async move {
                if let Some(deadline) = ack_deadline {
                    time::sleep_until(deadline).await;
                } else {
                    future::pending::<()>().await;
                }
            }, if delayed_ack.has_pending() => {
                flush_delayed_ack(&io, &state, &out_of_order_payloads, &mut delayed_ack).await?;
            }
        }
    }
}

fn next_retransmit_deadline(
    state: &UtpDriverState,
    unacked_packets: &VecDeque<SentPacket>,
) -> Option<Instant> {
    unacked_packets
        .iter()
        .map(|packet| packet.sent_at + state.retransmit_timeout)
        .min()
}

async fn flush_pending_payloads(
    io: &UtpSessionIo,
    state: &mut UtpDriverState,
    pending_payloads: &mut VecDeque<Vec<u8>>,
    unacked_packets: &mut VecDeque<SentPacket>,
    out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
) -> io::Result<()> {
    while pending_payloads.front().is_some() {
        if unacked_packets.len() >= MAX_INFLIGHT_PACKETS {
            break;
        }

        let current_window = unacked_payload_bytes(unacked_packets);
        let send_window = state.send_window_bytes();
        let next_payload_len = pending_payloads
            .front()
            .map(|payload| payload.len().min(state.packet_size))
            .unwrap_or(0);
        if current_window >= send_window
            || current_window.saturating_add(next_payload_len) > send_window
        {
            break;
        }

        let payload =
            pop_next_payload_chunk(pending_payloads, state.packet_size, state.max_packet_size);
        let seq_nr = state.next_send_seq_nr;
        state.next_send_seq_nr = state.next_send_seq_nr.wrapping_add(1);
        io.send_payload_packet(state, TYPE_DATA, seq_nr, &payload, out_of_order_payloads)
            .await?;
        unacked_packets.push_back(SentPacket {
            packet_type: TYPE_DATA,
            seq_nr,
            payload,
            sent_at: Instant::now(),
            retransmits: 0,
        });
    }

    Ok(())
}

async fn process_incoming_packet<W>(
    io: &UtpSessionIo,
    local_writer: &mut W,
    state: &mut UtpDriverState,
    unacked_packets: &mut VecDeque<SentPacket>,
    receive: IncomingPayloadState<'_>,
    packet: UtpPacket,
) -> io::Result<ProcessIncomingOutcome>
where
    W: AsyncWrite + Unpin,
{
    if packet.connection_id != state.receive_connection_id {
        return Ok(ProcessIncomingOutcome::Continue);
    }

    state.record_received_packet(&packet);
    let mut outcome = ProcessIncomingOutcome::Continue;

    match packet.packet_type {
        TYPE_STATE | TYPE_DATA | TYPE_FIN => {
            let in_flight_before_ack = unacked_payload_bytes(unacked_packets);
            let outcome = acknowledge_packets(
                unacked_packets,
                packet.ack_nr,
                &packet.selective_ack,
                state.next_send_seq_nr,
            );
            state
                .apply_ack_outcome(
                    io,
                    unacked_packets,
                    &*receive.out_of_order_payloads,
                    outcome,
                    &packet,
                    in_flight_before_ack,
                )
                .await?;
        }
        _ => {}
    }

    match packet.packet_type {
        TYPE_STATE => {}
        TYPE_DATA => {
            let expected_seq_nr = state.last_remote_seq_nr.wrapping_add(1);
            let immediate_ack = if state.accepts_remote_payload_sequence(packet.seq_nr) {
                if !packet.payload.is_empty() {
                    write_all_tracked(
                        local_writer,
                        &packet.payload,
                        &io.local_receive_buffered_bytes,
                    )
                    .await?;
                }
                state.record_remote_payload_sequence(packet.seq_nr);
                let should_ack_now = !receive.out_of_order_payloads.is_empty();
                if deliver_buffered_payloads(
                    local_writer,
                    state,
                    &mut *receive.out_of_order_payloads,
                    &*receive.remote_fin_seq_nr,
                    &io.local_receive_buffered_bytes,
                )
                .await?
                {
                    local_writer.shutdown().await?;
                    outcome = ProcessIncomingOutcome::RemoteEof;
                }
                should_ack_now
            } else if seq_gt(packet.seq_nr, expected_seq_nr)
                && receive.out_of_order_payloads.len() < MAX_OUT_OF_ORDER_PACKETS
            {
                receive
                    .out_of_order_payloads
                    .entry(packet.seq_nr)
                    .or_insert(packet.payload);
                true
            } else {
                true
            };
            queue_state_ack(
                io,
                state,
                &*receive.out_of_order_payloads,
                &mut *receive.delayed_ack,
                immediate_ack,
            )
            .await?;
        }
        TYPE_FIN => {
            if state.accepts_remote_payload_sequence(packet.seq_nr) {
                *receive.remote_fin_seq_nr = Some(packet.seq_nr);
                state.record_remote_payload_sequence(packet.seq_nr);
                local_writer.shutdown().await?;
                outcome = ProcessIncomingOutcome::RemoteEof;
            } else if seq_gt(packet.seq_nr, state.last_remote_seq_nr.wrapping_add(1))
                && receive.out_of_order_payloads.len() < MAX_OUT_OF_ORDER_PACKETS
            {
                *receive.remote_fin_seq_nr = Some(packet.seq_nr);
                receive
                    .out_of_order_payloads
                    .entry(packet.seq_nr)
                    .or_default();
            } else if (*receive.remote_fin_seq_nr)
                .is_some_and(|seq_nr| seq_lte(seq_nr, state.last_remote_seq_nr))
            {
                outcome = ProcessIncomingOutcome::RemoteEof;
            }
            queue_state_ack(
                io,
                state,
                &*receive.out_of_order_payloads,
                &mut *receive.delayed_ack,
                true,
            )
            .await?;
        }
        TYPE_RESET => {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "uTP peer reset stream",
            ));
        }
        TYPE_SYN => {}
        _ => {}
    }

    Ok(outcome)
}

async fn retransmit_due_packets(
    io: &UtpSessionIo,
    state: &mut UtpDriverState,
    unacked_packets: &mut VecDeque<SentPacket>,
    out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
) -> io::Result<()> {
    let now = Instant::now();
    let timeout = state.retransmit_timeout;
    let mut saw_timeout = false;
    let mut first_timed_out_seq = None;
    for sent in unacked_packets.iter_mut() {
        if now.duration_since(sent.sent_at) < timeout {
            continue;
        }
        if sent.retransmits >= MAX_RETRANSMITS {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "uTP retransmit budget exhausted",
            ));
        }

        saw_timeout = true;
        first_timed_out_seq.get_or_insert(sent.seq_nr);
        sent.sent_at = now;
        sent.retransmits = sent.retransmits.saturating_add(1);
        io.send_payload_packet(
            state,
            sent.packet_type,
            sent.seq_nr,
            &sent.payload,
            out_of_order_payloads,
        )
        .await?;
    }

    if saw_timeout {
        if let Some(seq_nr) = first_timed_out_seq {
            state.on_packet_loss(seq_nr);
        }
        state.consecutive_timeouts = state.consecutive_timeouts.saturating_add(1);
        state.retransmit_timeout = doubled_duration(state.retransmit_timeout);
    }

    Ok(())
}

async fn send_fin_packet(
    io: &UtpSessionIo,
    state: &mut UtpDriverState,
    unacked_packets: &mut VecDeque<SentPacket>,
    out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
) -> io::Result<()> {
    let seq_nr = state.next_send_seq_nr;
    state.next_send_seq_nr = state.next_send_seq_nr.wrapping_add(1);
    let packet = UtpPacket {
        packet_type: TYPE_FIN,
        connection_id: state.send_connection_id,
        timestamp_microseconds: timestamp_microseconds(state.start),
        timestamp_difference_microseconds: timestamp_difference_microseconds(state),
        wnd_size: advertised_window(out_of_order_payloads, io.local_receive_buffered_bytes()),
        seq_nr,
        ack_nr: state.ack_nr(),
        selective_ack: Vec::new(),
        payload: Vec::new(),
    };
    io.send(&packet).await?;
    unacked_packets.push_back(SentPacket {
        packet_type: TYPE_FIN,
        seq_nr,
        payload: Vec::new(),
        sent_at: Instant::now(),
        retransmits: 0,
    });
    Ok(())
}

async fn send_state_packet(
    io: &UtpSessionIo,
    state: &UtpDriverState,
    out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
) -> io::Result<()> {
    let ack_nr = state.ack_nr();
    let packet = UtpPacket {
        packet_type: TYPE_STATE,
        connection_id: state.send_connection_id,
        timestamp_microseconds: timestamp_microseconds(state.start),
        timestamp_difference_microseconds: timestamp_difference_microseconds(state),
        wnd_size: advertised_window(out_of_order_payloads, io.local_receive_buffered_bytes()),
        seq_nr: state.next_send_seq_nr,
        ack_nr,
        selective_ack: selective_ack_for(ack_nr, out_of_order_payloads),
        payload: Vec::new(),
    };
    io.send(&packet).await?;
    Ok(())
}

async fn queue_state_ack(
    io: &UtpSessionIo,
    state: &UtpDriverState,
    out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
    delayed_ack: &mut DelayedAckState,
    immediate: bool,
) -> io::Result<()> {
    delayed_ack.queue();
    if immediate || delayed_ack.should_flush() {
        flush_delayed_ack(io, state, out_of_order_payloads, delayed_ack).await?;
    }
    Ok(())
}

async fn flush_delayed_ack(
    io: &UtpSessionIo,
    state: &UtpDriverState,
    out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
    delayed_ack: &mut DelayedAckState,
) -> io::Result<()> {
    if !delayed_ack.has_pending() {
        return Ok(());
    }
    send_state_packet(io, state, out_of_order_payloads).await?;
    delayed_ack.clear();
    Ok(())
}

fn acknowledge_packets(
    unacked_packets: &mut VecDeque<SentPacket>,
    ack_nr: u16,
    selective_ack: &[u8],
    next_send_seq_nr: u16,
) -> AckOutcome {
    let last_sent_seq_nr = next_send_seq_nr.wrapping_sub(1);
    if !unacked_packets.is_empty() && seq_gt(ack_nr, last_sent_seq_nr) {
        return AckOutcome::default();
    }

    let now = Instant::now();
    let mut outcome = AckOutcome {
        advanced_ack: unacked_packets
            .iter()
            .any(|sent| seq_lte(sent.seq_nr, ack_nr)),
        ..AckOutcome::default()
    };
    let acked_sequences: HashSet<u16> = unacked_packets
        .iter()
        .filter(|sent| {
            seq_lte(sent.seq_nr, ack_nr)
                || selective_ack_contains(selective_ack, ack_nr, sent.seq_nr)
        })
        .map(|sent| sent.seq_nr)
        .collect();

    for sent in unacked_packets.iter() {
        if acked_sequences.contains(&sent.seq_nr) {
            continue;
        }

        let later_acked = unacked_packets
            .iter()
            .filter(|candidate| {
                seq_gt(candidate.seq_nr, sent.seq_nr) && acked_sequences.contains(&candidate.seq_nr)
            })
            .take(3)
            .count();
        if later_acked >= 3 {
            outcome.fast_retransmit.push(sent.seq_nr);
        }
    }

    let mut retained = VecDeque::with_capacity(unacked_packets.len());
    while let Some(sent) = unacked_packets.pop_front() {
        if acked_sequences.contains(&sent.seq_nr) {
            outcome.acked_packets.push(AckedPacket {
                payload_len: sent.payload.len(),
                rtt_sample: (sent.retransmits == 0).then(|| now.duration_since(sent.sent_at)),
            });
        } else {
            retained.push_back(sent);
        }
    }
    *unacked_packets = retained;

    outcome
}

fn pop_next_payload_chunk(
    pending_payloads: &mut VecDeque<Vec<u8>>,
    packet_size: usize,
    max_packet_size: usize,
) -> Vec<u8> {
    let packet_size = packet_size.clamp(MIN_PACKET_SIZE, max_packet_size);
    let front = pending_payloads
        .front_mut()
        .expect("front checked before chunk extraction");
    if front.len() <= packet_size {
        return pending_payloads.pop_front().expect("front checked above");
    }

    front.drain(..packet_size).collect()
}

async fn deliver_buffered_payloads<W>(
    local_writer: &mut W,
    state: &mut UtpDriverState,
    out_of_order_payloads: &mut BTreeMap<u16, Vec<u8>>,
    remote_fin_seq_nr: &Option<u16>,
    local_receive_buffered_bytes: &AtomicUsize,
) -> io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    let mut delivered_remote_fin = false;
    loop {
        let expected_seq_nr = state.last_remote_seq_nr.wrapping_add(1);
        let Some(payload) = out_of_order_payloads.remove(&expected_seq_nr) else {
            break;
        };
        if remote_fin_seq_nr.is_some_and(|seq_nr| seq_nr == expected_seq_nr) {
            state.record_remote_payload_sequence(expected_seq_nr);
            delivered_remote_fin = true;
            break;
        }
        if !payload.is_empty() {
            write_all_tracked(local_writer, &payload, local_receive_buffered_bytes).await?;
        }
        state.record_remote_payload_sequence(expected_seq_nr);
    }

    Ok(delivered_remote_fin)
}

async fn write_all_tracked<W>(
    local_writer: &mut W,
    mut payload: &[u8],
    local_receive_buffered_bytes: &AtomicUsize,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while !payload.is_empty() {
        let written = local_writer.write(payload).await?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write uTP payload to local stream",
            ));
        }
        local_receive_buffered_bytes.fetch_add(written, Ordering::Relaxed);
        payload = &payload[written..];
    }
    Ok(())
}

async fn retransmit_packet(
    io: &UtpSessionIo,
    state: &UtpDriverState,
    unacked_packets: &mut VecDeque<SentPacket>,
    out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
    seq_nr: u16,
) -> io::Result<bool> {
    let Some(sent) = unacked_packets
        .iter_mut()
        .find(|packet| packet.seq_nr == seq_nr)
    else {
        return Ok(false);
    };

    if sent.retransmits > 0 && sent.sent_at.elapsed() < state.retransmit_timeout {
        return Ok(false);
    }

    if sent.retransmits >= MAX_RETRANSMITS {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "uTP retransmit budget exhausted",
        ));
    }

    sent.sent_at = Instant::now();
    sent.retransmits = sent.retransmits.saturating_add(1);
    io.send_payload_packet(
        state,
        sent.packet_type,
        sent.seq_nr,
        &sent.payload,
        out_of_order_payloads,
    )
    .await?;
    Ok(true)
}

fn advertised_window(
    out_of_order_payloads: &BTreeMap<u16, Vec<u8>>,
    local_receive_buffered_bytes: usize,
) -> u32 {
    let buffered_bytes = out_of_order_payloads
        .values()
        .map(|payload| payload.len() as u32)
        .sum::<u32>()
        .saturating_add(local_receive_buffered_bytes.min(u32::MAX as usize) as u32);
    RECEIVE_WINDOW.saturating_sub(buffered_bytes)
}

fn saturating_atomic_sub(value: &AtomicUsize, amount: usize) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(amount))
    });
}

fn selective_ack_for(ack_nr: u16, out_of_order_payloads: &BTreeMap<u16, Vec<u8>>) -> Vec<u8> {
    if out_of_order_payloads.is_empty() {
        return Vec::new();
    }

    let mut mask = vec![0_u8; SACK_EXTENSION_BYTES];
    let mut any = false;
    for seq_nr in out_of_order_payloads.keys().copied() {
        let offset = seq_nr.wrapping_sub(ack_nr);
        if !(2..(2 + (SACK_EXTENSION_BYTES as u16 * 8))).contains(&offset) {
            continue;
        }
        let bit_index = (offset - 2) as usize;
        mask[bit_index / 8] |= 1 << (bit_index % 8);
        any = true;
    }

    if any {
        mask
    } else {
        Vec::new()
    }
}

fn selective_ack_contains(selective_ack: &[u8], ack_nr: u16, seq_nr: u16) -> bool {
    let offset = seq_nr.wrapping_sub(ack_nr);
    if !(2..0x8000).contains(&offset) {
        return false;
    }

    let bit_index = (offset - 2) as usize;
    selective_ack
        .get(bit_index / 8)
        .is_some_and(|byte| byte & (1 << (bit_index % 8)) != 0)
}

fn seq_lte(lhs: u16, rhs: u16) -> bool {
    lhs == rhs || rhs.wrapping_sub(lhs) < 0x8000
}

fn seq_gt(lhs: u16, rhs: u16) -> bool {
    lhs != rhs && lhs.wrapping_sub(rhs) < 0x8000
}

fn unacked_payload_bytes(unacked_packets: &VecDeque<SentPacket>) -> usize {
    unacked_packets
        .iter()
        .map(|packet| packet.payload.len())
        .sum()
}

fn timestamp_microseconds(start: Instant) -> u32 {
    start.elapsed().as_micros() as u32
}

fn timestamp_difference_microseconds(state: &UtpDriverState) -> u32 {
    state.reply_delay_microseconds
}

fn doubled_duration(duration: Duration) -> Duration {
    duration.checked_mul(2).unwrap_or(Duration::from_secs(60))
}

fn random_connection_id() -> u16 {
    loop {
        let id = rand::random::<u16>();
        if id != u16::MAX {
            return id;
        }
    }
}

fn elapsed_connect_attempts(start: Instant) -> usize {
    let elapsed = start.elapsed().as_millis();
    let retry_ms = CONNECT_RETRY_TIMEOUT.as_millis().max(1);
    (elapsed / retry_ms) as usize
}

fn utp_transport_log_enabled() -> bool {
    std::env::var(UTP_CONNECT_LOG_ENV)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UdpSocket;

    fn test_syn(connection_id: u16, seq_nr: u16) -> UtpPacket {
        UtpPacket {
            packet_type: TYPE_SYN,
            connection_id,
            timestamp_microseconds: 10,
            timestamp_difference_microseconds: 0,
            wnd_size: RECEIVE_WINDOW,
            seq_nr,
            ack_nr: 0,
            selective_ack: Vec::new(),
            payload: Vec::new(),
        }
    }

    fn test_reset(connection_id: u16, ack_nr: u16) -> UtpPacket {
        UtpPacket {
            packet_type: TYPE_RESET,
            connection_id,
            timestamp_microseconds: 20,
            timestamp_difference_microseconds: 0,
            wnd_size: RECEIVE_WINDOW,
            seq_nr: 1,
            ack_nr,
            selective_ack: Vec::new(),
            payload: Vec::new(),
        }
    }

    #[test]
    fn packet_round_trips() {
        let packet = UtpPacket {
            packet_type: TYPE_DATA,
            connection_id: 123,
            timestamp_microseconds: 456,
            timestamp_difference_microseconds: 789,
            wnd_size: 1_024,
            seq_nr: 65_530,
            ack_nr: 42,
            selective_ack: Vec::new(),
            payload: b"hello".to_vec(),
        };

        let decoded = UtpPacket::decode(&packet.encode()).unwrap();

        assert_eq!(decoded, packet);
    }

    #[test]
    fn decode_skips_extension_chain() {
        let packet = UtpPacket {
            packet_type: TYPE_DATA,
            connection_id: 123,
            timestamp_microseconds: 456,
            timestamp_difference_microseconds: 789,
            wnd_size: 1_024,
            seq_nr: 65_530,
            ack_nr: 42,
            selective_ack: Vec::new(),
            payload: b"hello".to_vec(),
        };
        let mut bytes = packet.encode();
        bytes[1] = 2;
        bytes.splice(HEADER_LEN..HEADER_LEN, [0, 3, 1, 2, 3]);

        let decoded = UtpPacket::decode(&bytes).unwrap();

        assert_eq!(decoded, packet);
    }

    #[test]
    fn selective_ack_extension_round_trips() {
        let packet = UtpPacket {
            packet_type: TYPE_STATE,
            connection_id: 123,
            timestamp_microseconds: 456,
            timestamp_difference_microseconds: 789,
            wnd_size: 1_024,
            seq_nr: 65_530,
            ack_nr: 42,
            selective_ack: vec![0b0000_0101, 0, 0, 0],
            payload: Vec::new(),
        };

        let bytes = packet.encode();
        assert_eq!(bytes[1], EXT_SELECTIVE_ACK);

        let decoded = UtpPacket::decode(&bytes).unwrap();

        assert_eq!(decoded, packet);
    }

    #[test]
    fn decode_accepts_libtorrent_short_selective_ack_extension() {
        let packet = UtpPacket {
            packet_type: TYPE_STATE,
            connection_id: 123,
            timestamp_microseconds: 456,
            timestamp_difference_microseconds: 789,
            wnd_size: 1_024,
            seq_nr: 65_530,
            ack_nr: 42,
            selective_ack: vec![0b0000_0101],
            payload: Vec::new(),
        };

        let bytes = packet.encode();
        assert_eq!(bytes[1], EXT_SELECTIVE_ACK);

        let decoded = UtpPacket::decode(&bytes).unwrap();

        assert_eq!(decoded, packet);
    }

    #[test]
    fn selective_ack_bit_mapping_starts_at_ack_plus_two() {
        let mut out_of_order = BTreeMap::new();
        out_of_order.insert(12, b"later".to_vec());
        out_of_order.insert(14, b"later2".to_vec());

        let mask = selective_ack_for(10, &out_of_order);

        assert_eq!(mask, vec![0b0000_0101, 0, 0, 0]);
        assert!(selective_ack_contains(&mask, 10, 12));
        assert!(!selective_ack_contains(&mask, 10, 13));
        assert!(selective_ack_contains(&mask, 10, 14));
    }

    #[test]
    fn advertised_window_subtracts_buffered_out_of_order_payloads() {
        let mut out_of_order = BTreeMap::new();
        out_of_order.insert(12, vec![0; 1_024]);
        out_of_order.insert(14, vec![0; 512]);

        assert_eq!(advertised_window(&out_of_order, 0), RECEIVE_WINDOW - 1_536);
        assert_eq!(
            advertised_window(&out_of_order, 2_048),
            RECEIVE_WINDOW - 3_584
        );
    }

    #[tokio::test]
    async fn tracked_stream_releases_advertised_window_when_read() {
        let (client_stream, mut driver_stream) = tokio_io::duplex(64);
        let buffered = Arc::new(AtomicUsize::new(0));
        let mut client_stream = TrackedUtpStream::new(client_stream, buffered.clone());

        write_all_tracked(&mut driver_stream, b"abcdef", &buffered)
            .await
            .unwrap();
        assert_eq!(buffered.load(Ordering::Relaxed), 6);

        let mut buf = [0_u8; 4];
        assert_eq!(client_stream.read(&mut buf).await.unwrap(), 4);
        assert_eq!(buffered.load(Ordering::Relaxed), 2);

        assert_eq!(client_stream.read(&mut buf).await.unwrap(), 2);
        assert_eq!(buffered.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn delayed_ack_state_flushes_at_threshold_and_clears() {
        let mut delayed_ack = DelayedAckState::default();
        assert!(!delayed_ack.has_pending());
        assert!(delayed_ack.deadline().is_none());

        for _ in 1..DELAYED_ACK_PACKET_THRESHOLD {
            delayed_ack.queue();
            assert!(delayed_ack.has_pending());
            assert!(delayed_ack.deadline().is_some());
            assert!(!delayed_ack.should_flush());
        }

        delayed_ack.queue();
        assert!(delayed_ack.should_flush());

        delayed_ack.clear();
        assert!(!delayed_ack.has_pending());
        assert!(delayed_ack.deadline().is_none());
    }

    #[test]
    fn ack_processing_honors_selective_ack() {
        let mut unacked = VecDeque::from([
            SentPacket {
                packet_type: TYPE_DATA,
                seq_nr: 10,
                payload: vec![1],
                sent_at: Instant::now(),
                retransmits: 0,
            },
            SentPacket {
                packet_type: TYPE_DATA,
                seq_nr: 11,
                payload: vec![2],
                sent_at: Instant::now(),
                retransmits: 0,
            },
            SentPacket {
                packet_type: TYPE_DATA,
                seq_nr: 12,
                payload: vec![3],
                sent_at: Instant::now(),
                retransmits: 0,
            },
        ]);

        let outcome = acknowledge_packets(&mut unacked, 10, &[0b0000_0001, 0, 0, 0], 13);

        assert_eq!(outcome.acked_packets.len(), 2);
        assert_eq!(unacked.len(), 1);
        assert_eq!(unacked.front().unwrap().seq_nr, 11);
    }

    #[test]
    fn ack_processing_ignores_stale_ack_numbers() {
        let mut unacked = VecDeque::from([
            SentPacket {
                packet_type: TYPE_DATA,
                seq_nr: 10,
                payload: vec![1],
                sent_at: Instant::now(),
                retransmits: 0,
            },
            SentPacket {
                packet_type: TYPE_DATA,
                seq_nr: 11,
                payload: vec![2],
                sent_at: Instant::now(),
                retransmits: 0,
            },
        ]);

        let outcome = acknowledge_packets(&mut unacked, 9, &[], 12);

        assert!(!outcome.advanced_ack);
        assert!(outcome.acked_packets.is_empty());
        assert_eq!(unacked.len(), 2);
        assert_eq!(unacked.front().unwrap().seq_nr, 10);
    }

    #[test]
    fn ack_processing_ignores_future_ack_numbers() {
        let mut unacked = VecDeque::from([
            SentPacket {
                packet_type: TYPE_DATA,
                seq_nr: 10,
                payload: vec![1],
                sent_at: Instant::now(),
                retransmits: 0,
            },
            SentPacket {
                packet_type: TYPE_DATA,
                seq_nr: 11,
                payload: vec![2],
                sent_at: Instant::now(),
                retransmits: 0,
            },
        ]);

        let outcome = acknowledge_packets(&mut unacked, 40, &[], 12);

        assert!(!outcome.advanced_ack);
        assert!(outcome.acked_packets.is_empty());
        assert_eq!(unacked.len(), 2);
        assert_eq!(unacked.front().unwrap().seq_nr, 10);
    }

    #[test]
    fn congestion_window_reacts_to_delay_and_loss() {
        let mut state = test_driver_state(UtpTuningConfig::legacy(MAX_PACKET_SIZE));

        state.update_congestion_window(10_000, MAX_PACKET_SIZE, MAX_PACKET_SIZE);
        let grown = state.max_window_bytes;
        state.update_congestion_window(250_000, MAX_PACKET_SIZE, MAX_PACKET_SIZE);
        assert!(state.max_window_bytes < grown);
        assert_eq!(state.packet_size, MIN_PACKET_SIZE);

        let before_loss = state.max_window_bytes;
        state.on_packet_loss(2);
        assert!(state.max_window_bytes <= before_loss);
        assert_eq!(state.packet_size, MIN_PACKET_SIZE);
    }

    #[test]
    fn production_loss_gating_reduces_window_once_per_interval() {
        let mut state = test_driver_state(UtpTuningConfig::production(MAX_PACKET_SIZE));
        state.max_window_bytes = (MAX_PACKET_SIZE * 8) as f64;
        state.next_send_seq_nr = 20;

        state.on_packet_loss(2);
        let after_first_loss = state.max_window_bytes;
        state.on_packet_loss(3);

        assert_eq!(state.max_window_bytes, after_first_loss);
        assert_eq!(state.packet_size, MIN_PACKET_SIZE);
    }

    #[test]
    fn production_window_does_not_drop_below_one_packet() {
        let mut state = test_driver_state(UtpTuningConfig::production(MAX_PACKET_SIZE));
        state.max_window_bytes = MAX_PACKET_SIZE as f64;

        state.on_packet_loss(2);

        assert_eq!(state.max_window_bytes, MAX_PACKET_SIZE as f64);
    }

    #[test]
    fn production_tuning_enables_full_congestion_controls() {
        let tuning = UtpTuningConfig::production(MAX_PACKET_SIZE);

        assert!(tuning.slow_start);
        assert!(tuning.saturated_only_ledbat);
        assert!(tuning.delay_sample_filter);
        assert!(tuning.use_in_flight_window_factor);
        assert_eq!(tuning.loss_reduce_interval, Some(CWND_REDUCE_TIMER));
    }

    #[test]
    fn delay_sample_filter_uses_lowest_recent_sample() {
        let mut filter = DelaySampleFilter::default();

        assert_eq!(filter.record(90_000), 90_000);
        assert_eq!(filter.record(250_000), 90_000);
        assert_eq!(filter.record(80_000), 80_000);
        assert_eq!(filter.record(120_000), 80_000);
        assert_eq!(filter.record(130_000), 80_000);
        assert_eq!(filter.record(140_000), 120_000);
    }

    #[test]
    fn sequence_comparison_wraps() {
        assert!(seq_lte(65_535, 0));
        assert!(seq_lte(0, 1));
        assert!(!seq_lte(10, 9));
    }

    #[test]
    fn packet_size_is_conservative_for_non_loopback_peers() {
        assert_eq!(
            max_packet_size_for(SocketAddr::from(([127, 0, 0, 1], 1))),
            MAX_PACKET_SIZE
        );
        assert_eq!(
            max_packet_size_for(SocketAddr::from(([203, 0, 113, 10], 1))),
            NETWORK_MAX_PACKET_SIZE
        );
    }

    fn test_driver_state(tuning: UtpTuningConfig) -> UtpDriverState {
        UtpDriverState {
            send_connection_id: 2,
            receive_connection_id: 1,
            next_send_seq_nr: 2,
            last_remote_seq_nr: 76,
            reply_delay_microseconds: 0,
            remote_window_bytes: RECEIVE_WINDOW as usize,
            max_window_bytes: tuning.initial_window_bytes,
            packet_size: MAX_PACKET_SIZE,
            max_packet_size: MAX_PACKET_SIZE,
            rtt_microseconds: None,
            rtt_var_microseconds: 0.0,
            retransmit_timeout: INITIAL_RETRANSMIT_TIMEOUT,
            consecutive_timeouts: 0,
            last_ack_nr_seen: 1,
            duplicate_ack_count: 0,
            delay_history: DelayHistory::default(),
            delay_sample_filter: DelaySampleFilter::default(),
            tuning,
            slow_start: tuning.slow_start,
            slow_start_threshold_bytes: None,
            loss_seq_nr: 1,
            next_loss_reduction_at: None,
            start: Instant::now(),
        }
    }

    #[tokio::test]
    async fn outbound_connection_exchanges_payload_with_utp_peer() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let server = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let server_addr = server.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let mut buf = vec![0_u8; 2_048];
            let (n, client_addr) = server.recv_from(&mut buf).await.unwrap();
            let syn = UtpPacket::decode(&buf[..n]).unwrap();
            assert_eq!(syn.packet_type, TYPE_SYN);
            assert_eq!(syn.seq_nr, 1);

            let server_seq_nr = 77;
            let state = UtpPacket {
                packet_type: TYPE_STATE,
                connection_id: syn.connection_id,
                timestamp_microseconds: 1,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: Vec::new(),
            };
            server.send_to(&state.encode(), client_addr).await.unwrap();

            let (n, client_addr) = server.recv_from(&mut buf).await.unwrap();
            let data = UtpPacket::decode(&buf[..n]).unwrap();
            assert_eq!(data.packet_type, TYPE_DATA);
            assert_eq!(data.connection_id, syn.connection_id.wrapping_add(1));
            assert_eq!(data.seq_nr, syn.seq_nr.wrapping_add(1));
            assert_eq!(data.ack_nr, server_seq_nr.wrapping_sub(1));
            assert_eq!(data.payload, b"ping");

            let ack = UtpPacket {
                packet_type: TYPE_STATE,
                connection_id: syn.connection_id,
                timestamp_microseconds: 2,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: data.seq_nr,
                selective_ack: Vec::new(),
                payload: Vec::new(),
            };
            server.send_to(&ack.encode(), client_addr).await.unwrap();

            let echo = UtpPacket {
                packet_type: TYPE_DATA,
                connection_id: syn.connection_id,
                timestamp_microseconds: 3,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: data.seq_nr,
                selective_ack: Vec::new(),
                payload: data.payload,
            };
            server.send_to(&echo.encode(), client_addr).await.unwrap();
        });

        let mut connection = UtpPeerTransport::connect(&network_lease, server_addr)
            .await
            .unwrap();
        connection.stream.write_all(b"ping").await.unwrap();

        let mut echoed = [0_u8; 4];
        connection.stream.read_exact(&mut echoed).await.unwrap();

        assert_eq!(&echoed, b"ping");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn outbound_connection_selective_acks_and_reorders_payloads() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let server = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let server_addr = server.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let mut buf = vec![0_u8; 2_048];
            let (n, client_addr) = server.recv_from(&mut buf).await.unwrap();
            let syn = UtpPacket::decode(&buf[..n]).unwrap();
            assert_eq!(syn.packet_type, TYPE_SYN);

            let server_seq_nr = 77;
            let state = UtpPacket {
                packet_type: TYPE_STATE,
                connection_id: syn.connection_id,
                timestamp_microseconds: 1,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: Vec::new(),
            };
            server.send_to(&state.encode(), client_addr).await.unwrap();

            let second = UtpPacket {
                packet_type: TYPE_DATA,
                connection_id: syn.connection_id,
                timestamp_microseconds: 2,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr.wrapping_add(1),
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: b"second".to_vec(),
            };
            server.send_to(&second.encode(), client_addr).await.unwrap();

            let (n, _) = time::timeout(Duration::from_secs(1), server.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            let sack = UtpPacket::decode(&buf[..n]).unwrap();
            assert_eq!(sack.packet_type, TYPE_STATE);
            assert_eq!(sack.ack_nr, server_seq_nr.wrapping_sub(1));
            assert_eq!(sack.selective_ack, vec![0b0000_0001, 0, 0, 0]);

            let first = UtpPacket {
                packet_type: TYPE_DATA,
                connection_id: syn.connection_id,
                timestamp_microseconds: 3,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: b"first".to_vec(),
            };
            server.send_to(&first.encode(), client_addr).await.unwrap();
        });

        let mut connection = UtpPeerTransport::connect(&network_lease, server_addr)
            .await
            .unwrap();
        let mut reordered = [0_u8; 11];
        connection.stream.read_exact(&mut reordered).await.unwrap();

        assert_eq!(&reordered, b"firstsecond");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn outbound_connection_delivers_reordered_tail_before_fin_eof() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let server = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let server_addr = server.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let mut buf = vec![0_u8; 2_048];
            let (n, client_addr) = server.recv_from(&mut buf).await.unwrap();
            let syn = UtpPacket::decode(&buf[..n]).unwrap();
            assert_eq!(syn.packet_type, TYPE_SYN);

            let server_seq_nr = 77;
            let state = UtpPacket {
                packet_type: TYPE_STATE,
                connection_id: syn.connection_id,
                timestamp_microseconds: 1,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: Vec::new(),
            };
            server.send_to(&state.encode(), client_addr).await.unwrap();

            let second = UtpPacket {
                packet_type: TYPE_DATA,
                connection_id: syn.connection_id,
                timestamp_microseconds: 2,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr.wrapping_add(1),
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: b"second".to_vec(),
            };
            server.send_to(&second.encode(), client_addr).await.unwrap();

            let fin = UtpPacket {
                packet_type: TYPE_FIN,
                connection_id: syn.connection_id,
                timestamp_microseconds: 3,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr.wrapping_add(2),
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: Vec::new(),
            };
            server.send_to(&fin.encode(), client_addr).await.unwrap();

            loop {
                let (n, _) = time::timeout(Duration::from_secs(1), server.recv_from(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                let sack = UtpPacket::decode(&buf[..n]).unwrap();
                if sack.packet_type == TYPE_STATE
                    && sack.ack_nr == server_seq_nr.wrapping_sub(1)
                    && sack.selective_ack == vec![0b0000_0011, 0, 0, 0]
                {
                    break;
                }
            }

            let first = UtpPacket {
                packet_type: TYPE_DATA,
                connection_id: syn.connection_id,
                timestamp_microseconds: 4,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: b"first".to_vec(),
            };
            server.send_to(&first.encode(), client_addr).await.unwrap();
        });

        let mut connection = UtpPeerTransport::connect(&network_lease, server_addr)
            .await
            .unwrap();
        let mut reordered = Vec::new();
        time::timeout(
            Duration::from_secs(1),
            connection.stream.read_to_end(&mut reordered),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(&reordered, b"firstsecond");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn outbound_connection_treats_duplicate_fin_as_eof_once() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let server = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let server_addr = server.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let mut buf = vec![0_u8; 2_048];
            let (n, client_addr) = server.recv_from(&mut buf).await.unwrap();
            let syn = UtpPacket::decode(&buf[..n]).unwrap();
            assert_eq!(syn.packet_type, TYPE_SYN);

            let server_seq_nr = 77;
            let state = UtpPacket {
                packet_type: TYPE_STATE,
                connection_id: syn.connection_id,
                timestamp_microseconds: 1,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: Vec::new(),
            };
            server.send_to(&state.encode(), client_addr).await.unwrap();

            let data = UtpPacket {
                packet_type: TYPE_DATA,
                connection_id: syn.connection_id,
                timestamp_microseconds: 2,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr,
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: b"done".to_vec(),
            };
            server.send_to(&data.encode(), client_addr).await.unwrap();

            let fin = UtpPacket {
                packet_type: TYPE_FIN,
                connection_id: syn.connection_id,
                timestamp_microseconds: 3,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: server_seq_nr.wrapping_add(1),
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: Vec::new(),
            };
            server.send_to(&fin.encode(), client_addr).await.unwrap();
            server.send_to(&fin.encode(), client_addr).await.unwrap();
        });

        let mut connection = UtpPeerTransport::connect(&network_lease, server_addr)
            .await
            .unwrap();
        let mut payload = Vec::new();
        time::timeout(
            Duration::from_secs(1),
            connection.stream.read_to_end(&mut payload),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(&payload, b"done");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn outbound_connection_retransmits_unacked_fin() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let server = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let server_addr = server.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let mut buf = vec![0_u8; 2_048];
            let (n, client_addr) = server.recv_from(&mut buf).await.unwrap();
            let syn = UtpPacket::decode(&buf[..n]).unwrap();
            assert_eq!(syn.packet_type, TYPE_SYN);

            let state = UtpPacket {
                packet_type: TYPE_STATE,
                connection_id: syn.connection_id,
                timestamp_microseconds: 1,
                timestamp_difference_microseconds: 0,
                wnd_size: RECEIVE_WINDOW,
                seq_nr: 77,
                ack_nr: syn.seq_nr,
                selective_ack: Vec::new(),
                payload: Vec::new(),
            };
            server.send_to(&state.encode(), client_addr).await.unwrap();

            let mut fins = Vec::new();
            while fins.len() < 2 {
                let (n, _) = time::timeout(Duration::from_secs(3), server.recv_from(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                let packet = UtpPacket::decode(&buf[..n]).unwrap();
                if packet.packet_type == TYPE_FIN {
                    fins.push(packet);
                }
            }

            assert_eq!(fins[0].connection_id, syn.connection_id.wrapping_add(1));
            assert_eq!(fins[1].connection_id, syn.connection_id.wrapping_add(1));
            assert_eq!(fins[0].seq_nr, fins[1].seq_nr);
        });

        let mut connection = UtpPeerTransport::connect(&network_lease, server_addr)
            .await
            .unwrap();
        connection.stream.shutdown().await.unwrap();

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn inbound_listener_resends_state_for_retransmitted_syn() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let listener = UtpPeerTransport::bind_listener(&network_lease, 0)
            .await
            .unwrap();
        let listen_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            listener.local_port().unwrap(),
        );
        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let syn = test_syn(0x1234, 9);
        let mut buf = vec![0_u8; 2_048];

        client.send_to(&syn.encode(), listen_addr).await.unwrap();
        let (n, _) = time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let first_state = UtpPacket::decode(&buf[..n]).unwrap();
        assert_eq!(first_state.packet_type, TYPE_STATE);
        assert_eq!(first_state.connection_id, syn.connection_id);
        assert_eq!(first_state.ack_nr, syn.seq_nr);

        client.send_to(&syn.encode(), listen_addr).await.unwrap();
        let (n, _) = time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let retransmitted_state = UtpPacket::decode(&buf[..n]).unwrap();
        assert_eq!(retransmitted_state, first_state);

        let _accepted = time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(
            time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "duplicate SYN should not enqueue a second accepted connection"
        );
    }

    #[tokio::test]
    async fn inbound_listener_ignores_adversarial_udp_without_accepting() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let listener = UtpPeerTransport::bind_listener(&network_lease, 0)
            .await
            .unwrap();
        let listen_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            listener.local_port().unwrap(),
        );
        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let mut wrong_version = test_syn(0x1234, 9).encode();
        wrong_version[0] = (TYPE_SYN << 4) | 2;
        let mut wrong_type = test_syn(0x1235, 10).encode();
        wrong_type[0] = (7 << 4) | UTP_VERSION;
        let stale_data = UtpPacket {
            packet_type: TYPE_DATA,
            connection_id: 0x1236,
            timestamp_microseconds: 10,
            timestamp_difference_microseconds: 0,
            wnd_size: RECEIVE_WINDOW,
            seq_nr: 11,
            ack_nr: 0,
            selective_ack: Vec::new(),
            payload: b"ignored".to_vec(),
        };
        let stale_data = stale_data.encode();

        for payload in [
            b"not utp".as_slice(),
            wrong_version.as_slice(),
            wrong_type.as_slice(),
            stale_data.as_slice(),
        ] {
            client.send_to(payload, listen_addr).await.unwrap();
        }

        assert!(
            time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "invalid or stale UDP datagrams must not create accepted uTP connections"
        );
    }

    #[tokio::test]
    async fn inbound_listener_resets_syn_when_accept_queue_is_full() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let listener = UtpPeerTransport::bind_listener(&network_lease, 0)
            .await
            .unwrap();
        let listen_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            listener.local_port().unwrap(),
        );
        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let mut buf = vec![0_u8; 2_048];
        let syn_count = UTP_ACCEPT_QUEUE_CAPACITY + 1;

        for offset in 0..syn_count {
            let connection_id = 0x2000_u16.wrapping_add(offset as u16);
            let seq_nr = 100_u16.wrapping_add(offset as u16);
            client
                .send_to(&test_syn(connection_id, seq_nr).encode(), listen_addr)
                .await
                .unwrap();
        }

        let mut states = Vec::new();
        let mut resets = 0usize;
        while states.len() + resets < syn_count {
            let (n, _) = time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            let packet = UtpPacket::decode(&buf[..n]).unwrap();
            match packet.packet_type {
                TYPE_STATE => states.push(packet),
                TYPE_RESET => resets += 1,
                _ => {}
            }
        }

        assert_eq!(states.len(), UTP_ACCEPT_QUEUE_CAPACITY);
        assert_eq!(resets, 1);

        for state in states {
            client
                .send_to(
                    &test_reset(state.connection_id.wrapping_add(1), state.ack_nr).encode(),
                    listen_addr,
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn inbound_listener_rebinds_after_drop() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let listener = UtpPeerTransport::bind_listener(&network_lease, 0)
            .await
            .unwrap();
        let port = listener.local_port().unwrap();
        drop(listener);

        let rebound = time::timeout(
            Duration::from_secs(1),
            UtpPeerTransport::bind_listener(&network_lease, port),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(rebound.local_port(), Some(port));
    }

    #[tokio::test]
    async fn generation_shutdown_closes_registered_utp_sessions() {
        let (_network_handle, network_lease) =
            crate::networking::runtime::test_network_lease_with_generation(90_201);
        let endpoint = UtpEndpoint::bind(
            &network_lease,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        )
        .await
        .expect("bind uTP endpoint");
        let (mut packets, _session_guard) = endpoint
            .register_session(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4242), 17)
            .expect("register uTP session");

        crate::networking::shared_udp::shutdown_shared_udp_generation(
            network_lease.generation_id(),
        )
        .await;

        assert!(time::timeout(Duration::from_millis(500), packets.recv())
            .await
            .expect("generation shutdown should close the session promptly")
            .is_none());
    }

    #[tokio::test]
    async fn inbound_listener_accepts_utp_stream() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let listener = UtpPeerTransport::bind_listener(&network_lease, 0)
            .await
            .unwrap();
        let listen_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            listener.local_port().unwrap(),
        );

        let accept_task = tokio::spawn(async move {
            let mut connection = listener.accept().await.unwrap();
            assert_eq!(
                connection.endpoint,
                PeerEndpoint::utp(connection.remote_addr)
            );
            assert_eq!(connection.direction, PeerConnectionDirection::Incoming);

            let mut payload = [0_u8; 4];
            connection.stream.read_exact(&mut payload).await.unwrap();
            assert_eq!(&payload, b"ping");

            connection.stream.write_all(b"pong").await.unwrap();
            time::sleep(Duration::from_millis(100)).await;
        });

        let mut client = UtpPeerTransport::connect(&network_lease, listen_addr)
            .await
            .unwrap();
        client.stream.write_all(b"ping").await.unwrap();

        let mut echoed = [0_u8; 4];
        client.stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"pong");

        accept_task.await.unwrap();
    }
}
