// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    InterfaceAddressFamily, InterfaceSnapshot, NetworkFamilyHostPolicy, NetworkInterfaceInfo,
};
use socket2::Socket;
use std::ffi::CStr;
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::os::windows::io::AsRawSocket;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, HANDLE, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, GetAdaptersAddresses, GetIpInterfaceEntry, GetUnicastIpAddressEntry,
    InitializeIpInterfaceEntry, InitializeUnicastIpAddressEntry, NotifyIpInterfaceChange,
    NotifyUnicastIpAddressChange, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    IP_ADAPTER_RECEIVE_ONLY, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE, MIB_UNICASTIPADDRESS_ROW,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    getsockopt, setsockopt, IpDadStatePreferred, WSAGetLastError, ADDRESS_FAMILY, AF_INET,
    AF_INET6, AF_UNSPEC, IN6_ADDR, IN_ADDR, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF,
    IP_UNICAST_IF, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_INET, SOCKET_ADDRESS, SOCKET_ERROR,
    SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
};

const INITIAL_ADAPTER_BUFFER_SIZE: usize = 15_000;
const MAX_ADAPTER_DISCOVERY_ATTEMPTS: usize = 3;

struct NetworkChangeCallbackState {
    sender: tokio::sync::mpsc::UnboundedSender<()>,
}

pub(super) struct NetworkChangeNotifier {
    unicast_handle: HANDLE,
    interface_handle: HANDLE,
    callback_state: *mut NetworkChangeCallbackState,
}

// The OS owns callback execution while registrations are active. Drop cancels both
// registrations before reclaiming callback_state, so moving the guard is safe.
unsafe impl Send for NetworkChangeNotifier {}

impl NetworkChangeNotifier {
    pub(super) fn new() -> io::Result<(Self, tokio::sync::mpsc::UnboundedReceiver<()>)> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let callback_state = Box::into_raw(Box::new(NetworkChangeCallbackState { sender }));
        let mut unicast_handle = null_mut();
        let unicast_result = unsafe {
            NotifyUnicastIpAddressChange(
                AF_UNSPEC,
                Some(unicast_change_callback),
                callback_state.cast(),
                false,
                &mut unicast_handle,
            )
        };
        if unicast_result != NO_ERROR {
            unsafe { drop(Box::from_raw(callback_state)) };
            return Err(windows_error(
                "NotifyUnicastIpAddressChange",
                unicast_result,
            ));
        }

        let mut interface_handle = null_mut();
        let interface_result = unsafe {
            NotifyIpInterfaceChange(
                AF_UNSPEC,
                Some(interface_change_callback),
                callback_state.cast(),
                false,
                &mut interface_handle,
            )
        };
        if interface_result != NO_ERROR {
            let cancel_result = unsafe { CancelMibChangeNotify2(unicast_handle) };
            if cancel_result == NO_ERROR {
                unsafe { drop(Box::from_raw(callback_state)) };
            } else {
                // If cancellation cannot be confirmed, retain the allocation so a
                // late callback can never dereference reclaimed state.
                tracing::warn!(
                    windows_error = cancel_result,
                    "failed to cancel partial Windows network change registration; retaining callback state"
                );
            }
            return Err(windows_error("NotifyIpInterfaceChange", interface_result));
        }

        Ok((
            Self {
                unicast_handle,
                interface_handle,
                callback_state,
            },
            receiver,
        ))
    }
}

impl Drop for NetworkChangeNotifier {
    fn drop(&mut self) {
        let mut all_cancelled = true;
        for (operation, handle) in [
            ("unicast", self.unicast_handle),
            ("interface", self.interface_handle),
        ] {
            let result = unsafe { CancelMibChangeNotify2(handle) };
            if result != NO_ERROR {
                all_cancelled = false;
                tracing::warn!(
                    windows_error = result,
                    operation,
                    "failed to cancel Windows network change notification"
                );
            }
        }
        if all_cancelled {
            unsafe { drop(Box::from_raw(self.callback_state)) };
        } else {
            tracing::warn!(
                "retaining Windows network change callback state because cancellation was not confirmed"
            );
        }
    }
}

