// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code)]

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream as StdTcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::{lookup_host, TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const SUPERVISOR_COMMAND_CAPACITY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkBlockedReason(Arc<str>);

impl NetworkBlockedReason {
    pub fn new(reason: impl Into<Arc<str>>) -> Self {
        Self(reason.into())
    }
}

impl fmt::Display for NetworkBlockedReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub enum NetworkState {
    Ready(Arc<NetworkGeneration>),
    Blocked(NetworkBlockedReason),
}

impl NetworkState {
    pub fn generation_id(&self) -> Option<u64> {
        match self {
            Self::Ready(generation) => Some(generation.id()),
            Self::Blocked(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkLeaseError {
    Blocked(NetworkBlockedReason),
    Invalidated {
        generation_id: u64,
    },
    ResolutionFailed {
        host: Arc<str>,
        port: u16,
        reason: Arc<str>,
    },
}

impl fmt::Display for NetworkLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked(reason) => write!(formatter, "networking is blocked: {reason}"),
            Self::Invalidated { generation_id } => {
                write!(
                    formatter,
                    "network generation {generation_id} was invalidated"
                )
            }
            Self::ResolutionFailed { host, port, reason } => {
                write!(formatter, "failed to resolve {host}:{port}: {reason}")
            }
        }
    }
}

impl std::error::Error for NetworkLeaseError {}

#[derive(Debug)]
pub struct NetworkGeneration {
    id: u64,
    socket_factory: SocketFactory,
    tracker_http_client: reqwest::Client,
    general_http_client: reqwest::Client,
    invalidated: AtomicBool,
    invalidation_tx: watch::Sender<bool>,
}

impl NetworkGeneration {
    fn unrestricted(id: u64) -> io::Result<Self> {
        let (invalidation_tx, _) = watch::channel(false);
        let socket_factory = SocketFactory::unrestricted();
        let tracker_http_client = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(io::Error::other)?;
        let general_http_client = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(io::Error::other)?;

        Ok(Self {
            id,
            socket_factory,
            tracker_http_client,
            general_http_client,
            invalidated: AtomicBool::new(false),
            invalidation_tx,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn socket_factory(&self) -> &SocketFactory {
        &self.socket_factory
    }

    pub fn tracker_http_client(&self) -> &reqwest::Client {
        &self.tracker_http_client
    }

    pub fn general_http_client(&self) -> &reqwest::Client {
        &self.general_http_client
    }

    pub fn is_invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Acquire)
    }

    pub fn subscribe_invalidation(&self) -> watch::Receiver<bool> {
        self.invalidation_tx.subscribe()
    }

    fn invalidate(&self) {
        if !self.invalidated.swap(true, Ordering::AcqRel) {
            self.invalidation_tx.send_replace(true);
        }
    }
}

impl Drop for NetworkGeneration {
    fn drop(&mut self) {
        self.invalidate();
    }
}

#[derive(Debug, Clone)]
pub struct NetworkLease {
    generation: Arc<NetworkGeneration>,
}

impl NetworkLease {
    pub fn generation_id(&self) -> u64 {
        self.generation.id()
    }

    pub fn generation(&self) -> &Arc<NetworkGeneration> {
        &self.generation
    }

    pub fn ensure_valid(&self) -> Result<(), NetworkLeaseError> {
        if self.generation.is_invalidated() {
            Err(NetworkLeaseError::Invalidated {
                generation_id: self.generation.id(),
            })
        } else {
            Ok(())
        }
    }

    pub fn subscribe_invalidation(&self) -> watch::Receiver<bool> {
        self.generation.subscribe_invalidation()
    }

    pub async fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, NetworkLeaseError> {
        self.ensure_valid()?;
        let addresses = lookup_host((host, port))
            .await
            .map_err(|error| NetworkLeaseError::ResolutionFailed {
                host: Arc::from(host),
                port,
                reason: Arc::from(error.to_string()),
            })?
            .collect();
        self.ensure_valid()?;
        Ok(addresses)
    }

    pub async fn connect_tcp(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        self.ensure_valid().map_err(io::Error::other)?;
        let stream = self.generation.socket_factory.connect_tcp(addr).await?;
        self.ensure_valid().map_err(io::Error::other)?;
        Ok(stream)
    }

    pub async fn bind_tcp_listener(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        self.ensure_valid().map_err(io::Error::other)?;
        let listener = self.generation.socket_factory.bind_tcp_listener(addr)?;
        self.ensure_valid().map_err(io::Error::other)?;
        Ok(listener)
    }

    pub async fn bind_udp(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        self.ensure_valid().map_err(io::Error::other)?;
        let socket = self.generation.socket_factory.bind_udp(addr)?;
        self.ensure_valid().map_err(io::Error::other)?;
        Ok(socket)
    }

    pub fn tracker_http_client(&self) -> Result<reqwest::Client, NetworkLeaseError> {
        self.ensure_valid()?;
        Ok(self.generation.tracker_http_client().clone())
    }

    pub fn general_http_client(&self) -> Result<reqwest::Client, NetworkLeaseError> {
        self.ensure_valid()?;
        Ok(self.generation.general_http_client().clone())
    }
}

#[derive(Debug, Clone)]
pub struct NetworkHandle {
    state_rx: watch::Receiver<NetworkState>,
    command_tx: mpsc::Sender<NetworkSupervisorCommand>,
}

impl NetworkHandle {
    pub fn try_lease(&self) -> Result<NetworkLease, NetworkLeaseError> {
        let generation = match &*self.state_rx.borrow() {
            NetworkState::Ready(generation) => generation.clone(),
            NetworkState::Blocked(reason) => {
                return Err(NetworkLeaseError::Blocked(reason.clone()))
            }
        };
        let lease = NetworkLease { generation };
        lease.ensure_valid()?;
        Ok(lease)
    }

    pub fn subscribe(&self) -> watch::Receiver<NetworkState> {
        self.state_rx.clone()
    }

    pub async fn rebuild_unrestricted(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.command_tx
            .send(NetworkSupervisorCommand::RebuildUnrestricted)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    pub async fn block(
        &self,
        reason: impl Into<Arc<str>>,
    ) -> Result<(), mpsc::error::SendError<()>> {
        self.command_tx
            .send(NetworkSupervisorCommand::Block(NetworkBlockedReason::new(
                reason,
            )))
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    pub async fn shutdown(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.command_tx
            .send(NetworkSupervisorCommand::Shutdown)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }
}

#[derive(Debug)]
enum NetworkSupervisorCommand {
    RebuildUnrestricted,
    Block(NetworkBlockedReason),
    Shutdown,
}

#[derive(Debug)]
pub struct NetworkSupervisor {
    next_generation_id: AtomicU64,
    state_tx: watch::Sender<NetworkState>,
    command_rx: mpsc::Receiver<NetworkSupervisorCommand>,
}

impl NetworkSupervisor {
    pub fn spawn_unrestricted() -> io::Result<(NetworkHandle, JoinHandle<()>)> {
        let generation = Arc::new(NetworkGeneration::unrestricted(1)?);
        let (state_tx, state_rx) = watch::channel(NetworkState::Ready(generation));
        let (command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let supervisor = Self {
            next_generation_id: AtomicU64::new(2),
            state_tx,
            command_rx,
        };
        let task = tokio::spawn(supervisor.run());
        Ok((
            NetworkHandle {
                state_rx,
                command_tx,
            },
            task,
        ))
    }

    async fn run(mut self) {
        while let Some(command) = self.command_rx.recv().await {
            match command {
                NetworkSupervisorCommand::RebuildUnrestricted => {
                    self.invalidate_current();
                    let generation_id = self.next_generation_id.fetch_add(1, Ordering::Relaxed);
                    match NetworkGeneration::unrestricted(generation_id) {
                        Ok(generation) => {
                            self.state_tx
                                .send_replace(NetworkState::Ready(Arc::new(generation)));
                        }
                        Err(error) => {
                            self.state_tx.send_replace(NetworkState::Blocked(
                                NetworkBlockedReason::new(error.to_string()),
                            ));
                        }
                    }
                }
                NetworkSupervisorCommand::Block(reason) => {
                    self.invalidate_current();
                    self.state_tx.send_replace(NetworkState::Blocked(reason));
                }
                NetworkSupervisorCommand::Shutdown => {
                    self.invalidate_current();
                    self.state_tx
                        .send_replace(NetworkState::Blocked(NetworkBlockedReason::new(
                            "network supervisor shut down",
                        )));
                    break;
                }
            }
        }
        self.invalidate_current();
    }

    fn invalidate_current(&self) {
        if let NetworkState::Ready(generation) = &*self.state_tx.borrow() {
            generation.invalidate();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SocketFactory;

impl SocketFactory {
    fn unrestricted() -> Self {
        Self
    }

    pub async fn connect_tcp(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        let socket = self.tcp_socket(addr)?;
        socket.set_nonblocking(true)?;
        match socket.connect(&SockAddr::from(addr)) {
            Ok(()) => {}
            Err(error) if connect_is_in_progress(&error) => {}
            Err(error) => return Err(error),
        }
        let std_stream: StdTcpStream = socket.into();
        let stream = TcpStream::from_std(std_stream)?;
        stream.writable().await?;
        if let Some(error) = stream.take_error()? {
            return Err(error);
        }
        Ok(stream)
    }

    pub fn bind_tcp_listener(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        let socket = self.tcp_socket(addr)?;
        socket.set_reuse_address(true)?;
        if addr.is_ipv6() {
            socket.set_only_v6(true)?;
        }
        socket.bind(&SockAddr::from(addr))?;
        socket.listen(1_024)?;
        socket.set_nonblocking(true)?;
        let listener: std::net::TcpListener = socket.into();
        TcpListener::from_std(listener)
    }

    pub fn bind_udp(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        let domain = domain_for(addr);
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        if addr.is_ipv6() {
            socket.set_only_v6(true)?;
        }
        socket.bind(&SockAddr::from(addr))?;
        socket.set_nonblocking(true)?;
        let socket: std::net::UdpSocket = socket.into();
        UdpSocket::from_std(socket)
    }

    fn tcp_socket(&self, addr: SocketAddr) -> io::Result<Socket> {
        Socket::new(domain_for(addr), Type::STREAM, Some(Protocol::TCP))
    }
}

#[cfg(test)]
pub(crate) fn test_network_lease() -> (NetworkHandle, NetworkLease) {
    let (handle, _supervisor_task) =
        NetworkSupervisor::spawn_unrestricted().expect("spawn test network supervisor");
    let lease = handle.try_lease().expect("obtain test network lease");
    (handle, lease)
}

#[cfg(test)]
pub(crate) fn test_network_handle() -> NetworkHandle {
    let generation =
        Arc::new(NetworkGeneration::unrestricted(1).expect("construct test network generation"));
    let (_state_tx, state_rx) = watch::channel(NetworkState::Ready(generation));
    let (command_tx, _command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
    NetworkHandle {
        state_rx,
        command_tx,
    }
}

fn domain_for(addr: SocketAddr) -> Domain {
    match addr.ip() {
        IpAddr::V4(_) => Domain::IPV4,
        IpAddr::V6(_) => Domain::IPV6,
    }
}

fn connect_is_in_progress(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(unix)]
    return matches!(error.raw_os_error(), Some(36 | 114 | 115));
    #[cfg(windows)]
    return matches!(error.raw_os_error(), Some(10035 | 10036));
    #[allow(unreachable_code)]
    false
}

pub fn unspecified_addr(ipv6: bool, port: u16) -> SocketAddr {
    if ipv6 {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_network_construction_is_centralized() {
        const DIRECT_NETWORK_CALLS: &[&str] = &[
            "TcpStream::connect(",
            "TcpListener::bind(",
            "UdpSocket::bind(",
            "lookup_host(",
            "Client::new(",
            "Client::builder(",
            "Socket::new(",
        ];

        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut violations = Vec::new();
        inspect_source_tree(&source_root, &mut |path, source| {
            let relative = path.strip_prefix(&source_root).expect("source path");
            let file_name = relative
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if relative == std::path::Path::new("networking/runtime.rs")
                || relative == std::path::Path::new("synthetic_load.rs")
                || file_name.ends_with("_tests.rs")
                || file_name == "test_support.rs"
            {
                return;
            }

            let production = source
                .split_once("\n#[cfg(test)]\nmod tests")
                .map_or(source, |(production, _)| production);
            for (line_index, line) in production.lines().enumerate() {
                for direct_call in DIRECT_NETWORK_CALLS {
                    if line.contains(direct_call) {
                        violations.push(format!(
                            "{}:{} uses {direct_call}",
                            relative.display(),
                            line_index + 1
                        ));
                    }
                }
            }
        });

        assert!(
            violations.is_empty(),
            "production networking must use NetworkLease:\n{}",
            violations.join("\n")
        );
    }

    fn inspect_source_tree(
        directory: &std::path::Path,
        inspect: &mut impl FnMut(&std::path::Path, &str),
    ) {
        for entry in std::fs::read_dir(directory).expect("read source directory") {
            let path = entry.expect("read source entry").path();
            if path.is_dir() {
                inspect_source_tree(&path, inspect);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = std::fs::read_to_string(&path).expect("read Rust source");
                inspect(&path, &source);
            }
        }
    }

    #[tokio::test]
    async fn unrestricted_supervisor_publishes_a_usable_generation() {
        let (handle, task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let lease = handle.try_lease().unwrap();
        assert_eq!(lease.generation_id(), 1);
        assert!(!lease.generation().is_invalidated());

        handle.shutdown().await.unwrap();
        task.await.unwrap();
        assert!(lease.generation().is_invalidated());
        assert!(handle.try_lease().is_err());
    }

    #[tokio::test]
    async fn invalidation_closes_the_lease_boundary_before_state_replacement() {
        let (handle, task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let old_lease = handle.try_lease().unwrap();
        let mut state_rx = handle.subscribe();
        handle
            .block("interface snapshot unavailable")
            .await
            .unwrap();

        state_rx.changed().await.unwrap();
        assert!(old_lease.ensure_valid().is_err());
        assert!(handle.try_lease().is_err());

        handle.shutdown().await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn rebuild_never_returns_the_invalidated_generation() {
        let (handle, task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let old_lease = handle.try_lease().unwrap();
        let mut state_rx = handle.subscribe();
        handle.rebuild_unrestricted().await.unwrap();

        state_rx.changed().await.unwrap();
        let new_lease = handle.try_lease().unwrap();
        assert!(old_lease.ensure_valid().is_err());
        assert!(new_lease.generation_id() > old_lease.generation_id());

        handle.shutdown().await.unwrap();
        task.await.unwrap();
    }
}
