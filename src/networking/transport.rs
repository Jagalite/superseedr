// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;
use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::watch;

use super::runtime::NetworkLease;
use super::{NetworkScopeId, PeerTransportKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerConnectionDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerEndpoint {
    pub kind: PeerTransportKind,
    pub addr: SocketAddr,
}

impl PeerEndpoint {
    pub const fn new(kind: PeerTransportKind, addr: SocketAddr) -> Self {
        Self { kind, addr }
    }

    pub const fn tcp(addr: SocketAddr) -> Self {
        Self::new(PeerTransportKind::Tcp, addr)
    }

    pub const fn utp(addr: SocketAddr) -> Self {
        Self::new(PeerTransportKind::Utp, addr)
    }

    pub fn key(&self) -> String {
        format!("{}://{}", self.kind, self.addr)
    }
}

impl fmt::Display for PeerEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}://{}", self.kind, self.addr)
    }
}

pub trait PeerIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> PeerIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub type PeerStream = Box<dyn PeerIo + 'static>;

pub struct PeerConnection {
    pub endpoint: PeerEndpoint,
    pub remote_addr: SocketAddr,
    pub direction: PeerConnectionDirection,
    pub stream: PeerStream,
    network_generation_id: Option<u64>,
    network_scope_id: Option<NetworkScopeId>,
    network_invalidation_rx: Option<watch::Receiver<bool>>,
}

impl PeerConnection {
    pub fn new<S>(
        stream: S,
        endpoint: PeerEndpoint,
        remote_addr: SocketAddr,
        direction: PeerConnectionDirection,
    ) -> Self
    where
        S: PeerIo + 'static,
    {
        Self {
            endpoint,
            remote_addr,
            direction,
            stream: Box::new(stream),
            network_generation_id: None,
            network_scope_id: None,
            network_invalidation_rx: None,
        }
    }

    pub(crate) fn with_network_lease(mut self, network_lease: &NetworkLease) -> Self {
        self.network_generation_id = Some(network_lease.generation_id());
        self.network_scope_id = NetworkScopeId::from_lease(network_lease);
        self.network_invalidation_rx = Some(network_lease.subscribe_invalidation());
        self
    }

    pub(crate) fn network_generation_id(&self) -> Option<u64> {
        self.network_generation_id
    }

    pub(crate) fn network_scope_id(&self) -> Option<NetworkScopeId> {
        self.network_scope_id
    }

    pub fn subscribe_network_invalidation(&self) -> Option<watch::Receiver<bool>> {
        self.network_invalidation_rx.clone()
    }

    pub fn tcp(
        stream: TcpStream,
        remote_addr: SocketAddr,
        direction: PeerConnectionDirection,
    ) -> Self {
        Self::new(
            stream,
            PeerEndpoint::tcp(remote_addr),
            remote_addr,
            direction,
        )
    }

    pub fn peer_id(&self) -> String {
        self.transport_key()
    }

    pub fn transport_key(&self) -> String {
        self.endpoint.key()
    }
}

pub struct TcpPeerTransport;

impl TcpPeerTransport {
    pub async fn connect(lease: &NetworkLease, addr: SocketAddr) -> io::Result<PeerConnection> {
        let stream = lease.connect_tcp(addr).await?;
        Ok(
            PeerConnection::tcp(stream, addr, PeerConnectionDirection::Outgoing)
                .with_network_lease(lease),
        )
    }

    pub fn incoming(stream: TcpStream, remote_addr: SocketAddr) -> PeerConnection {
        PeerConnection::tcp(stream, remote_addr, PeerConnectionDirection::Incoming)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_key_includes_transport_kind() {
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();

        assert_eq!(PeerEndpoint::tcp(addr).key(), "tcp://127.0.0.1:6881");
        assert_eq!(
            PeerEndpoint::new(PeerTransportKind::Utp, addr).key(),
            "utp://127.0.0.1:6881"
        );
        assert_ne!(
            PeerEndpoint::tcp(addr),
            PeerEndpoint::new(PeerTransportKind::Quic, addr)
        );
    }

    #[test]
    fn endpoint_display_includes_transport_kind() {
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();

        assert_eq!(PeerEndpoint::tcp(addr).to_string(), "tcp://127.0.0.1:6881");
    }

    #[test]
    fn peer_connection_id_includes_transport_kind() {
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let stream = tokio::io::duplex(64).0;
        let connection = PeerConnection::new(
            stream,
            PeerEndpoint::utp(addr),
            addr,
            PeerConnectionDirection::Incoming,
        );

        assert_eq!(connection.peer_id(), "utp://127.0.0.1:6881");
    }

    #[tokio::test]
    async fn network_lease_tags_peer_connection_with_generation() {
        let (handle, supervisor_task) =
            crate::networking::runtime::NetworkSupervisor::spawn_unrestricted().unwrap();
        let lease = handle.try_lease().expect("network lease");
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let connection = PeerConnection::new(
            tokio::io::duplex(64).0,
            PeerEndpoint::tcp(addr),
            addr,
            PeerConnectionDirection::Incoming,
        )
        .with_network_lease(&lease);

        assert_eq!(
            connection.network_generation_id(),
            Some(lease.generation_id())
        );

        handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }
}
