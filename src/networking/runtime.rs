// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
#[cfg(unix)]
use std::ffi::{CStr, CString};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream as StdTcpStream};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::{lookup_host, TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const SUPERVISOR_COMMAND_CAPACITY: usize = 8;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkBindingMode {
    #[default]
    Any,
    Interface,
    LocalAddress,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct NetworkBindingConfig {
    pub mode: NetworkBindingMode,
    pub interface: Option<String>,
    pub enable_ipv4: bool,
    pub enable_ipv6: bool,
    pub ipv4_address: Option<Ipv4Addr>,
    pub ipv6_address: Option<Ipv6Addr>,
}

impl Default for NetworkBindingConfig {
    fn default() -> Self {
        Self {
            mode: NetworkBindingMode::Any,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: true,
            ipv4_address: None,
            ipv6_address: None,
        }
    }
}

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
        Self::from_config(id, &NetworkBindingConfig::default())
    }

    fn from_config(id: u64, config: &NetworkBindingConfig) -> io::Result<Self> {
        let (invalidation_tx, _) = watch::channel(false);
        let socket_factory = SocketFactory::from_config(config)?;
        socket_factory.preflight()?;
        let tracker_http_client = socket_factory
            .configure_http_client(reqwest::Client::builder())
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(io::Error::other)?;
        let general_http_client = socket_factory
            .configure_http_client(reqwest::Client::builder())
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
    pub fn spawn_with_config(config: &NetworkBindingConfig) -> (NetworkHandle, JoinHandle<()>) {
        let initial_state = match NetworkGeneration::from_config(1, config) {
            Ok(generation) => NetworkState::Ready(Arc::new(generation)),
            Err(error) => NetworkState::Blocked(NetworkBlockedReason::new(format!(
                "network binding configuration could not be activated: {error}"
            ))),
        };
        let (state_tx, state_rx) = watch::channel(initial_state);
        let (command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let supervisor = Self {
            next_generation_id: AtomicU64::new(2),
            state_tx,
            command_rx,
        };
        let task = tokio::spawn(supervisor.run());
        (
            NetworkHandle {
                state_rx,
                command_tx,
            },
            task,
        )
    }

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

#[derive(Debug, Clone)]
pub struct SocketFactory {
    binding: ResolvedNetworkBinding,
}

#[derive(Debug, Clone)]
struct ResolvedNetworkBinding {
    mode: NetworkBindingMode,
    interface_name: Option<Arc<str>>,
    interface_index: Option<NonZeroU32>,
    enable_ipv4: bool,
    enable_ipv6: bool,
    ipv4_address: Option<Ipv4Addr>,
    ipv6_address: Option<Ipv6Addr>,
    http_local_address: Option<IpAddr>,
}

#[derive(Debug)]
struct InterfaceSnapshot {
    index: NonZeroU32,
    ipv4_addresses: Vec<Ipv4Addr>,
    ipv6_addresses: Vec<Ipv6Addr>,
}

impl SocketFactory {
    fn unrestricted() -> Self {
        Self {
            binding: ResolvedNetworkBinding::unrestricted(),
        }
    }

    fn from_config(config: &NetworkBindingConfig) -> io::Result<Self> {
        Ok(Self {
            binding: ResolvedNetworkBinding::resolve(config)?,
        })
    }

    pub async fn connect_tcp(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        let socket = self.tcp_socket(addr)?;
        self.bind_outgoing_source(&socket, addr)?;
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
        socket.bind(&SockAddr::from(self.bound_local_addr(addr)?))?;
        socket.listen(1_024)?;
        socket.set_nonblocking(true)?;
        let listener: std::net::TcpListener = socket.into();
        TcpListener::from_std(listener)
    }

    pub fn bind_udp(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        let domain = domain_for(addr);
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        self.apply_interface_binding(&socket, addr)?;
        if addr.is_ipv6() {
            socket.set_only_v6(true)?;
        }
        socket.bind(&SockAddr::from(self.bound_local_addr(addr)?))?;
        socket.set_nonblocking(true)?;
        let socket: std::net::UdpSocket = socket.into();
        UdpSocket::from_std(socket)
    }

    fn tcp_socket(&self, addr: SocketAddr) -> io::Result<Socket> {
        let socket = Socket::new(domain_for(addr), Type::STREAM, Some(Protocol::TCP))?;
        self.apply_interface_binding(&socket, addr)?;
        Ok(socket)
    }

    fn preflight(&self) -> io::Result<()> {
        for addr in self.enabled_probe_addresses() {
            let tcp = self.tcp_socket(addr)?;
            self.bind_outgoing_source(&tcp, addr)?;
            let udp = Socket::new(domain_for(addr), Type::DGRAM, Some(Protocol::UDP))?;
            self.apply_interface_binding(&udp, addr)?;
            if self.source_address(addr).is_some() {
                udp.bind(&SockAddr::from(self.bound_local_addr(addr)?))?;
            }
        }
        Ok(())
    }

    fn enabled_probe_addresses(&self) -> impl Iterator<Item = SocketAddr> {
        let mut addresses = Vec::with_capacity(2);
        if self.binding.enable_ipv4 {
            addresses.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
        }
        if self.binding.enable_ipv6 {
            addresses.push(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0));
        }
        addresses.into_iter()
    }

    fn bind_outgoing_source(&self, socket: &Socket, remote: SocketAddr) -> io::Result<()> {
        if let Some(source) = self.source_address(remote) {
            socket.bind(&SockAddr::from(SocketAddr::new(source, 0)))?;
        }
        Ok(())
    }

    fn bound_local_addr(&self, requested: SocketAddr) -> io::Result<SocketAddr> {
        self.ensure_family_enabled(requested)?;
        let Some(source) = self.source_address(requested) else {
            return Ok(requested);
        };
        if !requested.ip().is_unspecified() && requested.ip() != source {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!(
                    "requested bind address {} does not match configured source {source}",
                    requested.ip()
                ),
            ));
        }
        Ok(SocketAddr::new(source, requested.port()))
    }

    fn source_address(&self, addr: SocketAddr) -> Option<IpAddr> {
        match addr {
            SocketAddr::V4(_) => self.binding.ipv4_address.map(IpAddr::V4),
            SocketAddr::V6(_) => self.binding.ipv6_address.map(IpAddr::V6),
        }
    }

    fn ensure_family_enabled(&self, addr: SocketAddr) -> io::Result<()> {
        let enabled = match addr {
            SocketAddr::V4(_) => self.binding.enable_ipv4,
            SocketAddr::V6(_) => self.binding.enable_ipv6,
        };
        if enabled {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!(
                    "{} is disabled by the network binding policy",
                    if addr.is_ipv4() { "IPv4" } else { "IPv6" }
                ),
            ))
        }
    }

    fn apply_interface_binding(&self, socket: &Socket, addr: SocketAddr) -> io::Result<()> {
        self.ensure_family_enabled(addr)?;
        let Some(_interface_name) = self.binding.interface_name.as_deref() else {
            return Ok(());
        };

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            socket.bind_device(Some(_interface_name.as_bytes()))?;
            return Ok(());
        }

        #[cfg(any(
            target_os = "illumos",
            target_os = "ios",
            target_os = "macos",
            target_os = "solaris",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos"
        ))]
        {
            let index = self.binding.interface_index.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "resolved interface has no index")
            })?;
            match addr {
                SocketAddr::V4(_) => socket.bind_device_by_index_v4(Some(index)),
                SocketAddr::V6(_) => socket.bind_device_by_index_v6(Some(index)),
            }
        }

        #[cfg(not(any(
            target_os = "android",
            target_os = "fuchsia",
            target_os = "linux",
            target_os = "illumos",
            target_os = "ios",
            target_os = "macos",
            target_os = "solaris",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos"
        )))]
        {
            let _ = (socket, addr, _interface_name);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "strict interface binding is not supported on this operating system",
            ))
        }
    }

    fn configure_http_client(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        if let Some(local_address) = self.binding.http_local_address {
            builder = builder.local_address(local_address);
        }
        #[cfg(any(
            target_os = "android",
            target_os = "fuchsia",
            target_os = "illumos",
            target_os = "ios",
            target_os = "linux",
            target_os = "macos",
            target_os = "solaris",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos"
        ))]
        if let Some(interface_name) = self.binding.interface_name.as_deref() {
            builder = builder.interface(interface_name);
        }
        builder
    }
}

