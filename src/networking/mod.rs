// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod model;

pub mod activation;
#[cfg(not(target_arch = "wasm32"))]
pub mod dns;
pub mod protocol;
#[cfg(not(target_arch = "wasm32"))]
pub mod runtime;
pub mod session;
#[cfg(not(target_arch = "wasm32"))]
pub mod shared_udp;
#[cfg(not(target_arch = "wasm32"))]
pub mod transport;
#[cfg(not(target_arch = "wasm32"))]
pub mod utp;
#[cfg(not(target_arch = "wasm32"))]
pub mod web_seed_worker;

pub(crate) use model::normalize_socket_addr;
pub use model::{
    DnsPolicy, NetworkActivationStatus, NetworkBindingConfig, NetworkBindingMode,
    NetworkInterfaceInfo, NetworkRuntimePhase, NetworkRuntimeStatus, NetworkScopeId,
    PeerTransportKind, DUAL_FAMILY_EXACT_SOURCE_SUPPORTED, INTERFACE_BINDING_SUPPORTED,
};

// Re-export key types for easier access.
pub use activation::{
    NetworkActivationHandle, NetworkActivationPublisher, NetworkActivationState, NetworkScope,
    Scoped,
};
pub use protocol::BlockInfo;
#[cfg(not(target_arch = "wasm32"))]
pub use runtime::{
    available_network_interfaces, NetworkHandle, NetworkLease, NetworkState, NetworkSupervisor,
};
pub use session::{ConnectionType, PeerSession};
#[cfg(not(target_arch = "wasm32"))]
pub use transport::{PeerConnection, TcpPeerTransport};
#[cfg(not(target_arch = "wasm32"))]
pub use utp::{UtpListenerSet, UtpPeerTransport};

#[cfg(feature = "webtorrent")]
pub mod webtorrent;