unsafe extern "system" fn unicast_change_callback(
    caller_context: *const std::ffi::c_void,
    _row: *const MIB_UNICASTIPADDRESS_ROW,
    _notification_type: MIB_NOTIFICATION_TYPE,
) {
    signal_network_change(caller_context);
}

unsafe extern "system" fn interface_change_callback(
    caller_context: *const std::ffi::c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _notification_type: MIB_NOTIFICATION_TYPE,
) {
    signal_network_change(caller_context);
}

unsafe fn signal_network_change(caller_context: *const std::ffi::c_void) {
    if let Some(state) = caller_context.cast::<NetworkChangeCallbackState>().as_ref() {
        let _ = state.sender.send(());
    }
}

#[derive(Debug, Clone)]
struct WindowsAdapter {
    identity: String,
    display_name: String,
    ipv4_index: Option<NonZeroU32>,
    ipv6_index: Option<NonZeroU32>,
    is_up: bool,
    is_loopback: bool,
    receive_only: bool,
    ipv4_addresses: Vec<Ipv4Addr>,
    ipv6_addresses: Vec<Ipv6Addr>,
    ipv4_host_policy: NetworkFamilyHostPolicy,
    ipv6_host_policy: NetworkFamilyHostPolicy,
}

impl WindowsAdapter {
    fn can_source_outbound(&self) -> bool {
        self.is_up && !self.receive_only
    }
}

pub(super) fn interface_snapshot(identity: &str) -> io::Result<InterfaceSnapshot> {
    let adapter = discover_adapters()?
        .into_iter()
        .find(|adapter| adapter.identity == identity)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Windows adapter {identity} was not found"),
            )
        })?;

    if !adapter.is_up {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("Windows adapter {identity} is not operational"),
        ));
    }
    if adapter.is_loopback {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Windows adapter {identity} is a loopback adapter"),
        ));
    }
    if adapter.receive_only {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Windows adapter {identity} is receive-only"),
        ));
    }

    Ok(InterfaceSnapshot {
        identity: Arc::from(adapter.identity),
        display_name: Arc::from(adapter.display_name),
        ipv4: InterfaceAddressFamily {
            interface_index: adapter.ipv4_index,
            eligible_sources: adapter.ipv4_addresses,
            host_policy: adapter.ipv4_host_policy,
        },
        ipv6: InterfaceAddressFamily {
            interface_index: adapter.ipv6_index,
            eligible_sources: adapter.ipv6_addresses,
            host_policy: adapter.ipv6_host_policy,
        },
    })
}

pub(super) fn available_network_interfaces() -> io::Result<Vec<NetworkInterfaceInfo>> {
    let mut interfaces = discover_adapters()?
        .into_iter()
        .map(|adapter| {
            let can_source_outbound = adapter.can_source_outbound();
            NetworkInterfaceInfo {
                identity: adapter.identity,
                display_name: adapter.display_name,
                ipv4_index: adapter.ipv4_index.map(NonZeroU32::get),
                ipv6_index: adapter.ipv6_index.map(NonZeroU32::get),
                is_up: can_source_outbound,
                is_loopback: adapter.is_loopback,
                ipv4_addresses: adapter.ipv4_addresses,
                ipv6_addresses: adapter.ipv6_addresses,
            }
        })
        .collect::<Vec<_>>();
    interfaces.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
            .then_with(|| left.identity.cmp(&right.identity))
    });
    Ok(interfaces)
}

pub(super) fn all_interface_addresses() -> io::Result<Vec<IpAddr>> {
    Ok(outbound_interface_addresses(discover_adapters()?))
}