impl ResolvedNetworkBinding {
    fn unrestricted() -> Self {
        Self {
            mode: NetworkBindingMode::Any,
            interface_name: None,
            interface_index: None,
            enable_ipv4: true,
            enable_ipv6: true,
            ipv4_address: None,
            ipv6_address: None,
            http_local_address: None,
        }
    }

    fn resolve(config: &NetworkBindingConfig) -> io::Result<Self> {
        match config.mode {
            NetworkBindingMode::Any => Ok(Self::unrestricted()),
            NetworkBindingMode::Interface => Self::resolve_interface(config),
            NetworkBindingMode::LocalAddress => Self::resolve_local_address(config),
        }
    }

    fn resolve_interface(config: &NetworkBindingConfig) -> io::Result<Self> {
        ensure_any_family_enabled(config)?;
        let interface_name = config
            .interface
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "interface binding mode requires an interface name",
                )
            })?;
        let snapshot = interface_snapshot(interface_name)?;
        if config.enable_ipv4 && snapshot.ipv4_addresses.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("interface {interface_name} has no IPv4 address"),
            ));
        }
        if config.enable_ipv6 && snapshot.ipv6_addresses.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("interface {interface_name} has no IPv6 address"),
            ));
        }
        validate_explicit_addresses(config, Some((&snapshot, interface_name)))?;
        reject_dual_family_exact_source(config)?;

        let http_local_address = single_family_http_address(config, &snapshot);
        Ok(Self {
            mode: NetworkBindingMode::Interface,
            interface_name: Some(Arc::from(interface_name)),
            interface_index: Some(snapshot.index),
            enable_ipv4: config.enable_ipv4,
            enable_ipv6: config.enable_ipv6,
            ipv4_address: config.ipv4_address,
            ipv6_address: config.ipv6_address,
            http_local_address,
        })
    }

    fn resolve_local_address(config: &NetworkBindingConfig) -> io::Result<Self> {
        ensure_any_family_enabled(config)?;
        if config
            .interface
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local-address mode cannot also specify an interface name",
            ));
        }
        if config.enable_ipv4 == config.enable_ipv6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local-address mode currently requires exactly one enabled address family",
            ));
        }
        validate_explicit_addresses(config, None)?;
        let http_local_address = if config.enable_ipv4 {
            Some(IpAddr::V4(config.ipv4_address.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "IPv4 local-address mode requires ipv4_address",
                )
            })?))
        } else {
            Some(IpAddr::V6(config.ipv6_address.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "IPv6 local-address mode requires ipv6_address",
                )
            })?))
        };
        let all_interfaces = all_interface_addresses()?;
        if !all_interfaces
            .iter()
            .any(|address| Some(*address) == http_local_address)
        {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!(
                    "configured local address {} is not assigned to this host",
                    http_local_address.expect("validated local address")
                ),
            ));
        }

        Ok(Self {
            mode: NetworkBindingMode::LocalAddress,
            interface_name: None,
            interface_index: None,
            enable_ipv4: config.enable_ipv4,
            enable_ipv6: config.enable_ipv6,
            ipv4_address: config.ipv4_address,
            ipv6_address: config.ipv6_address,
            http_local_address,
        })
    }
}

