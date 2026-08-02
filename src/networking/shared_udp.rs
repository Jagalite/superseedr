// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, LazyLock, Mutex as StdMutex, Weak,
};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::networking::runtime::NetworkLease;

const MAX_DATAGRAM_SIZE: usize = 65_535;
const SHARED_UDP_SUBSCRIBER_QUEUE_CAPACITY: usize = 1_024;
const SHARED_UDP_BIND_RETRY_ATTEMPTS: usize = 16;
const SHARED_UDP_BIND_RETRY_DELAY: Duration = Duration::from_millis(1);
const CHAOS_PPM_DENOMINATOR: u64 = 1_000_000;

pub const SHARED_UDP_CHAOS_ENV: &str = "SUPERSEEDR_SHARED_UDP_CHAOS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SharedUdpFamily {
    Ipv4,
    Ipv6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SharedUdpKey {
    generation_id: u64,
    family: SharedUdpFamily,
    bind_addr: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedUdpProtocol {
    Dht,
    Utp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedUdpDatagram {
    pub source: SocketAddr,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
struct SharedUdpInner {
    key: SharedUdpKey,
    network_lease: NetworkLease,
    socket: Arc<UdpSocket>,
    chaos: SharedUdpChaosConfig,
    chaos_sequence: AtomicU64,
    dht_tx: StdMutex<Option<mpsc::Sender<SharedUdpDatagram>>>,
    utp_tx: StdMutex<Option<mpsc::Sender<SharedUdpDatagram>>>,
    shutdown_tx: watch::Sender<bool>,
    receive_task: StdMutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, Clone)]
pub struct SharedUdpHandle {
    inner: Arc<SharedUdpInner>,
}

static SHARED_UDP_REGISTRY: LazyLock<StdMutex<HashMap<SharedUdpKey, Weak<SharedUdpInner>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SharedUdpChaosConfig {
    seed: u64,
    loss_ppm: u32,
    duplicate_ppm: u32,
    corrupt_ppm: u32,
    reorder_ppm: u32,
    max_delay_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SharedUdpChaosAction {
    drop_original: bool,
    duplicate: bool,
    corrupt: bool,
    delay: Duration,
}

impl SharedUdpKey {
    pub fn new(generation_id: u64, bind_addr: SocketAddr, family: SharedUdpFamily) -> Self {
        Self {
            generation_id,
            family,
            bind_addr: normalize_bind_addr(bind_addr, family),
        }
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }
}

impl SharedUdpHandle {
    /// Shares the concrete UDP socket because DHT and uTP must demultiplex the
    /// same configured port. The generation ID prevents reuse after a network
    /// binding change invalidates the owning lease.
    pub async fn bind(
        network_lease: &NetworkLease,
        bind_addr: SocketAddr,
        family: SharedUdpFamily,
    ) -> io::Result<Self> {
        network_lease.ensure_valid().map_err(io::Error::other)?;
        let requested_key = SharedUdpKey::new(network_lease.generation_id(), bind_addr, family);
        let socket = {
            let mut attempt = 0usize;
            loop {
                if let Some(handle) = lookup_shared_udp(&requested_key) {
                    return Ok(handle);
                }

                match network_lease.bind_udp(requested_key.bind_addr).await {
                    Ok(socket) => break Arc::new(socket),
                    Err(error)
                        if error.kind() == io::ErrorKind::AddrInUse
                            && requested_key.bind_addr.port() != 0
                            && attempt < SHARED_UDP_BIND_RETRY_ATTEMPTS =>
                    {
                        attempt += 1;
                        tokio::time::sleep(SHARED_UDP_BIND_RETRY_DELAY).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        let actual_key = SharedUdpKey {
            generation_id: requested_key.generation_id,
            family: requested_key.family,
            bind_addr: socket.local_addr()?,
        };
        if let Some(handle) = lookup_shared_udp(&actual_key) {
            return Ok(handle);
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let inner = Arc::new(SharedUdpInner {
            key: actual_key,
            network_lease: network_lease.clone(),
            socket: socket.clone(),
            chaos: SharedUdpChaosConfig::from_env(),
            chaos_sequence: AtomicU64::new(0),
            dht_tx: StdMutex::new(None),
            utp_tx: StdMutex::new(None),
            shutdown_tx,
            receive_task: StdMutex::new(None),
        });

        let receive_task = spawn_receive_loop(
            socket,
            Arc::downgrade(&inner),
            inner.shutdown_tx.subscribe(),
            shutdown_rx,
            network_lease.subscribe_invalidation(),
        );
        *inner
            .receive_task
            .lock()
            .expect("shared udp receive task lock") = Some(receive_task);

        if requested_key.bind_addr.port() != 0 {
            register_shared_udp(requested_key, &inner);
        }
        register_shared_udp(actual_key, &inner);

        Ok(Self { inner })
    }
}

impl SharedUdpHandle {
    pub fn key(&self) -> SharedUdpKey {
        self.inner.key
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.socket.local_addr()
    }

    pub async fn send_to(&self, payload: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.inner
            .network_lease
            .ensure_valid()
            .map_err(io::Error::other)?;
        if self.inner.chaos.is_disabled() {
            return send_while_generation_is_valid(
                &self.inner.network_lease,
                self.inner.socket.send_to(payload, target),
            )
            .await;
        }
        let action = self.inner.chaos.action_for(
            self.inner.chaos_sequence.fetch_add(1, Ordering::Relaxed),
            self.inner.key,
            target,
            payload,
        );
        if action.drop_original {
            tracing::trace!(%target, "shared UDP chaos dropped outbound datagram");
            return Ok(payload.len());
        }

        let mut datagram = payload.to_vec();
        if action.corrupt {
            corrupt_datagram(&mut datagram);
        }

        if action.duplicate {
            send_chaos_datagram(
                self.inner.socket.clone(),
                self.inner.network_lease.clone(),
                target,
                datagram.clone(),
                action.delay,
            );
        }

        if !action.delay.is_zero() {
            send_chaos_datagram(
                self.inner.socket.clone(),
                self.inner.network_lease.clone(),
                target,
                datagram,
                action.delay,
            );
            return Ok(payload.len());
        }

        send_while_generation_is_valid(
            &self.inner.network_lease,
            self.inner.socket.send_to(&datagram, target),
        )
        .await
    }

    pub fn subscribe(
        &self,
        protocol: SharedUdpProtocol,
    ) -> io::Result<mpsc::Receiver<SharedUdpDatagram>> {
        let (tx, rx) = mpsc::channel(SHARED_UDP_SUBSCRIBER_QUEUE_CAPACITY);
        let slot = match protocol {
            SharedUdpProtocol::Dht => &self.inner.dht_tx,
            SharedUdpProtocol::Utp => &self.inner.utp_tx,
        };
        let mut guard = slot.lock().expect("shared udp subscriber lock");
        if guard.as_ref().is_some_and(|sender| !sender.is_closed()) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{protocol:?} UDP subscriber already registered"),
            ));
        }
        *guard = Some(tx);
        Ok(rx)
    }

    #[cfg(feature = "dht")]
    pub async fn close_if_unused(&self) {
        if self.has_active_subscribers() {
            return;
        }
        self.shutdown().await;
    }

    #[cfg(any(feature = "dht", test))]
    pub async fn shutdown(&self) {
        unregister_shared_udp(self.inner.key, &self.inner);
        let _ = self.inner.shutdown_tx.send(true);
        let receive_task = self
            .inner
            .receive_task
            .lock()
            .expect("shared udp receive task lock")
            .take();
        if let Some(receive_task) = receive_task {
            let _ = receive_task.await;
        }
    }

    #[cfg(feature = "dht")]
    fn has_active_subscribers(&self) -> bool {
        [SharedUdpProtocol::Dht, SharedUdpProtocol::Utp]
            .into_iter()
            .any(|protocol| {
                let slot = match protocol {
                    SharedUdpProtocol::Dht => &self.inner.dht_tx,
                    SharedUdpProtocol::Utp => &self.inner.utp_tx,
                };
                slot.lock()
                    .expect("shared udp subscriber lock")
                    .as_ref()
                    .is_some_and(|sender| !sender.is_closed())
            })
    }
}

impl Drop for SharedUdpInner {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

fn lookup_shared_udp(key: &SharedUdpKey) -> Option<SharedUdpHandle> {
    let mut registry = SHARED_UDP_REGISTRY
        .lock()
        .expect("shared udp registry lock");
    match registry.get(key).and_then(Weak::upgrade) {
        Some(inner) => Some(SharedUdpHandle { inner }),
        None => {
            registry.remove(key);
            None
        }
    }
}

fn register_shared_udp(key: SharedUdpKey, inner: &Arc<SharedUdpInner>) {
    let mut registry = SHARED_UDP_REGISTRY
        .lock()
        .expect("shared udp registry lock");
    registry.insert(key, Arc::downgrade(inner));
}

#[cfg(any(feature = "dht", test))]
fn unregister_shared_udp(key: SharedUdpKey, inner: &Arc<SharedUdpInner>) {
    let mut registry = SHARED_UDP_REGISTRY
        .lock()
        .expect("shared udp registry lock");
    let should_remove = registry
        .get(&key)
        .is_some_and(|registered| Weak::ptr_eq(registered, &Arc::downgrade(inner)));
    if should_remove {
        registry.remove(&key);
    }
}

fn spawn_receive_loop(
    socket: Arc<UdpSocket>,
    inner: Weak<SharedUdpInner>,
    mut inner_shutdown_rx: watch::Receiver<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
    mut network_invalidation_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; MAX_DATAGRAM_SIZE];
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                changed = inner_shutdown_rx.changed() => {
                    if changed.is_err() || *inner_shutdown_rx.borrow() {
                        break;
                    }
                }
                changed = network_invalidation_rx.changed() => {
                    if changed.is_err() || *network_invalidation_rx.borrow() {
                        break;
                    }
                }
                result = socket.recv_from(&mut buffer) => {
                    let (len, source) = match result {
                        Ok(result) => result,
                        Err(error) if is_transient_udp_recv_error(&error) => continue,
                        Err(_) => break,
                    };
                    let Some(inner) = inner.upgrade() else {
                        break;
                    };
                    dispatch_datagram(&inner, source, &buffer[..len]);
                }
            }
        }
    })
}

fn dispatch_datagram(inner: &SharedUdpInner, source: SocketAddr, payload: &[u8]) {
    let protocol = if looks_like_utp(payload) {
        SharedUdpProtocol::Utp
    } else if looks_like_dht(payload) {
        SharedUdpProtocol::Dht
    } else {
        return;
    };
    let slot = match protocol {
        SharedUdpProtocol::Dht => &inner.dht_tx,
        SharedUdpProtocol::Utp => &inner.utp_tx,
    };
    let sender = slot
        .lock()
        .expect("shared udp subscriber lock")
        .as_ref()
        .filter(|sender| !sender.is_closed())
        .cloned();
    if let Some(sender) = sender {
        let datagram = SharedUdpDatagram {
            source,
            payload: payload.to_vec(),
        };
        if sender.try_send(datagram).is_err() {
            tracing::debug!(?protocol, %source, "dropping shared UDP datagram for full subscriber queue");
        }
    }
}

fn send_chaos_datagram(
    socket: Arc<UdpSocket>,
    network_lease: NetworkLease,
    target: SocketAddr,
    datagram: Vec<u8>,
    delay: Duration,
) {
    tokio::spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let _ =
            send_while_generation_is_valid(&network_lease, socket.send_to(&datagram, target)).await;
    });
}