fn outbound_interface_addresses(adapters: impl IntoIterator<Item = WindowsAdapter>) -> Vec<IpAddr> {
    let mut addresses = adapters
        .into_iter()
        .filter(WindowsAdapter::can_source_outbound)
        .flat_map(|adapter| {
            adapter
                .ipv4_addresses
                .into_iter()
                .map(IpAddr::V4)
                .chain(adapter.ipv6_addresses.into_iter().map(IpAddr::V6))
        })
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

fn discover_adapters() -> io::Result<Vec<WindowsAdapter>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut required_size = INITIAL_ADAPTER_BUFFER_SIZE as u32;

    for _ in 0..MAX_ADAPTER_DISCOVERY_ATTEMPTS {
        let word_count = (required_size as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0usize; word_count];
        let result = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC as u32,
                flags,
                null(),
                buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &mut required_size,
            )
        };
        if result == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if result != NO_ERROR {
            return Err(windows_error("GetAdaptersAddresses", result));
        }

        let mut adapters = Vec::new();
        let mut current = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !current.is_null() {
            let adapter = unsafe { &*current };
            adapters.push(unsafe { copy_adapter(adapter)? });
            current = adapter.Next;
        }
        return Ok(adapters);
    }

    Err(io::Error::other(
        "GetAdaptersAddresses buffer size did not stabilize",
    ))
}

unsafe fn copy_adapter(adapter: &IP_ADAPTER_ADDRESSES_LH) -> io::Result<WindowsAdapter> {
    let identity = c_string(adapter.AdapterName, "adapter identity")?;
    let display_name = wide_string(adapter.FriendlyName)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| identity.clone());
    let ipv4_index = NonZeroU32::new(adapter.Anonymous1.Anonymous.IfIndex);
    let ipv6_index = NonZeroU32::new(adapter.Ipv6IfIndex);
    let flags = adapter.Anonymous2.Flags;
    let is_up = adapter.OperStatus == IfOperStatusUp;
    let is_loopback = adapter.IfType == IF_TYPE_SOFTWARE_LOOPBACK;
    let receive_only = flags & IP_ADAPTER_RECEIVE_ONLY != 0;

    let mut ipv4_addresses = Vec::new();
    let mut ipv6_addresses = Vec::new();
    let mut unicast = adapter.FirstUnicastAddress;
    while !unicast.is_null() {
        let entry = &*unicast;
        if let Some(address) = socket_address_to_ip(&entry.Address).map(super::normalize_ip_address)
        {
            match address {
                IpAddr::V4(address) => {
                    if let Some(index) = ipv4_index {
                        if unicast_is_eligible(IpAddr::V4(address), index.get())? {
                            ipv4_addresses.push(address);
                        }
                    }
                }
                IpAddr::V6(address) => {
                    if let Some(index) = ipv6_index {
                        if unicast_is_eligible(IpAddr::V6(address), index.get())? {
                            ipv6_addresses.push(address);
                        }
                    }
                }
            }
        }
        unicast = entry.Next;
    }
    ipv4_addresses.sort_unstable();
    ipv4_addresses.dedup();
    ipv6_addresses.sort_unstable();
    ipv6_addresses.dedup();

    Ok(WindowsAdapter {
        identity,
        display_name,
        ipv4_index,
        ipv6_index,
        is_up,
        is_loopback,
        receive_only,
        ipv4_addresses,
        ipv6_addresses,
        ipv4_host_policy: ipv4_index
            .map(|index| read_host_policy(AF_INET, index.get()))
            .unwrap_or_default(),
        ipv6_host_policy: ipv6_index
            .map(|index| read_host_policy(AF_INET6, index.get()))
            .unwrap_or_default(),
    })
}

unsafe fn unicast_is_eligible(address: IpAddr, interface_index: u32) -> io::Result<bool> {
    let mut row: MIB_UNICASTIPADDRESS_ROW = zeroed();
    InitializeUnicastIpAddressEntry(&mut row);
    row.Address = sockaddr_inet(address);
    row.InterfaceIndex = interface_index;
    let result = GetUnicastIpAddressEntry(&mut row);
    if result != NO_ERROR {
        return Err(windows_error("GetUnicastIpAddressEntry", result));
    }

    Ok(unicast_row_is_eligible(address, interface_index, &row))
}

fn unicast_row_is_eligible(
    address: IpAddr,
    interface_index: u32,
    row: &MIB_UNICASTIPADDRESS_ROW,
) -> bool {
    row.InterfaceIndex == interface_index
        && row.DadState == IpDadStatePreferred
        && row.ValidLifetime != 0
        && row.PreferredLifetime != 0
        && !row.SkipAsSource
        && eligible_source_address(address)
}

