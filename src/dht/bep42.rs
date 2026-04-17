// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::types::{Bep42State, NodeId};
use crc::{Crc, CRC_32_ISCSI};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

const IPV4_MASK: u32 = 0x030f3fff;
const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

pub fn classify_node(addr: SocketAddr, node_id: Option<NodeId>) -> Bep42State {
    let Some(node_id) = node_id else {
        return Bep42State::Unknown;
    };

    match addr {
        SocketAddr::V4(addr) => classify_ipv4(*addr.ip(), node_id),
        SocketAddr::V6(addr) => classify_ipv6(*addr.ip(), node_id),
    }
}

pub fn classify_ipv4(ip: Ipv4Addr, node_id: NodeId) -> Bep42State {
    if ipv4_is_exempt(ip) {
        return Bep42State::ExemptLocal;
    }

    let expected = first_21_bits(&id_prefix_ipv4(ip, node_id.as_array()[NodeId::LEN - 1]));
    if node_id.first_21_bits() == expected {
        Bep42State::Compliant
    } else {
        Bep42State::NonCompliant
    }
}

pub fn is_secure_public_candidate(
    addr: SocketAddr,
    node_id: Option<NodeId>,
    bep42_state: Bep42State,
) -> bool {
    matches!(addr, SocketAddr::V4(addr_v4) if !ipv4_is_exempt(*addr_v4.ip()))
        && node_id.is_some()
        && matches!(bep42_state, Bep42State::Compliant)
}

pub fn same_public_identity_group(
    left_addr: SocketAddr,
    left_node_id: Option<NodeId>,
    left_state: Bep42State,
    right_addr: SocketAddr,
    right_node_id: Option<NodeId>,
    right_state: Bep42State,
) -> bool {
    let (SocketAddr::V4(left_addr), SocketAddr::V4(right_addr)) = (left_addr, right_addr) else {
        return false;
    };
    if left_addr.ip() != right_addr.ip() {
        return false;
    }
    if ipv4_is_exempt(*left_addr.ip()) || ipv4_is_exempt(*right_addr.ip()) {
        return false;
    }

    let left_secure = is_secure_public_candidate(left_addr.into(), left_node_id, left_state);
    let right_secure = is_secure_public_candidate(right_addr.into(), right_node_id, right_state);
    if !left_secure || !right_secure {
        return true;
    }

    match (left_node_id, right_node_id) {
        (Some(left_node_id), Some(right_node_id)) => {
            left_node_id.first_21_bits() == right_node_id.first_21_bits()
        }
        _ => true,
    }
}

fn classify_ipv6(ip: Ipv6Addr, _node_id: NodeId) -> Bep42State {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local() || ip.is_unicast_link_local() {
        Bep42State::ExemptLocal
    } else {
        Bep42State::Unknown
    }
}

fn ipv4_is_exempt(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_link_local()
        || ip.is_loopback()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_documentation()
}

fn first_21_bits(bytes: &[u8]) -> [u8; 3] {
    [bytes[0], bytes[1], bytes[2] & 0xf8]
}

fn id_prefix_ipv4(ip: Ipv4Addr, r: u8) -> [u8; 3] {
    let r32: u32 = r.into();
    let ip_int = u32::from_be_bytes(ip.octets());
    let masked_ip = (ip_int & IPV4_MASK) | (r32 << 29);

    let mut digest = CASTAGNOLI.digest();
    digest.update(&masked_ip.to_be_bytes());

    let crc = digest.finalize();
    [crc.to_be_bytes()[0], crc.to_be_bytes()[1], crc.to_be_bytes()[2]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_known_bep42_vector() {
        let addr: SocketAddr = "124.31.75.21:6881".parse().expect("ipv4 socket");
        let node_id = NodeId::try_from(
            &hex::decode("5fbfbff10c5d6a4ec8a88e4c6ab4c28b95eee401")
                .expect("hex node id")[..],
        )
        .expect("node id");

        assert_eq!(classify_node(addr, Some(node_id)), Bep42State::Compliant);
    }

    #[test]
    fn marks_loopback_ipv4_as_exempt() {
        let addr: SocketAddr = "127.0.0.1:6881".parse().expect("ipv4 socket");
        let node_id = NodeId::from([1u8; 20]);

        assert_eq!(classify_node(addr, Some(node_id)), Bep42State::ExemptLocal);
    }
}