async fn send_while_generation_is_valid<T>(
    network_lease: &NetworkLease,
    operation: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    network_lease
        .cancel_on_invalidation(operation)
        .await
        .map_err(io::Error::other)?
}

fn corrupt_datagram(datagram: &mut [u8]) {
    if let Some(byte) = datagram.last_mut() {
        *byte ^= 0x80;
    }
}

impl SharedUdpChaosConfig {
    fn from_env() -> Self {
        std::env::var(SHARED_UDP_CHAOS_ENV)
            .ok()
            .and_then(|value| Self::parse(&value))
            .unwrap_or_default()
    }

    fn parse(value: &str) -> Option<Self> {
        let mut config = Self::default();
        for part in value
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let (key, value) = part.split_once('=')?;
            match key.trim() {
                "seed" => config.seed = value.trim().parse().ok()?,
                "loss_ppm" => config.loss_ppm = parse_ppm(value)?,
                "duplicate_ppm" => config.duplicate_ppm = parse_ppm(value)?,
                "corrupt_ppm" => config.corrupt_ppm = parse_ppm(value)?,
                "reorder_ppm" => config.reorder_ppm = parse_ppm(value)?,
                "max_delay_ms" => config.max_delay_ms = value.trim().parse().ok()?,
                _ => return None,
            }
        }
        Some(config)
    }

    fn is_disabled(self) -> bool {
        self.loss_ppm == 0
            && self.duplicate_ppm == 0
            && self.corrupt_ppm == 0
            && self.reorder_ppm == 0
            && self.max_delay_ms == 0
    }

    fn action_for(
        self,
        sequence: u64,
        key: SharedUdpKey,
        target: SocketAddr,
        payload: &[u8],
    ) -> SharedUdpChaosAction {
        if self.is_disabled() {
            return SharedUdpChaosAction {
                drop_original: false,
                duplicate: false,
                corrupt: false,
                delay: Duration::ZERO,
            };
        }

        let sample = chaos_sample(self.seed, sequence, key, target, payload);
        let drop_original = chance(sample, self.loss_ppm, 0);
        let duplicate = chance(sample, self.duplicate_ppm, 1);
        let corrupt = chance(sample, self.corrupt_ppm, 2);
        let reorder = chance(sample, self.reorder_ppm, 3);
        let delay = if self.max_delay_ms == 0 {
            Duration::ZERO
        } else if reorder {
            Duration::from_millis(chaos_delay_ms(sample, self.max_delay_ms, 4))
        } else {
            Duration::from_millis(chaos_delay_ms(sample, self.max_delay_ms, 5) / 4)
        };

        SharedUdpChaosAction {
            drop_original,
            duplicate,
            corrupt,
            delay,
        }
    }
}