fn eligible_source_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_unspecified()
                && !address.is_loopback()
                && !address.is_multicast()
                && !address.is_broadcast()
        }
        IpAddr::V6(address) => {
            !address.is_unspecified()
                && !address.is_loopback()
                && !address.is_multicast()
                && !address.is_unicast_link_local()
        }
    }
}

unsafe fn read_host_policy(
    family: ADDRESS_FAMILY,
    interface_index: u32,
) -> NetworkFamilyHostPolicy {
    let mut row: MIB_IPINTERFACE_ROW = zeroed();
    InitializeIpInterfaceEntry(&mut row);
    row.Family = family;
    row.InterfaceIndex = interface_index;
    if GetIpInterfaceEntry(&mut row) != NO_ERROR {
        return NetworkFamilyHostPolicy::default();
    }
    NetworkFamilyHostPolicy {
        weak_host_send: Some(row.WeakHostSend),
        weak_host_receive: Some(row.WeakHostReceive),
    }
}

unsafe fn socket_address_to_ip(address: &SOCKET_ADDRESS) -> Option<IpAddr> {
    if address.lpSockaddr.is_null() || address.iSockaddrLength < size_of::<ADDRESS_FAMILY>() as i32
    {
        return None;
    }
    match (*address.lpSockaddr).sa_family {
        AF_INET if address.iSockaddrLength as usize >= size_of::<SOCKADDR_IN>() => {
            let address = &*address.lpSockaddr.cast::<SOCKADDR_IN>();
            Some(IpAddr::V4(Ipv4Addr::from(
                address.sin_addr.S_un.S_addr.to_ne_bytes(),
            )))
        }
        AF_INET6 if address.iSockaddrLength as usize >= size_of::<SOCKADDR_IN6>() => {
            let address = &*address.lpSockaddr.cast::<SOCKADDR_IN6>();
            Some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.u.Byte)))
        }
        _ => None,
    }
}

unsafe fn sockaddr_inet(address: IpAddr) -> SOCKADDR_INET {
    match address {
        IpAddr::V4(address) => SOCKADDR_INET {
            Ipv4: SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: 0,
                sin_addr: IN_ADDR {
                    S_un: windows_sys::Win32::Networking::WinSock::IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(address.octets()),
                    },
                },
                sin_zero: [0; 8],
            },
        },
        IpAddr::V6(address) => SOCKADDR_INET {
            Ipv6: SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: IN6_ADDR {
                    u: windows_sys::Win32::Networking::WinSock::IN6_ADDR_0 {
                        Byte: address.octets(),
                    },
                },
                Anonymous: Default::default(),
            },
        },
    }
}

unsafe fn c_string(value: *mut u8, field: &'static str) -> io::Result<String> {
    if value.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Windows {field} was null"),
        ));
    }
    CStr::from_ptr(value.cast())
        .to_str()
        .map(str::to_owned)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

unsafe fn wide_string(value: *mut u16) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let mut length = 0usize;
    while *value.add(length) != 0 {
        length += 1;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(
        value, length,
    )))
}

pub(super) fn validate_host_policy(
    family: &'static str,
    policy: NetworkFamilyHostPolicy,
) -> io::Result<()> {
    match (policy.weak_host_send, policy.weak_host_receive) {
        (Some(false), Some(false)) => Ok(()),
        (Some(true), _) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{family} WeakHostSend is enabled for the selected Windows adapter"),
        )),
        (_, Some(true)) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{family} WeakHostReceive is enabled for the selected Windows adapter"),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{family} strong-host policy could not be established for the selected Windows adapter"),
        )),
    }
}

pub(super) fn select_effective_source<T: Copy>(configured: Option<T>, eligible: &[T]) -> Option<T> {
    configured.or_else(|| eligible.first().copied())
}

pub(super) fn select_effective_ipv4_source(
    configured: Option<Ipv4Addr>,
    eligible: &[Ipv4Addr],
) -> Option<Ipv4Addr> {
    configured.or_else(|| {
        eligible
            .iter()
            .copied()
            .find(|address| !address.is_link_local())
            .or_else(|| eligible.first().copied())
    })
}

