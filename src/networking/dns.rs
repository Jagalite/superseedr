// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::networking::runtime::{normalize_ip_address, normalize_socket_addr, SocketFactory};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time;

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const HEADER_LEN: usize = 12;
const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;
const MAX_CNAME_DEPTH: usize = 8;
const MAX_NAME_POINTER_JUMPS: usize = 16;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemDnsResolver;

impl Resolve for SystemDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = tokio::task::spawn_blocking(move || {
                (host.as_str(), 0).to_socket_addrs().map(|addresses| {
                    let addresses: Addrs = Box::new(addresses);
                    addresses
                })
            })
            .await
            .map_err(|error| -> Box<dyn Error + Send + Sync> { Box::new(error) })?
            .map_err(|error| -> Box<dyn Error + Send + Sync> { Box::new(error) })?;
            Ok(addresses)
        })
    }
}

#[derive(Clone)]
pub(crate) struct FamilyFilteringResolver {
    inner: NetworkDnsResolver,
    ipv4: bool,
    ipv6: bool,
}

impl FamilyFilteringResolver {
    pub(crate) fn new(inner: NetworkDnsResolver, ipv4: bool, ipv6: bool) -> Self {
        Self { inner, ipv4, ipv6 }
    }
}

#[derive(Clone)]
pub(crate) enum NetworkDnsResolver {
    System(SystemDnsResolver),
    Bound(Arc<BoundDnsResolver>),
    #[cfg(test)]
    Fixed(Vec<SocketAddr>),
}

impl Resolve for NetworkDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        match self {
            Self::System(resolver) => resolver.resolve(name),
            Self::Bound(resolver) => resolver.resolve(name),
            #[cfg(test)]
            Self::Fixed(addresses) => {
                let addresses = addresses.clone();
                Box::pin(async move { Ok(Box::new(addresses.into_iter()) as Addrs) })
            }
        }
    }
}

impl Resolve for FamilyFilteringResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolving = self.inner.resolve(name);
        let ipv4 = self.ipv4;
        let ipv6 = self.ipv6;
        Box::pin(async move {
            let addresses: Vec<_> = resolving
                .await?
                .map(normalize_socket_addr)
                .filter(|address| (address.is_ipv4() && ipv4) || (address.is_ipv6() && ipv6))
                .collect();
            if addresses.is_empty() {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "DNS returned no addresses on an enabled address family",
                )) as Box<dyn Error + Send + Sync>);
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BoundDnsResolver {
    factory: SocketFactory,
    servers: Arc<[SocketAddr]>,
    ipv4: bool,
    ipv6: bool,
    invalidation_rx: watch::Receiver<bool>,
}

impl BoundDnsResolver {
    pub(crate) fn new(
        factory: SocketFactory,
        servers: Vec<SocketAddr>,
        ipv4: bool,
        ipv6: bool,
        invalidation_rx: watch::Receiver<bool>,
    ) -> io::Result<Self> {
        let servers: Vec<_> = servers.into_iter().map(normalize_socket_addr).collect();
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
        })
    }

    pub(crate) async fn resolve_ips(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        self.ensure_valid()?;
        if let Ok(address) = host.parse::<IpAddr>().map(normalize_ip_address) {
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
        addresses = addresses
            .into_iter()
            .map(normalize_ip_address)
            .filter(|address| match address {
                IpAddr::V4(_) => self.ipv4,
                IpAddr::V6(_) => self.ipv6,
            })
            .collect();
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
        let mut query_name = host.trim_end_matches('.').to_ascii_lowercase();
        let mut visited = HashSet::new();
        for alias_depth in 0..=MAX_CNAME_DEPTH {
            if !visited.insert(query_name.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "DNS CNAME response contains an alias cycle",
                ));
            }
            let id = random_transaction_id();
            let packet = encode_query(id, &query_name, query_type)?;
            let mut last_error = None;
            let mut next_name = None;
            for server in
                self.servers.iter().copied().filter(|server| {
                    (server.is_ipv4() && self.ipv4) || (server.is_ipv6() && self.ipv6)
                })
            {
                match self.query_server(server, &packet, id, query_type).await {
                    Ok(parsed) if !parsed.addresses.is_empty() => return Ok(parsed.addresses),
                    Ok(parsed) if parsed.next_name.is_some() => {
                        next_name = parsed.next_name;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => last_error = Some(error),
                }
            }
            if let Some(next_name) = next_name {
                if alias_depth == MAX_CNAME_DEPTH {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "DNS CNAME response exceeds the supported alias depth",
                    ));
                }
                query_name = next_name;
                continue;
            }
            return Err(last_error.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no DNS records found for {query_name}"),
                )
            }));
        }
        unreachable!("bounded CNAME loop must return")
    }

    async fn query_server(
        &self,
        server: SocketAddr,
        packet: &[u8],
        id: u16,
        query_type: u16,
    ) -> io::Result<ParsedResponse> {
        let response = time::timeout(QUERY_TIMEOUT, self.query_udp(server, packet))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "bound DNS UDP query timed out")
            })??;
        let parsed = parse_response(&response, id, query_type)?;
        if !parsed.truncated {
            return Ok(parsed);
        }
        let response = time::timeout(QUERY_TIMEOUT, self.query_tcp(server, packet))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "bound DNS TCP query timed out")
            })??;
        parse_response(&response, id, query_type)
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