fn ensure_any_family_enabled(config: &NetworkBindingConfig) -> io::Result<()> {
    if config.enable_ipv4 || config.enable_ipv6 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "strict network binding requires at least one enabled address family",
        ))
    }
}

fn reject_dual_family_exact_source(config: &NetworkBindingConfig) -> io::Result<()> {
    if config.enable_ipv4
        && config.enable_ipv6
        && (config.ipv4_address.is_some() || config.ipv6_address.is_some())
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "exact source selection with both address families is not supported; use interface-only binding or enable one family",
        ))
    } else {
        Ok(())
    }
}

fn validate_explicit_addresses(
    config: &NetworkBindingConfig,
    interface: Option<(&InterfaceSnapshot, &str)>,
) -> io::Result<()> {
    if !config.enable_ipv4 && config.ipv4_address.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ipv4_address cannot be set while IPv4 is disabled",
        ));
    }
    if !config.enable_ipv6 && config.ipv6_address.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ipv6_address cannot be set while IPv6 is disabled",
        ));
    }
    if let Some((snapshot, interface_name)) = interface {
        if let Some(address) = config.ipv4_address {
            if !snapshot.ipv4_addresses.contains(&address) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("IPv4 address {address} is not assigned to interface {interface_name}"),
                ));
            }
        }
        if let Some(address) = config.ipv6_address {
            if !snapshot.ipv6_addresses.contains(&address) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("IPv6 address {address} is not assigned to interface {interface_name}"),
                ));
            }
        }
    }
    Ok(())
}

