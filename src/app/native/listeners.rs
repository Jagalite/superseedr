// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native peer listener ownership and transport acquisition.

use super::*;

pub struct ListenerSet {
    pub(super) accept_rx: tokio::sync::Mutex<mpsc::Receiver<io::Result<PeerConnection>>>,
    pub(super) accept_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) local_port: u16,
    #[cfg(test)]
    pub(super) ipv4_bound: bool,
    #[cfg(test)]
    pub(super) ipv6_bound: bool,
    #[cfg(test)]
    pub(super) utp_bound: bool,
}

pub(super) const PEER_TRANSPORT_ENV: &str = "SUPERSEEDR_PEER_TRANSPORT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeerListenerTransportMode {
    Tcp,
    Utp,
    All,
}

pub(super) fn tcp_peer_listener_enabled_from_env() -> bool {
    tcp_peer_listener_enabled(peer_listener_transport_mode_from_env())
}

pub(super) fn tcp_peer_listener_enabled(mode: PeerListenerTransportMode) -> bool {
    matches!(
        mode,
        PeerListenerTransportMode::Tcp | PeerListenerTransportMode::All
    )
}

pub(super) fn utp_peer_listener_enabled_from_env() -> bool {
    matches!(
        peer_listener_transport_mode_from_env(),
        PeerListenerTransportMode::Utp | PeerListenerTransportMode::All
    )
}

pub(super) fn peer_listener_transport_mode_from_env() -> PeerListenerTransportMode {
    match std::env::var(PEER_TRANSPORT_ENV) {
        Ok(value) => peer_listener_transport_mode(&value),
        Err(_) => PeerListenerTransportMode::All,
    }
}

pub(super) fn peer_listener_transport_mode(value: &str) -> PeerListenerTransportMode {
    match value.to_ascii_lowercase().as_str() {
        "tcp" => PeerListenerTransportMode::Tcp,
        "utp" => PeerListenerTransportMode::Utp,
        "all" => PeerListenerTransportMode::All,
        _ => PeerListenerTransportMode::All,
    }
}

#[cfg(test)]
pub(super) async fn bind_peer_listener(
    network_handle: &NetworkHandle,
    port: u16,
) -> io::Result<Option<ListenerSet>> {
    let lease = network_handle.try_lease().map_err(io::Error::other)?;
    bind_peer_listener_with_lease(&lease, port).await
}

pub(super) async fn bind_peer_listener_with_lease(
    lease: &NetworkLease,
    port: u16,
) -> io::Result<Option<ListenerSet>> {
    lease.ensure_valid().map_err(io::Error::other)?;
    let tcp_enabled = tcp_peer_listener_enabled_from_env();
    let utp_enabled = utp_peer_listener_enabled_from_env();
    if !tcp_enabled && !utp_enabled {
        tracing_event!(
            Level::INFO,
            "Peer listener disabled because TCP is disabled and uTP is not enabled"
        );
        return Ok(None);
    }

    let listener = ListenerSet::bind(lease, port, tcp_enabled, utp_enabled).await?;
    lease.ensure_valid().map_err(io::Error::other)?;
    Ok(Some(listener))
}

impl ListenerSet {
    pub(super) async fn bind(
        network_lease: &NetworkLease,
        port: u16,
        tcp_enabled: bool,
        utp_enabled: bool,
    ) -> io::Result<Self> {
        let (ipv4, ipv6) = if tcp_enabled {
            bind_tcp_peer_listeners(network_lease, port).await?
        } else {
            tracing_event!(
                Level::INFO,
                "TCP peer listener disabled by peer transport mode"
            );
            (None, None)
        };

        let udp_port = match (
            port,
            ipv4.as_ref()
                .or(ipv6.as_ref())
                .and_then(|listener| listener.local_addr().ok().map(|addr| addr.port())),
        ) {
            (0, Some(bound_port)) => bound_port,
            _ => port,
        };
        let utp = if utp_enabled {
            match UtpPeerTransport::bind_listener(network_lease, udp_port).await {
                Ok(listener) => Some(listener),
                Err(error) if ipv4.is_some() || ipv6.is_some() => {
                    tracing_event!(
                        Level::WARN,
                        error = %error,
                        "uTP listener bind failed; continuing with TCP listener only."
                    );
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };

        if ipv4.is_none() && ipv6.is_none() && utp.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "failed to bind any peer listener",
            ));
        }

        let local_port = ipv4
            .as_ref()
            .or(ipv6.as_ref())
            .and_then(|listener| listener.local_addr().ok())
            .map(|addr| addr.port())
            .or_else(|| utp.as_ref().and_then(UtpListenerSet::local_port))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "listener has no local port",
                )
            })?;
        #[cfg(test)]
        let ipv4_bound = ipv4.is_some();
        #[cfg(test)]
        let ipv6_bound = ipv6.is_some();
        #[cfg(test)]
        let utp_bound = utp.is_some();
        let (accept_tx, accept_rx) = mpsc::channel(64);
        let network_lease = network_lease.clone();
        let accept_task = tokio::spawn(async move {
            let mut invalidation_rx = network_lease.subscribe_invalidation();
            loop {
                if *invalidation_rx.borrow() {
                    break;
                }
                let result = tokio::select! {
                    biased;
                    _ = accept_tx.closed() => break,
                    _ = invalidation_rx.changed() => break,
                    result = accept_peer_transport(&ipv4, &ipv6, utp.as_ref()) => result,
                };
                let result = result.map(|connection| connection.with_network_lease(&network_lease));
                let delivered = tokio::select! {
                    biased;
                    _ = accept_tx.closed() => false,
                    _ = invalidation_rx.changed() => false,
                    result = accept_tx.send(result) => result.is_ok(),
                };
                if !delivered {
                    break;
                }
            }
        });

        Ok(Self {
            accept_rx: tokio::sync::Mutex::new(accept_rx),
            accept_task: Some(accept_task),
            local_port,
            #[cfg(test)]
            ipv4_bound,
            #[cfg(test)]
            ipv6_bound,
            #[cfg(test)]
            utp_bound,
        })
    }

    pub(super) async fn accept(&self) -> io::Result<PeerConnection> {
        self.accept_rx
            .lock()
            .await
            .recv()
            .await
            .unwrap_or_else(|| Err(network_listener_invalidated()))
    }

    pub(super) fn local_port(&self) -> Option<u16> {
        Some(self.local_port)
    }

    pub(super) async fn shutdown(mut self) {
        if let Some(accept_task) = self.accept_task.take() {
            accept_task.abort();
            let _ = accept_task.await;
        }
    }
}