pub(super) fn apply_interface_binding(
    socket: &Socket,
    addr: SocketAddr,
    interface_index: NonZeroU32,
) -> io::Result<()> {
    let (level, option, encoded) = if addr.is_ipv4() {
        (
            IPPROTO_IP,
            IP_UNICAST_IF,
            encode_ipv4_interface_index(interface_index.get()),
        )
    } else {
        (IPPROTO_IPV6, IPV6_UNICAST_IF, interface_index.get())
    };
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            level,
            option,
            (&encoded as *const u32).cast::<u8>(),
            size_of::<u32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        return Err(winsock_error("setsockopt interface binding"));
    }

    let mut readback = 0u32;
    let mut readback_length = size_of::<u32>() as i32;
    let result = unsafe {
        getsockopt(
            socket.as_raw_socket() as usize,
            level,
            option,
            (&mut readback as *mut u32).cast::<u8>(),
            &mut readback_length,
        )
    };
    if result == SOCKET_ERROR {
        return Err(winsock_error("getsockopt interface binding"));
    }
    if readback_length != size_of::<u32>() as i32 || readback != interface_index.get() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Windows interface binding readback mismatch: expected {}, received {readback}",
                interface_index.get()
            ),
        ));
    }
    Ok(())
}

pub(super) fn set_exclusive_address_use(socket: &Socket) -> io::Result<()> {
    let enabled = 1u32;
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&enabled as *const u32).cast::<u8>(),
            size_of::<u32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        Err(winsock_error("setsockopt SO_EXCLUSIVEADDRUSE"))
    } else {
        Ok(())
    }
}

fn encode_ipv4_interface_index(index: u32) -> u32 {
    index.to_be()
}

fn windows_error(operation: &str, code: u32) -> io::Error {
    io::Error::other(format!("{operation} failed with Windows error {code}"))
}