fn parse_ppm(value: &str) -> Option<u32> {
    let ppm = value.trim().parse::<u32>().ok()?;
    (u64::from(ppm) <= CHAOS_PPM_DENOMINATOR).then_some(ppm)
}

fn chance(sample: u64, ppm: u32, stream: u64) -> bool {
    if ppm == 0 {
        return false;
    }
    splitmix64(sample ^ stream) % CHAOS_PPM_DENOMINATOR < u64::from(ppm)
}

fn chaos_delay_ms(sample: u64, max_delay_ms: u64, stream: u64) -> u64 {
    if max_delay_ms == 0 {
        return 0;
    }
    splitmix64(sample ^ stream) % (max_delay_ms.saturating_add(1))
}

fn chaos_sample(
    seed: u64,
    sequence: u64,
    key: SharedUdpKey,
    target: SocketAddr,
    payload: &[u8],
) -> u64 {
    let mut sample = seed ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    sample ^= socket_addr_hash(key.bind_addr());
    sample ^= socket_addr_hash(target).rotate_left(17);
    sample ^= (payload.len() as u64).rotate_left(29);
    if let Some(first) = payload.first() {
        sample ^= u64::from(*first).rotate_left(7);
    }
    if let Some(last) = payload.last() {
        sample ^= u64::from(*last).rotate_left(41);
    }
    splitmix64(sample)
}

