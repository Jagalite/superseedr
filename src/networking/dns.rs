// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::networking::runtime::SocketFactory;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::error::Error;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time;

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const HEADER_LEN: usize = 12;
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

#[derive(Debug, Clone)]
pub(crate) struct BoundDnsResolver {
    factory: SocketFactory,
    servers: Arc<[SocketAddr]>,
    ipv4: bool,
    ipv6: bool,
    invalidation_rx: watch::Receiver<bool>,
    next_id: Arc<AtomicU64>,
}

impl BoundDnsResolver {
    pub(crate) fn new(
        factory: SocketFactory,
        servers: Vec<SocketAddr>,
        ipv4: bool,
        ipv6: bool,
        invalidation_rx: watch::Receiver<bool>,
    ) -> io::Result<Self> {
        if servers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bound DNS requires at least one literal DNS server address",
            ));
        }
        if servers.iter().any(|server| server.port() == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bound DNS server ports must be non-zero",
            ));
        }
        if !servers
            .iter()
            .any(|server| (server.is_ipv4() && ipv4) || (server.is_ipv6() && ipv6))
        {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "bound DNS has no server on an enabled address family",
            ));
        }
        Ok(Self {
            factory,
            servers: Arc::from(servers),
            ipv4,
            ipv6,
            invalidation_rx,
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub(crate) async fn resolve_ips(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        self.ensure_valid()?;
        if let Ok(address) = host.parse::<IpAddr>() {
            let enabled = match address {
                IpAddr::V4(_) => self.ipv4,
                IpAddr::V6(_) => self.ipv6,
            };
            return enabled.then_some(vec![address]).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "literal address family is disabled by the network binding policy",
                )
            });
        }
        let ipv4 = async {
            if self.ipv4 {
                self.query(host, TYPE_A).await
            } else {
                Ok(Vec::new())
            }
        };
        let ipv6 = async {
            if self.ipv6 {
                self.query(host, TYPE_AAAA).await
            } else {
                Ok(Vec::new())
            }
        };
        let (ipv4, ipv6) = tokio::join!(ipv4, ipv6);
        let mut addresses = Vec::new();
        let mut last_error = None;
        match ipv4 {
            Ok(found) => addresses.extend(found),
            Err(error) => last_error = Some(error),
        }
        match ipv6 {
            Ok(found) => addresses.extend(found),
            Err(error) => last_error = Some(error),
        }
        self.ensure_valid()?;
        addresses.sort_unstable();
        addresses.dedup();
        if addresses.is_empty() {
            Err(last_error.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no DNS records found for {host}"),
                )
            }))
        } else {
            Ok(addresses)
        }
    }

    async fn query(&self, host: &str, query_type: u16) -> io::Result<Vec<IpAddr>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) as u16;
        let packet = encode_query(id, host, query_type)?;
        let mut last_error = None;
        for server in self
            .servers
            .iter()
            .copied()
            .filter(|server| (server.is_ipv4() && self.ipv4) || (server.is_ipv6() && self.ipv6))
        {
            match self.query_server(server, &packet, id, query_type).await {
                Ok(addresses) if !addresses.is_empty() => return Ok(addresses),
                Ok(_) => {}
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no DNS records found for {host}"),
            )
        }))
    }

    async fn query_server(
        &self,
        server: SocketAddr,
        packet: &[u8],
        id: u16,
        query_type: u16,
    ) -> io::Result<Vec<IpAddr>> {
        let response = time::timeout(QUERY_TIMEOUT, self.query_udp(server, packet))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "bound DNS UDP query timed out")
            })??;
        let parsed = parse_response(&response, id, query_type)?;
        if !parsed.truncated {
            return Ok(parsed.addresses);
        }
        let response = time::timeout(QUERY_TIMEOUT, self.query_tcp(server, packet))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "bound DNS TCP query timed out")
            })??;
        Ok(parse_response(&response, id, query_type)?.addresses)
    }

    async fn query_udp(&self, server: SocketAddr, packet: &[u8]) -> io::Result<Vec<u8>> {
        let bind_addr = if server.is_ipv4() {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        };
        let socket = self.factory.bind_udp(bind_addr)?;
        self.cancel_on(socket.connect(server)).await?;
        self.cancel_on(socket.send(packet)).await?;
        let mut response = vec![0_u8; 4_096];
        let len = self.cancel_on(socket.recv(&mut response)).await?;
        response.truncate(len);
        Ok(response)
    }

    async fn query_tcp(&self, server: SocketAddr, packet: &[u8]) -> io::Result<Vec<u8>> {
        let mut stream = self.cancel_on(self.factory.connect_tcp(server)).await?;
        let packet_len = u16::try_from(packet.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "DNS query exceeds TCP framing limit",
            )
        })?;
        self.cancel_on(stream.write_all(&packet_len.to_be_bytes()))
            .await?;
        self.cancel_on(stream.write_all(packet)).await?;
        let mut length = [0_u8; 2];
        self.cancel_on(stream.read_exact(&mut length)).await?;
        let mut response = vec![0_u8; usize::from(u16::from_be_bytes(length))];
        self.cancel_on(stream.read_exact(&mut response)).await?;
        Ok(response)
    }

    async fn cancel_on<T>(
        &self,
        operation: impl std::future::Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        let mut invalidation_rx = self.invalidation_rx.clone();
        if *invalidation_rx.borrow() {
            return Err(invalidated());
        }
        tokio::select! {
            biased;
            _ = invalidation_rx.changed() => Err(invalidated()),
            result = operation => result,
        }
    }

    fn ensure_valid(&self) -> io::Result<()> {
        if *self.invalidation_rx.borrow() {
            Err(invalidated())
        } else {
            Ok(())
        }
    }
}

