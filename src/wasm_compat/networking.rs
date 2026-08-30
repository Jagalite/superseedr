// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code, unused_imports)]

//! WASM compatibility for networking data shown by production presentation modules.
//!
//! No sockets, DNS resolver, listener, transport, or supervisor is implemented here.

use serde::{Deserialize, Serialize};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

pub mod transport {
    use std::fmt;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum PeerTransportKind {
        Tcp,
        Utp,
        Quic,
    }

    impl PeerTransportKind {
        pub const fn as_scheme(self) -> &'static str {
            match self {
                Self::Tcp => "tcp",
                Self::Utp => "utp",
                Self::Quic => "quic",
            }
        }
    }

    impl fmt::Display for PeerTransportKind {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.as_scheme())
        }
    }
}

pub mod runtime {
    pub use super::{
        local_address_is_assigned_to_host, normalize_socket_addr, DnsPolicy, NetworkBindingConfig,
        NetworkBindingMode, NetworkInterfaceInfo, NetworkRuntimePhase, NetworkRuntimeStatus,
        DUAL_FAMILY_EXACT_SOURCE_SUPPORTED, INTERFACE_BINDING_SUPPORTED,
    };
}

pub const INTERFACE_BINDING_SUPPORTED: bool = false;
pub const DUAL_FAMILY_EXACT_SOURCE_SUPPORTED: bool = false;

pub fn normalize_socket_addr(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(address) => address
            .ip()
            .to_ipv4_mapped()
            .map(|ipv4| SocketAddr::new(IpAddr::V4(ipv4), address.port()))
            .unwrap_or(SocketAddr::V6(address)),
        address => address,
    }
}

pub fn local_address_is_assigned_to_host(address: IpAddr) -> io::Result<bool> {
    if address.is_loopback() {
        Ok(true)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "local-address discovery is unavailable in the browser renderer",
        ))
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkBindingMode {
    #[default]
    Any,
    Interface,
    LocalAddress,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DnsPolicy {
    #[default]
    System,
    Bound,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkRuntimePhase {
    Ready,
    Blocked,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkFamilyHostPolicy {
    pub weak_host_send: Option<bool>,
    pub weak_host_receive: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NetworkRuntimeStatus {
    pub phase: NetworkRuntimePhase,
    pub mode: NetworkBindingMode,
    pub interface: Option<String>,
    #[serde(default)]
    pub interface_display_name: Option<String>,
    #[serde(default)]
    pub ipv4_interface_index: Option<u32>,
    #[serde(default)]
    pub ipv6_interface_index: Option<u32>,
    pub enable_ipv4: bool,
    pub enable_ipv6: bool,
    #[serde(default)]
    pub configured_ipv4_address: Option<Ipv4Addr>,
    #[serde(default)]
    pub configured_ipv6_address: Option<Ipv6Addr>,
    pub selected_ipv4_address: Option<Ipv4Addr>,
    pub selected_ipv6_address: Option<Ipv6Addr>,
    pub interface_ipv4_addresses: Vec<Ipv4Addr>,
    pub interface_ipv6_addresses: Vec<Ipv6Addr>,
    #[serde(default)]
    pub ipv4_host_policy: NetworkFamilyHostPolicy,
    #[serde(default)]
    pub ipv6_host_policy: NetworkFamilyHostPolicy,
    pub dns_policy: DnsPolicy,
    pub dns_servers: Vec<SocketAddr>,
    pub generation_id: Option<u64>,
    pub config_epoch: Option<u64>,
    pub blocked_reason: Option<String>,
    pub warning: Option<String>,
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
    pub dns_policy: DnsPolicy,
    pub dns_servers: Vec<SocketAddr>,
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
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkInterfaceInfo {
    pub identity: String,
    pub display_name: String,
    pub ipv4_index: Option<u32>,
    pub ipv6_index: Option<u32>,
    pub is_up: bool,
    pub is_loopback: bool,
    pub ipv4_addresses: Vec<Ipv4Addr>,
    pub ipv6_addresses: Vec<Ipv6Addr>,
}

impl NetworkInterfaceInfo {
    pub fn is_selectable(&self) -> bool {
        self.is_up
            && !self.is_loopback
            && (!self.ipv4_addresses.is_empty() || !self.ipv6_addresses.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkActivationStatus {
    Pending {
        generation_id: Option<u64>,
    },
    Active {
        generation_id: u64,
        listen_port: u16,
    },
    Blocked {
        reason: Arc<str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkScopeId {
    generation_id: u64,
    activation_id: u64,
}

#[derive(Debug, Clone, Default)]
pub struct NetworkActivationHandle;

#[derive(Debug)]
pub struct PeerConnection;