fn single_family_http_address(
    config: &NetworkBindingConfig,
    snapshot: &InterfaceSnapshot,
) -> Option<IpAddr> {
    match (config.enable_ipv4, config.enable_ipv6) {
        (true, false) => config
            .ipv4_address
            .or_else(|| snapshot.ipv4_addresses.first().copied())
            .map(IpAddr::V4),
        (false, true) => config
            .ipv6_address
            .or_else(|| snapshot.ipv6_addresses.first().copied())
            .map(IpAddr::V6),
        _ => None,
    }
}

#[cfg(unix)]
fn interface_snapshot(interface_name: &str) -> io::Result<InterfaceSnapshot> {
    let name = CString::new(interface_name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name contains an interior NUL byte",
        )
    })?;
    let raw_index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    let index = NonZeroU32::new(raw_index).ok_or_else(|| {
        let error = io::Error::last_os_error();
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface {interface_name} was not found: {error}"),
        )
    })?;
    let mut ipv4_addresses = Vec::new();
    let mut ipv6_addresses = Vec::new();
    visit_interface_addresses(|name, address, is_up| {
        if name == interface_name && is_up {
            match address {
                IpAddr::V4(address) => ipv4_addresses.push(address),
                IpAddr::V6(address) => ipv6_addresses.push(address),
            }
        }
    })?;
    ipv4_addresses.sort_unstable();
    ipv4_addresses.dedup();
    ipv6_addresses.sort_unstable();
    ipv6_addresses.dedup();
    Ok(InterfaceSnapshot {
        index,
        ipv4_addresses,
        ipv6_addresses,
    })
}

#[cfg(not(unix))]
fn interface_snapshot(interface_name: &str) -> io::Result<InterfaceSnapshot> {
    let _ = interface_name;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "strict interface binding is not supported on this operating system",
    ))
}

#[cfg(unix)]
fn all_interface_addresses() -> io::Result<Vec<IpAddr>> {
    let mut addresses = Vec::new();
    visit_interface_addresses(|_, address, is_up| {
        if is_up {
            addresses.push(address);
        }
    })?;
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

#[cfg(not(unix))]
fn all_interface_addresses() -> io::Result<Vec<IpAddr>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "local-address binding is not supported on this operating system",
    ))
}