fn random_transaction_id() -> u16 {
    rand::random()
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

#[derive(Debug)]
struct ParsedResponse {
    truncated: bool,
    addresses: Vec<IpAddr>,
    next_name: Option<String>,
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
    let mut query_name = None;
    for _ in 0..usize::from(read_u16(packet, 4)?) {
        let (name, next_offset) = decode_name(packet, offset)?;
        offset = next_offset;
        let question_type = read_u16(packet, offset)?;
        let question_class = read_u16(packet, offset + 2)?;
        offset = checked_end(packet, offset, 4, "truncated DNS question")?;
        if query_name.is_none() && question_type == query_type && question_class == CLASS_IN {
            query_name = Some(name);
        }
    }
    let query_name = query_name.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "DNS response does not contain the requested question",
        )
    })?;
    let answer_count = usize::from(read_u16(packet, 6)?);
    let authority_count = usize::from(read_u16(packet, 8)?);
    let additional_count = usize::from(read_u16(packet, 10)?);
    let record_count = answer_count
        .checked_add(authority_count)
        .and_then(|count| count.checked_add(additional_count))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "too many DNS records"))?;
    let mut addresses_by_name: HashMap<String, Vec<IpAddr>> = HashMap::new();
    let mut aliases = HashMap::new();
    for _ in 0..record_count {
        let (owner, next_offset) = decode_name(packet, offset)?;
        offset = next_offset;
        let record_type = read_u16(packet, offset)?;
        let class = read_u16(packet, offset + 2)?;
        let data_len = usize::from(read_u16(packet, offset + 8)?);
        offset = checked_end(packet, offset, 10, "truncated DNS record")?;
        let end = checked_end(packet, offset, data_len, "truncated DNS record data")?;
        if class == CLASS_IN {
            match (record_type, data_len) {
                (TYPE_CNAME, _) => {
                    let (target, name_end) = decode_name(packet, offset)?;
                    if name_end != end {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid DNS CNAME record data",
                        ));
                    }
                    aliases.entry(owner).or_insert(target);
                }
                (TYPE_A, 4) if query_type == TYPE_A => addresses_by_name
                    .entry(owner)
                    .or_default()
                    .push(IpAddr::V4(Ipv4Addr::new(
                        packet[offset],
                        packet[offset + 1],
                        packet[offset + 2],
                        packet[offset + 3],
                    ))),
                (TYPE_AAAA, 16) if query_type == TYPE_AAAA => {
                    let mut octets = [0_u8; 16];
                    octets.copy_from_slice(&packet[offset..end]);
                    addresses_by_name
                        .entry(owner)
                        .or_default()
                        .push(IpAddr::V6(Ipv6Addr::from(octets)));
                }
                _ => {}
            }
        }
        offset = end;
    }
    let mut current_name = query_name.clone();
    let mut visited = HashSet::new();
    for alias_depth in 0..=MAX_CNAME_DEPTH {
        if !visited.insert(current_name.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DNS CNAME response contains an alias cycle",
            ));
        }
        if let Some(addresses) = addresses_by_name.remove(&current_name) {
            return Ok(ParsedResponse {
                truncated: flags & 0x0200 != 0,
                addresses,
                next_name: None,
            });
        }
        let Some(target) = aliases.get(&current_name) else {
            return Ok(ParsedResponse {
                truncated: flags & 0x0200 != 0,
                addresses: Vec::new(),
                next_name: (current_name != query_name).then_some(current_name),
            });
        };
        if alias_depth == MAX_CNAME_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DNS CNAME response exceeds the supported alias depth",
            ));
        }
        current_name = target.clone();
    }
    unreachable!("bounded CNAME traversal must return")
}