impl Drop for ListenerSet {
    fn drop(&mut self) {
        if let Some(accept_task) = self.accept_task.take() {
            accept_task.abort();
        }
    }
}

pub(super) async fn accept_peer_transport(
    ipv4: &Option<TcpListener>,
    ipv6: &Option<TcpListener>,
    utp: Option<&UtpListenerSet>,
) -> io::Result<PeerConnection> {
    let tcp_enabled = ipv4.is_some() || ipv6.is_some();
    match (tcp_enabled, utp) {
        (true, Some(utp)) => tokio::select! {
            result = accept_tcp_peer(ipv4, ipv6) => result,
            result = utp.accept() => result,
        },
        (true, None) => accept_tcp_peer(ipv4, ipv6).await,
        (false, Some(utp)) => utp.accept().await,
        (false, None) => Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no listener is currently bound",
        )),
    }
}

pub(super) async fn accept_tcp_peer(
    ipv4: &Option<TcpListener>,
    ipv6: &Option<TcpListener>,
) -> io::Result<PeerConnection> {
    let (stream, remote_addr) = match (ipv4, ipv6) {
        (Some(ipv4), Some(ipv6)) => tokio::select! {
            result = ipv4.accept() => result,
            result = ipv6.accept() => result,
        },
        (Some(ipv4), None) => ipv4.accept().await,
        (None, Some(ipv6)) => ipv6.accept().await,
        (None, None) => Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no TCP listener is currently bound",
        )),
    }?;
    Ok(TcpPeerTransport::incoming(stream, remote_addr))
}

pub(super) fn network_listener_invalidated() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "peer listener network generation was invalidated",
    )
}

pub(super) async fn wait_for_peer_network_invalidation(
    invalidation_rx: &mut Option<watch::Receiver<bool>>,
) {
    let Some(invalidation_rx) = invalidation_rx.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    if *invalidation_rx.borrow() {
        return;
    }
    let _ = invalidation_rx.changed().await;
}

pub(super) fn network_policy_warning(
    config: &crate::networking::NetworkBindingConfig,
) -> Option<String> {
    (config.mode != crate::networking::NetworkBindingMode::Any
        && config.dns_policy == crate::networking::DnsPolicy::System)
        .then(|| {
            "Strict network binding is active, but DNS uses the system resolver and is outside the application-level leak guarantee."
                .to_string()
        })
}

pub(super) async fn bind_tcp_peer_listeners(
    network_lease: &NetworkLease,
    port: u16,
) -> io::Result<(Option<TcpListener>, Option<TcpListener>)> {
    let coordinate_ephemeral_dual_stack = port == 0
        && network_lease.ipv4_enabled()
        && network_lease.ipv6_enabled()
        && !network_lease.uses_tokio_tcp_backend();
    let attempts = if coordinate_ephemeral_dual_stack {
        DUAL_STACK_EPHEMERAL_BIND_ATTEMPTS
    } else {
        1
    };

    for attempt in 1..=attempts {
        let ipv6 = match network_lease
            .bind_tcp_listener(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port))
            .await
        {
            Ok(listener) => Some(listener),
            Err(error) => {
                tracing_event!(
                    Level::WARN,
                    error = %error,
                    "IPv6 listener bind failed; continuing without IPv6 listener."
                );
                None
            }
        };

        let ipv4_port = match (port, ipv6.as_ref()) {
            (0, Some(listener)) => listener.local_addr()?.port(),
            _ => port,
        };

        let ipv4 = match network_lease
            .bind_tcp_listener(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                ipv4_port,
            ))
            .await
        {
            Ok(listener) => Some(listener),
            Err(error)
                if ipv6.is_some()
                    && coordinate_ephemeral_dual_stack
                    && error.kind() == io::ErrorKind::AddrInUse
                    && attempt < attempts =>
            {
                tracing_event!(
                    Level::DEBUG,
                    port = ipv4_port,
                    attempt,
                    "Ephemeral TCP port was unavailable to IPv4; retrying dual-stack allocation."
                );
                continue;
            }
            Err(error)
                if ipv6.is_some()
                    && coordinate_ephemeral_dual_stack
                    && error.kind() == io::ErrorKind::AddrInUse =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "failed to allocate a shared IPv4/IPv6 ephemeral TCP port after {attempts} attempts: {error}"
                    ),
                ));
            }
            Err(error) if ipv6.is_some() && error.kind() == io::ErrorKind::AddrInUse => None,
            Err(error) if ipv6.is_some() => {
                tracing_event!(
                    Level::WARN,
                    error = %error,
                    "IPv4 listener bind failed; continuing with IPv6 listener only."
                );
                None
            }
            Err(error) => return Err(error),
        };

        return Ok((ipv4, ipv6));
    }

    unreachable!("dual-stack bind loop always returns or exhausts with an error")
}