fn socket_addr_hash(addr: SocketAddr) -> u64 {
    let ip_hash = match addr.ip() {
        IpAddr::V4(ip) => u32::from(ip) as u64,
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            segments.into_iter().fold(0u64, |hash, segment| {
                hash.rotate_left(11) ^ u64::from(segment)
            })
        }
    };
    ip_hash ^ (u64::from(addr.port()) << 48)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn looks_like_dht(payload: &[u8]) -> bool {
    payload.first().copied() == Some(b'd')
}

fn looks_like_utp(payload: &[u8]) -> bool {
    payload.len() >= 20 && payload[0] & 0x0f == 1 && payload[0] >> 4 <= 4
}

fn normalize_bind_addr(bind_addr: SocketAddr, family: SharedUdpFamily) -> SocketAddr {
    match family {
        SharedUdpFamily::Ipv4 if bind_addr.is_ipv4() => bind_addr,
        SharedUdpFamily::Ipv4 => SocketAddr::from((Ipv4Addr::UNSPECIFIED, bind_addr.port())),
        SharedUdpFamily::Ipv6 if bind_addr.is_ipv6() => bind_addr,
        SharedUdpFamily::Ipv6 => SocketAddr::from((Ipv6Addr::UNSPECIFIED, bind_addr.port())),
    }
}

fn is_transient_udp_recv_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
    )
}