fn winsock_error(operation: &str) -> io::Error {
    let error = unsafe { WSAGetLastError() };
    io::Error::other(format!("{operation} failed with Winsock error {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_interface_index_is_encoded_in_network_byte_order() {
        assert_eq!(
            encode_ipv4_interface_index(0x0102_0304),
            0x0102_0304u32.to_be()
        );
    }

    #[test]
    fn source_eligibility_rejects_unsafe_address_classes() {
        assert!(!eligible_source_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!eligible_source_address(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!eligible_source_address(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!eligible_source_address(IpAddr::V6(
            "fe80::1".parse().unwrap()
        )));
        assert!(eligible_source_address(IpAddr::V4(
            "192.0.2.1".parse().unwrap()
        )));
        assert!(eligible_source_address(IpAddr::V6(
            "2001:db8::1".parse().unwrap()
        )));
    }

    #[test]
    fn weak_or_unknown_host_policy_is_rejected() {
        assert!(validate_host_policy(
            "IPv4",
            NetworkFamilyHostPolicy {
                weak_host_send: Some(false),
                weak_host_receive: Some(false),
            }
        )
        .is_ok());
        for policy in [
            NetworkFamilyHostPolicy::default(),
            NetworkFamilyHostPolicy {
                weak_host_send: Some(true),
                weak_host_receive: Some(false),
            },
            NetworkFamilyHostPolicy {
                weak_host_send: Some(false),
                weak_host_receive: Some(true),
            },
        ] {
            assert!(validate_host_policy("IPv4", policy).is_err());
        }
    }

    #[test]
    fn unicast_row_policy_requires_preferred_live_non_skipped_source() {
        let address = IpAddr::V4("192.0.2.8".parse().unwrap());
        let mut row = MIB_UNICASTIPADDRESS_ROW {
            InterfaceIndex: 17,
            DadState: IpDadStatePreferred,
            ValidLifetime: 60,
            PreferredLifetime: 30,
            ..Default::default()
        };
        assert!(unicast_row_is_eligible(address, 17, &row));
        row.SkipAsSource = true;
        assert!(!unicast_row_is_eligible(address, 17, &row));
        row.SkipAsSource = false;
        row.PreferredLifetime = 0;
        assert!(!unicast_row_is_eligible(address, 17, &row));
        row.PreferredLifetime = 30;
        row.DadState = windows_sys::Win32::Networking::WinSock::IpDadStateTentative;
        assert!(!unicast_row_is_eligible(address, 17, &row));
    }

    #[test]
    fn automatic_ipv4_source_selection_uses_sorted_first_address() {
        let eligible = [
            "192.0.2.8".parse::<Ipv4Addr>().unwrap(),
            "192.0.2.9".parse::<Ipv4Addr>().unwrap(),
        ];
        assert_eq!(
            select_effective_ipv4_source(None, &eligible),
            Some(eligible[0])
        );
        assert_eq!(
            select_effective_ipv4_source(Some(eligible[1]), &eligible),
            Some(eligible[1])
        );
    }

    #[test]
    fn automatic_ipv4_source_selection_prefers_non_link_local_addresses() {
        let link_local = "169.254.10.8".parse::<Ipv4Addr>().unwrap();
        let routable = "192.0.2.8".parse::<Ipv4Addr>().unwrap();

        assert_eq!(
            select_effective_ipv4_source(None, &[link_local, routable]),
            Some(routable)
        );
        assert_eq!(
            select_effective_ipv4_source(None, &[link_local]),
            Some(link_local)
        );
        assert_eq!(
            select_effective_ipv4_source(Some(link_local), &[link_local, routable]),
            Some(link_local)
        );
    }

    #[test]
    fn local_address_candidates_exclude_receive_only_adapters() {
        let outbound_address = "192.0.2.8".parse::<Ipv4Addr>().unwrap();
        let receive_only_address = "198.51.100.8".parse::<Ipv4Addr>().unwrap();
        let adapter = |identity: &str, address, receive_only| WindowsAdapter {
            identity: identity.to_owned(),
            display_name: identity.to_owned(),
            ipv4_index: None,
            ipv6_index: None,
            is_up: true,
            is_loopback: false,
            receive_only,
            ipv4_addresses: vec![address],
            ipv6_addresses: Vec::new(),
            ipv4_host_policy: NetworkFamilyHostPolicy::default(),
            ipv6_host_policy: NetworkFamilyHostPolicy::default(),
        };

        let addresses = outbound_interface_addresses([
            adapter("outbound", outbound_address, false),
            adapter("receive-only", receive_only_address, true),
        ]);

        assert_eq!(addresses, vec![IpAddr::V4(outbound_address)]);
    }

    #[test]
    fn native_adapter_discovery_returns_stable_identities() {
        let adapters = discover_adapters().expect("discover Windows adapters");
        assert!(!adapters.is_empty());
        let mut identities = adapters
            .iter()
            .map(|adapter| adapter.identity.as_str())
            .collect::<Vec<_>>();
        identities.sort_unstable();
        identities.dedup();
        assert_eq!(identities.len(), adapters.len());
        assert!(adapters.iter().all(|adapter| !adapter.identity.is_empty()));
    }

    #[test]
    fn native_socket_option_readback_matches_a_live_index_when_available() {
        let adapters = discover_adapters().expect("discover Windows adapters");
        let candidate = adapters
            .iter()
            .filter(|adapter| adapter.is_up && !adapter.is_loopback && !adapter.receive_only)
            .find_map(|adapter| {
                adapter
                    .ipv4_index
                    .zip(adapter.ipv4_addresses.first().copied())
                    .map(|(index, address)| (index, IpAddr::V4(address)))
                    .or_else(|| {
                        adapter
                            .ipv6_index
                            .zip(adapter.ipv6_addresses.first().copied())
                            .map(|(index, address)| (index, IpAddr::V6(address)))
                    })
            });
        let Some((index, address)) = candidate else {
            return;
        };
        let socket = Socket::new(
            if address.is_ipv4() {
                socket2::Domain::IPV4
            } else {
                socket2::Domain::IPV6
            },
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .expect("create native option probe socket");
        apply_interface_binding(&socket, SocketAddr::new(address, 0), index)
            .expect("apply and read back native interface option");
    }
}