impl Resolve for BoundDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = resolver
                .resolve_ips(&host)
                .await
                .map_err(|error| -> Box<dyn Error + Send + Sync> { Box::new(error) })?;
            let addresses: Addrs = Box::new(
                addresses
                    .into_iter()
                    .map(|address| SocketAddr::new(address, 0)),
            );
            Ok(addresses)
        })
    }
}

fn invalidated() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "network generation was invalidated during bound DNS resolution",
    )
}

fn encode_query(id: u16, host: &str, query_type: u16) -> io::Result<Vec<u8>> {
    let host = host.trim_end_matches('.');
    if host.is_empty() || host.len() > 253 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid DNS name",
        ));
    }
    let mut packet = Vec::with_capacity(HEADER_LEN + host.len() + 6);
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&0x0100_u16.to_be_bytes());
    packet.extend_from_slice(&1_u16.to_be_bytes());
    packet.extend_from_slice(&[0; 6]);
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid DNS label",
            ));
        }
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&query_type.to_be_bytes());
    packet.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(packet)
}

struct ParsedResponse {
    truncated: bool,
    addresses: Vec<IpAddr>,
}

fn parse_response(packet: &[u8], id: u16, query_type: u16) -> io::Result<ParsedResponse> {
    if packet.len() < HEADER_LEN || read_u16(packet, 0)? != id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid DNS response",
        ));
    }
    let flags = read_u16(packet, 2)?;
    if flags & 0x8000 == 0 || flags & 0x000f != 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "DNS server returned an error",
        ));
    }
    let mut offset = HEADER_LEN;
    for _ in 0..usize::from(read_u16(packet, 4)?) {
        offset = skip_name(packet, offset)?;
        offset = checked_end(packet, offset, 4, "truncated DNS question")?;
    }
    let mut addresses = Vec::new();
    for _ in 0..usize::from(read_u16(packet, 6)?) {
        offset = skip_name(packet, offset)?;
        let record_type = read_u16(packet, offset)?;
        let class = read_u16(packet, offset + 2)?;
        let data_len = usize::from(read_u16(packet, offset + 8)?);
        offset = checked_end(packet, offset, 10, "truncated DNS record")?;
        let end = checked_end(packet, offset, data_len, "truncated DNS record data")?;
        if class == CLASS_IN && record_type == query_type {
            match (record_type, data_len) {
                (TYPE_A, 4) => addresses.push(IpAddr::V4(Ipv4Addr::new(
                    packet[offset],
                    packet[offset + 1],
                    packet[offset + 2],
                    packet[offset + 3],
                ))),
                (TYPE_AAAA, 16) => {
                    let mut octets = [0_u8; 16];
                    octets.copy_from_slice(&packet[offset..end]);
                    addresses.push(IpAddr::V6(Ipv6Addr::from(octets)));
                }
                _ => {}
            }
        }
        offset = end;
    }
    Ok(ParsedResponse {
        truncated: flags & 0x0200 != 0,
        addresses,
    })
}

fn skip_name(packet: &[u8], mut offset: usize) -> io::Result<usize> {
    loop {
        let length = *packet
            .get(offset)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated DNS name"))?;
        if length & 0xc0 == 0xc0 {
            return checked_end(packet, offset, 2, "truncated DNS name pointer");
        }
        offset += 1;
        if length == 0 {
            return Ok(offset);
        }
        if length & 0xc0 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid DNS label",
            ));
        }
        offset = checked_end(packet, offset, usize::from(length), "truncated DNS label")?;
    }
}

