// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod dns;
pub mod protocol;
pub mod runtime;
pub mod session;
pub mod shared_udp;
pub mod transport;
pub mod utp;
pub mod web_seed_worker;

// Re-export key types for easier access.
pub use protocol::BlockInfo;
pub use runtime::{
    available_network_interfaces, DnsPolicy, NetworkBindingConfig, NetworkBindingMode,
    NetworkHandle, NetworkInterfaceInfo, NetworkLease, NetworkRuntimeStatus, NetworkState,
    NetworkSupervisor,
};
pub use session::{ConnectionType, PeerSession};
pub use transport::{PeerConnection, TcpPeerTransport};
pub use utp::{UtpListenerSet, UtpPeerTransport};