fn decode_name(packet: &[u8], offset: usize) -> io::Result<(String, usize)> {
    let mut cursor = offset;
    let mut next_offset = None;
    let mut labels = Vec::new();
    let mut visited_pointers = HashSet::new();
    let mut pointer_jumps = 0;
    loop {
        let length = *packet
            .get(cursor)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated DNS name"))?;
        if length & 0xc0 == 0xc0 {
            let pointer_end = checked_end(packet, cursor, 2, "truncated DNS name pointer")?;
            let pointer = (usize::from(length & 0x3f) << 8) | usize::from(packet[cursor + 1]);
            if pointer >= packet.len()
                || !visited_pointers.insert(pointer)
                || pointer_jumps == MAX_NAME_POINTER_JUMPS
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid DNS name compression pointer",
                ));
            }
            next_offset.get_or_insert(pointer_end);
            cursor = pointer;
            pointer_jumps += 1;
            continue;
        }
        if length & 0xc0 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid DNS label",
            ));
        }
        cursor += 1;
        if length == 0 {
            let next_offset = next_offset.unwrap_or(cursor);
            let name = labels.join(".");
            if name.len() > 253 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "DNS name exceeds the supported length",
                ));
            }
            return Ok((name, next_offset));
        }
        let label_end = checked_end(packet, cursor, usize::from(length), "truncated DNS label")?;
        let label = std::str::from_utf8(&packet[cursor..label_end]).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "DNS label is not valid UTF-8")
        })?;
        labels.push(label.to_ascii_lowercase());
        cursor = label_end;
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

    #[tokio::test]
    async fn family_filtering_resolver_omits_disabled_address_family() {
        let resolver = FamilyFilteringResolver::new(
            NetworkDnsResolver::Fixed(vec![
                SocketAddr::from(([192, 0, 2, 10], 0)),
                SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 10], 0)),
            ]),
            true,
            false,
        );

        let addresses: Vec<_> = resolver
            .resolve("peer.test".parse().expect("valid test hostname"))
            .await
            .expect("resolve enabled family")
            .collect();

        assert_eq!(addresses, vec![SocketAddr::from(([192, 0, 2, 10], 0))]);
    }

    #[tokio::test]
    async fn family_filtering_resolver_rejects_results_only_on_disabled_family() {
        let resolver = FamilyFilteringResolver::new(
            NetworkDnsResolver::Fixed(vec![SocketAddr::from((
                [0x2001, 0xdb8, 0, 0, 0, 0, 0, 10],
                0,
            ))]),
            true,
            false,
        );

        let result = resolver
            .resolve("peer.test".parse().expect("valid test hostname"))
            .await;
        let error = match result {
            Ok(_) => panic!("disabled family must not be returned"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("enabled address family"));
    }

    #[test]
    fn bound_dns_transaction_ids_are_not_a_generation_local_sequence() {
        let ids: Vec<_> = (0..8).map(|_| random_transaction_id()).collect();

        assert_ne!(ids, (1_u16..=8).collect::<Vec<_>>());
    }

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
    fn follows_cname_to_an_address_in_the_additional_section() {
        let mut response = encode_query(8, "resolver.test", TYPE_A).unwrap();
        response[2..4].copy_from_slice(&0x8180_u16.to_be_bytes());
        response[6..8].copy_from_slice(&1_u16.to_be_bytes());
        response[10..12].copy_from_slice(&1_u16.to_be_bytes());
        append_cname_record(&mut response, &[0xc0, 0x0c], "target.resolver.test");
        append_ipv4_record(
            &mut response,
            &encoded_name("target.resolver.test"),
            Ipv4Addr::new(192, 0, 2, 25),
        );

        let parsed = parse_response(&response, 8, TYPE_A).unwrap();
        assert_eq!(
            parsed.addresses,
            vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 25))]
        );
        assert_eq!(parsed.next_name, None);
    }

    #[test]
    fn rejects_a_cname_cycle_in_one_response() {
        let mut response = encode_query(9, "resolver.test", TYPE_A).unwrap();
        response[2..4].copy_from_slice(&0x8180_u16.to_be_bytes());
        response[6..8].copy_from_slice(&1_u16.to_be_bytes());
        append_cname_record(&mut response, &[0xc0, 0x0c], "resolver.test");

        let error = parse_response(&response, 9, TYPE_A).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("alias cycle"));
    }

    #[test]
    fn rejects_a_cname_chain_beyond_the_supported_depth() {
        let mut response = encode_query(10, "alias0.resolver.test", TYPE_A).unwrap();
        response[2..4].copy_from_slice(&0x8180_u16.to_be_bytes());
        response[6..8].copy_from_slice(&((MAX_CNAME_DEPTH + 1) as u16).to_be_bytes());
        for alias in 0..=MAX_CNAME_DEPTH {
            append_cname_record(
                &mut response,
                &encoded_name(&format!("alias{alias}.resolver.test")),
                &format!("alias{}.resolver.test", alias + 1),
            );
        }

        let error = parse_response(&response, 10, TYPE_A).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("alias depth"));
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
    async fn follows_a_cname_only_response_with_a_second_bound_query() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let mut query = vec![0_u8; 512];
            let (len, peer) = server.recv_from(&mut query).await.unwrap();
            query.truncate(len);
            server
                .send_to(&cname_only_response(query, "target.resolver.test"), peer)
                .await
                .unwrap();

            let mut query = vec![0_u8; 512];
            let (len, peer) = server.recv_from(&mut query).await.unwrap();
            query.truncate(len);
            server
                .send_to(&ipv4_response(query, false), peer)
                .await
                .unwrap();
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

    fn cname_only_response(mut query: Vec<u8>, target: &str) -> Vec<u8> {
        query[2..4].copy_from_slice(&0x8180_u16.to_be_bytes());
        query[6..8].copy_from_slice(&1_u16.to_be_bytes());
        append_cname_record(&mut query, &[0xc0, 0x0c], target);
        query
    }

    fn append_cname_record(packet: &mut Vec<u8>, owner: &[u8], target: &str) {
        packet.extend_from_slice(owner);
        packet.extend_from_slice(&TYPE_CNAME.to_be_bytes());
        packet.extend_from_slice(&CLASS_IN.to_be_bytes());
        packet.extend_from_slice(&60_u32.to_be_bytes());
        let target = encoded_name(target);
        packet.extend_from_slice(&(target.len() as u16).to_be_bytes());
        packet.extend_from_slice(&target);
    }

    fn append_ipv4_record(packet: &mut Vec<u8>, owner: &[u8], address: Ipv4Addr) {
        packet.extend_from_slice(owner);
        packet.extend_from_slice(&TYPE_A.to_be_bytes());
        packet.extend_from_slice(&CLASS_IN.to_be_bytes());
        packet.extend_from_slice(&60_u32.to_be_bytes());
        packet.extend_from_slice(&4_u16.to_be_bytes());
        packet.extend_from_slice(&address.octets());
    }

    fn encoded_name(name: &str) -> Vec<u8> {
        let mut encoded = Vec::new();
        for label in name.split('.') {
            encoded.push(label.len() as u8);
            encoded.extend_from_slice(label.as_bytes());
        }
        encoded.push(0);
        encoded
    }
}