pub fn family_for_addr(addr: SocketAddr) -> SharedUdpFamily {
    match addr.ip() {
        IpAddr::V4(_) => SharedUdpFamily::Ipv4,
        IpAddr::V6(_) => SharedUdpFamily::Ipv6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_udp_routes_dht_and_utp_on_one_socket() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let handle = SharedUdpHandle::bind(
            &network_lease,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            SharedUdpFamily::Ipv4,
        )
        .await
        .unwrap();
        let mut dht_rx = handle.subscribe(SharedUdpProtocol::Dht).unwrap();
        let mut utp_rx = handle.subscribe(SharedUdpProtocol::Utp).unwrap();
        let sender = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let local_addr = handle.local_addr().unwrap();

        sender.send_to(b"d1:eli201ee", local_addr).await.unwrap();
        sender
            .send_to(
                &[
                    (4 << 4) | 1,
                    0,
                    0,
                    1,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    1,
                    0,
                    0,
                ],
                local_addr,
            )
            .await
            .unwrap();

        let dht = dht_rx.recv().await.unwrap();
        let utp = utp_rx.recv().await.unwrap();

        assert_eq!(dht.payload, b"d1:eli201ee");
        assert_eq!(utp.payload[0] >> 4, 4);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shared_udp_never_reuses_an_invalidated_generation() {
        let (network_handle, old_lease) = crate::networking::runtime::test_network_lease();
        let old_handle = SharedUdpHandle::bind(
            &old_lease,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            SharedUdpFamily::Ipv4,
        )
        .await
        .unwrap();
        let mut state_rx = network_handle.subscribe();

        network_handle.rebuild_unrestricted().await.unwrap();
        state_rx.changed().await.unwrap();
        let new_lease = network_handle.try_lease().unwrap();
        let new_handle = SharedUdpHandle::bind(
            &new_lease,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            SharedUdpFamily::Ipv4,
        )
        .await
        .unwrap();

        assert_ne!(old_handle.key(), new_handle.key());
        assert!(old_handle
            .send_to(b"stale", new_handle.local_addr().unwrap())
            .await
            .is_err());

        old_handle.shutdown().await;
        new_handle.shutdown().await;
    }

    #[tokio::test]
    async fn shared_udp_send_wait_is_canceled_by_generation_invalidation() {
        let (network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let pending_send = send_while_generation_is_valid(
            &network_lease,
            std::future::pending::<io::Result<usize>>(),
        );
        tokio::pin!(pending_send);

        tokio::select! {
            biased;
            result = &mut pending_send => panic!("send completed before invalidation: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        network_handle
            .block("test pending shared UDP send cancellation")
            .await
            .unwrap();

        let error = tokio::time::timeout(Duration::from_millis(500), &mut pending_send)
            .await
            .expect("pending shared UDP send should cancel promptly")
            .expect_err("invalidated shared UDP send must fail");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("invalidated"));
    }

    #[test]
    fn shared_udp_chaos_config_parses_spec() {
        let config = SharedUdpChaosConfig::parse(
            "seed=42,loss_ppm=1000,duplicate_ppm=2000,corrupt_ppm=3000,reorder_ppm=4000,max_delay_ms=50",
        )
        .unwrap();

        assert_eq!(
            config,
            SharedUdpChaosConfig {
                seed: 42,
                loss_ppm: 1_000,
                duplicate_ppm: 2_000,
                corrupt_ppm: 3_000,
                reorder_ppm: 4_000,
                max_delay_ms: 50,
            }
        );
    }

    #[test]
    fn shared_udp_chaos_config_rejects_invalid_ppm() {
        assert!(SharedUdpChaosConfig::parse("loss_ppm=1000001").is_none());
    }

    #[test]
    fn shared_udp_chaos_action_is_deterministic() {
        let config = SharedUdpChaosConfig {
            seed: 7,
            loss_ppm: 10_000,
            duplicate_ppm: 10_000,
            corrupt_ppm: 10_000,
            reorder_ppm: 10_000,
            max_delay_ms: 20,
        };
        let key = SharedUdpKey::new(
            1,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10_000),
            SharedUdpFamily::Ipv4,
        );
        let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 20_000);

        assert_eq!(
            config.action_for(123, key, target, b"payload"),
            config.action_for(123, key, target, b"payload")
        );
    }

    #[test]
    fn shared_udp_chaos_action_can_force_all_faults() {
        let config = SharedUdpChaosConfig {
            seed: 7,
            loss_ppm: 1_000_000,
            duplicate_ppm: 1_000_000,
            corrupt_ppm: 1_000_000,
            reorder_ppm: 1_000_000,
            max_delay_ms: 20,
        };
        let key = SharedUdpKey::new(
            1,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10_000),
            SharedUdpFamily::Ipv4,
        );
        let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 20_000);

        let action = config.action_for(123, key, target, b"payload");

        assert!(action.drop_original);
        assert!(action.duplicate);
        assert!(action.corrupt);
        assert!(action.delay <= Duration::from_millis(20));
    }

    #[test]
    fn corrupt_datagram_flips_last_byte() {
        let mut datagram = vec![0x00, 0x7f];

        corrupt_datagram(&mut datagram);

        assert_eq!(datagram, vec![0x00, 0xff]);
    }
}