#[cfg(unix)]
fn visit_interface_addresses(mut visit: impl FnMut(&str, IpAddr, bool)) -> io::Result<()> {
    let mut head = std::ptr::null_mut::<libc::ifaddrs>();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    struct InterfaceAddresses(*mut libc::ifaddrs);
    impl Drop for InterfaceAddresses {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
    let addresses = InterfaceAddresses(head);
    let mut current = addresses.0;
    while !current.is_null() {
        let entry = unsafe { &*current };
        if !entry.ifa_addr.is_null() && !entry.ifa_name.is_null() {
            let name = unsafe { CStr::from_ptr(entry.ifa_name) }.to_string_lossy();
            let is_up = entry.ifa_flags & (libc::IFF_UP as u32) != 0;
            let family = unsafe { (*entry.ifa_addr).sa_family as i32 };
            let address = match family {
                libc::AF_INET => {
                    let address = unsafe { &*(entry.ifa_addr.cast::<libc::sockaddr_in>()) };
                    Some(IpAddr::V4(Ipv4Addr::from(
                        address.sin_addr.s_addr.to_ne_bytes(),
                    )))
                }
                libc::AF_INET6 => {
                    let address = unsafe { &*(entry.ifa_addr.cast::<libc::sockaddr_in6>()) };
                    Some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)))
                }
                _ => None,
            };
            if let Some(address) = address {
                visit(&name, address, is_up);
            }
        }
        current = entry.ifa_next;
    }
    Ok(())
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

    #[tokio::test]
    async fn missing_strict_interface_starts_blocked_without_fallback() {
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some("superseedr-missing-interface".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
        };
        let (handle, task) = NetworkSupervisor::spawn_with_config(&config);

        let error = handle.try_lease().expect_err("strict binding must block");
        assert!(error.to_string().contains("was not found"));

        handle.shutdown().await.unwrap();
        task.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_address_policy_binds_incoming_and_outgoing_sockets() {
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
        };
        let factory = SocketFactory::from_config(&config).expect("resolve loopback address");
        factory.preflight().expect("preflight loopback address");

        let listener = factory
            .bind_tcp_listener(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            .expect("bind loopback listener");
        assert_eq!(
            listener.local_addr().expect("listener address").ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );

        let socket = factory
            .tcp_socket(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9))
            .expect("construct outgoing socket");
        factory
            .bind_outgoing_source(&socket, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9))
            .expect("bind outgoing source");
        assert_eq!(
            socket
                .local_addr()
                .expect("outgoing address")
                .as_socket()
                .map(|address| address.ip()),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    #[cfg(unix)]
    #[test]
    fn interface_policy_applies_the_resolved_os_interface_index() {
        let (interface_name, interface_address) = loopback_interface();
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some(interface_name.clone()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(interface_address),
            ipv6_address: None,
        };
        let factory = SocketFactory::from_config(&config).expect("resolve loopback interface");
        factory.preflight().expect("preflight loopback interface");
        let socket = factory
            .tcp_socket(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9))
            .expect("construct interface-bound socket");

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        assert_eq!(
            socket.device().expect("read bound device").as_deref(),
            Some(interface_name.as_bytes())
        );
        #[cfg(any(
            target_os = "illumos",
            target_os = "ios",
            target_os = "macos",
            target_os = "solaris",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos"
        ))]
        assert_eq!(
            socket
                .device_index_v4()
                .expect("read bound interface index"),
            factory.binding.interface_index
        );
    }

    #[cfg(unix)]
    fn loopback_interface() -> (String, Ipv4Addr) {
        let mut selected = None;
        visit_interface_addresses(|name, address, is_up| {
            if selected.is_none() && is_up {
                if let IpAddr::V4(address) = address {
                    if address.is_loopback() {
                        selected = Some((name.to_string(), address));
                    }
                }
            }
        })
        .expect("enumerate interfaces");
        selected.expect("find an active IPv4 loopback interface")
    }
}