fn read_u16(packet: &[u8], offset: usize) -> io::Result<u16> {
    let bytes = packet
        .get(offset..offset + 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated DNS integer"))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn checked_end(packet: &[u8], offset: usize, len: usize, message: &str) -> io::Result<usize> {
    offset
        .checked_add(len)
        .filter(|end| *end <= packet.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::networking::runtime::{DnsPolicy, NetworkBindingConfig, NetworkBindingMode};
    use tokio::net::{TcpListener, UdpSocket};

    #[test]
    fn parses_ipv4_answer_with_compressed_name() {
        let mut response = encode_query(7, "resolver.test", TYPE_A).unwrap();
        response[2..4].copy_from_slice(&0x8180_u16.to_be_bytes());
        response[6..8].copy_from_slice(&1_u16.to_be_bytes());
        response.extend_from_slice(&[0xc0, 0x0c]);
        response.extend_from_slice(&TYPE_A.to_be_bytes());
        response.extend_from_slice(&CLASS_IN.to_be_bytes());
        response.extend_from_slice(&60_u32.to_be_bytes());
        response.extend_from_slice(&4_u16.to_be_bytes());
        response.extend_from_slice(&[127, 0, 0, 1]);

        let parsed = parse_response(&response, 7, TYPE_A).unwrap();
        assert_eq!(parsed.addresses, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
        assert!(!parsed.truncated);
    }

    #[test]
    fn rejects_invalid_dns_labels() {
        let host = format!("{}.test", "x".repeat(64));
        assert!(encode_query(1, &host, TYPE_A).is_err());
    }

    #[tokio::test]
    async fn resolves_through_the_configured_bound_udp_server() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let mut query = vec![0_u8; 512];
            let (len, peer) = server.recv_from(&mut query).await.unwrap();
            query.truncate(len);
            server
                .send_to(&ipv4_response(query, false), peer)
                .await
                .unwrap();
        });
        let (invalidation_tx, invalidation_rx) = watch::channel(false);
        let resolver = BoundDnsResolver::new(
            local_ipv4_factory(),
            vec![server_addr],
            true,
            false,
            invalidation_rx,
        )
        .unwrap();

        assert_eq!(
            resolver.resolve_ips("resolver.test").await.unwrap(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        );
        assert!(!*invalidation_tx.borrow());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_udp_response_falls_back_to_bound_tcp_dns() {
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let server_addr = tcp.local_addr().unwrap();
        let udp = UdpSocket::bind(server_addr).await.unwrap();
        let udp_task = tokio::spawn(async move {
            let mut query = vec![0_u8; 512];
            let (len, peer) = udp.recv_from(&mut query).await.unwrap();
            query.truncate(len);
            udp.send_to(&ipv4_response(query, true), peer)
                .await
                .unwrap();
        });
        let tcp_task = tokio::spawn(async move {
            let (mut stream, _) = tcp.accept().await.unwrap();
            let mut length = [0_u8; 2];
            stream.read_exact(&mut length).await.unwrap();
            let mut query = vec![0_u8; usize::from(u16::from_be_bytes(length))];
            stream.read_exact(&mut query).await.unwrap();
            let response = ipv4_response(query, false);
            stream
                .write_all(&(response.len() as u16).to_be_bytes())
                .await
                .unwrap();
            stream.write_all(&response).await.unwrap();
        });
        let (_invalidation_tx, invalidation_rx) = watch::channel(false);
        let resolver = BoundDnsResolver::new(
            local_ipv4_factory(),
            vec![server_addr],
            true,
            false,
            invalidation_rx,
        )
        .unwrap();

        assert_eq!(
            resolver.resolve_ips("resolver.test").await.unwrap(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        );
        udp_task.await.unwrap();
        tcp_task.await.unwrap();
    }

    #[tokio::test]
    async fn invalidated_generation_rejects_bound_dns_before_socket_creation() {
        let (invalidation_tx, invalidation_rx) = watch::channel(false);
        let resolver = BoundDnsResolver::new(
            local_ipv4_factory(),
            vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 53))],
            true,
            false,
            invalidation_rx,
        )
        .unwrap();
        invalidation_tx.send_replace(true);

        let error = resolver.resolve_ips("resolver.test").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    fn local_ipv4_factory() -> SocketFactory {
        SocketFactory::from_config(&NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        })
        .unwrap()
    }

    fn ipv4_response(mut query: Vec<u8>, truncated: bool) -> Vec<u8> {
        let flags = if truncated { 0x8380_u16 } else { 0x8180_u16 };
        query[2..4].copy_from_slice(&flags.to_be_bytes());
        query[6..8].copy_from_slice(&u16::from(!truncated).to_be_bytes());
        if !truncated {
            query.extend_from_slice(&[0xc0, 0x0c]);
            query.extend_from_slice(&TYPE_A.to_be_bytes());
            query.extend_from_slice(&CLASS_IN.to_be_bytes());
            query.extend_from_slice(&60_u32.to_be_bytes());
            query.extend_from_slice(&4_u16.to_be_bytes());
            query.extend_from_slice(&[127, 0, 0, 1]);
        }
        query
    }
}
