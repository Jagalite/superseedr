// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code)]

use crate::networking::dns::{
    is_public_destination, BoundDnsResolver, FamilyFilteringResolver, NetworkDnsResolver,
    PublicFilteringResolver, SystemDnsResolver,
};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
#[cfg(any(unix, test))]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::{CStr, CString};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, TcpStream as StdTcpStream};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::{lookup_host, TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{self, Duration, MissedTickBehavior};

const SUPERVISOR_COMMAND_CAPACITY: usize = 8;
const BINDING_MONITOR_INTERVAL: Duration = Duration::from_secs(1);
const APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const GENERAL_HTTP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
#[cfg(any(target_os = "linux", test))]
const LINUX_IFA_F_OPTIMISTIC: u32 = 0x04;
#[cfg(any(target_os = "linux", test))]
const LINUX_IFA_F_DADFAILED: u32 = 0x08;
#[cfg(any(target_os = "linux", test))]
const LINUX_IFA_F_TENTATIVE: u32 = 0x40;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkBindingMode {
    #[default]
    Any,
    Interface,
    LocalAddress,
}

pub const INTERFACE_BINDING_SUPPORTED: bool = cfg!(any(
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
));

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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NetworkRuntimeStatus {
    pub phase: NetworkRuntimePhase,
    pub mode: NetworkBindingMode,
    pub interface: Option<String>,
    pub interface_index: Option<u32>,
    pub enable_ipv4: bool,
    pub enable_ipv6: bool,
    pub selected_ipv4_address: Option<Ipv4Addr>,
    pub selected_ipv6_address: Option<Ipv6Addr>,
    pub interface_ipv4_addresses: Vec<Ipv4Addr>,
    pub interface_ipv6_addresses: Vec<Ipv6Addr>,
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

    pub fn runtime_status(&self, config: &NetworkBindingConfig) -> NetworkRuntimeStatus {
        let warning = (config.mode != NetworkBindingMode::Any
            && config.dns_policy == DnsPolicy::System)
            .then(|| {
                "System DNS is outside the strict application-level leak guarantee".to_string()
            });

        match self {
            Self::Ready(generation) => {
                let binding = &generation.socket_factory.binding;
                NetworkRuntimeStatus {
                    phase: NetworkRuntimePhase::Ready,
                    mode: binding.mode,
                    interface: binding.interface_name.as_deref().map(str::to_owned),
                    interface_index: binding.interface_index.map(NonZeroU32::get),
                    enable_ipv4: binding.enable_ipv4,
                    enable_ipv6: binding.enable_ipv6,
                    selected_ipv4_address: binding.ipv4_address,
                    selected_ipv6_address: binding.ipv6_address,
                    interface_ipv4_addresses: binding.interface_ipv4_addresses.to_vec(),
                    interface_ipv6_addresses: binding.interface_ipv6_addresses.to_vec(),
                    dns_policy: config.dns_policy,
                    dns_servers: config.dns_servers.clone(),
                    generation_id: Some(generation.id()),
                    config_epoch: Some(generation.config_epoch()),
                    blocked_reason: None,
                    warning,
                }
            }
            Self::Blocked(reason) => NetworkRuntimeStatus {
                phase: NetworkRuntimePhase::Blocked,
                mode: config.mode,
                interface: config.interface.clone(),
                interface_index: None,
                enable_ipv4: config.enable_ipv4,
                enable_ipv6: config.enable_ipv6,
                selected_ipv4_address: config.ipv4_address,
                selected_ipv6_address: config.ipv6_address,
                interface_ipv4_addresses: Vec::new(),
                interface_ipv6_addresses: Vec::new(),
                dns_policy: config.dns_policy,
                dns_servers: config.dns_servers.clone(),
                generation_id: None,
                config_epoch: None,
                blocked_reason: Some(reason.to_string()),
                warning,
            },
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
    HttpRequestRejected {
        url: Arc<str>,
        reason: Arc<str>,
    },
    HttpClientUnavailable {
        purpose: &'static str,
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
            Self::HttpRequestRejected { url, reason } => {
                write!(formatter, "HTTP request to {url} rejected: {reason}")
            }
            Self::HttpClientUnavailable { purpose, reason } => {
                write!(formatter, "{purpose} HTTP client is unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for NetworkLeaseError {}

#[derive(Debug, Clone)]
pub struct NetworkHttpClient {
    client: reqwest::Client,
    ipv4: bool,
    ipv6: bool,
    public_only: bool,
}

impl NetworkHttpClient {
    fn new(client: reqwest::Client, ipv4: bool, ipv6: bool) -> Self {
        Self {
            client,
            ipv4,
            ipv6,
            public_only: false,
        }
    }

    fn public_only(mut self) -> Self {
        self.public_only = true;
        self
    }

    #[cfg(test)]
    pub(crate) fn unrestricted_test_client() -> Self {
        Self::new(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build unrestricted test HTTP client"),
            true,
            true,
        )
    }

    pub fn get(&self, url: impl AsRef<str>) -> Result<reqwest::RequestBuilder, NetworkLeaseError> {
        let url_text = url.as_ref();
        let url = reqwest::Url::parse(url_text).map_err(|error| {
            NetworkLeaseError::HttpRequestRejected {
                url: Arc::from(url_text),
                reason: Arc::from(error.to_string()),
            }
        })?;
        validate_http_url_family(&url, self.ipv4, self.ipv6)?;
        if self.public_only {
            validate_public_http_url(&url)?;
        }
        Ok(self.client.get(url))
    }
}

fn validate_public_http_url(url: &reqwest::Url) -> Result<(), NetworkLeaseError> {
    let rejected = |reason: &'static str| NetworkLeaseError::HttpRequestRejected {
        url: Arc::from(url.as_str()),
        reason: Arc::from(reason),
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(rejected("RSS URL must use HTTP or HTTPS"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(rejected("RSS URL credentials are not allowed"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| rejected("RSS URL has no host"))?;
    if host.eq_ignore_ascii_case("localhost") {
        return Err(rejected("RSS URL host is not public"));
    }
    let literal_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if literal_host
        .parse::<IpAddr>()
        .is_ok_and(|address| !is_public_destination(address))
    {
        return Err(rejected("RSS URL literal address is not public"));
    }
    Ok(())
}

fn validate_http_url_family(
    url: &reqwest::Url,
    ipv4: bool,
    ipv6: bool,
) -> Result<(), NetworkLeaseError> {
    let Some(host) = url.host_str() else {
        return Err(NetworkLeaseError::HttpRequestRejected {
            url: Arc::from(url.as_str()),
            reason: Arc::from("URL has no host"),
        });
    };
    let literal_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let Ok(address) = literal_host.parse::<IpAddr>() else {
        return Ok(());
    };
    let enabled = match normalize_ip_address(address) {
        IpAddr::V4(_) => ipv4,
        IpAddr::V6(_) => ipv6,
    };
    if enabled {
        Ok(())
    } else {
        Err(NetworkLeaseError::HttpRequestRejected {
            url: Arc::from(url.as_str()),
            reason: Arc::from("literal address uses a disabled address family"),
        })
    }
}

fn http_redirect_policy(ipv4: bool, ipv6: bool) -> reqwest::redirect::Policy {
    let default_policy = reqwest::redirect::Policy::default();
    reqwest::redirect::Policy::custom(move |attempt| {
        if let Err(error) = validate_http_url_family(attempt.url(), ipv4, ipv6) {
            attempt.error(error)
        } else {
            default_policy.redirect(attempt)
        }
    })
}

fn rss_redirect_policy(ipv4: bool, ipv6: bool) -> reqwest::redirect::Policy {
    let default_policy = reqwest::redirect::Policy::default();
    reqwest::redirect::Policy::custom(move |attempt| {
        if let Err(error) = validate_http_url_family(attempt.url(), ipv4, ipv6)
            .and_then(|()| validate_public_http_url(attempt.url()))
        {
            attempt.error(error)
        } else {
            default_policy.redirect(attempt)
        }
    })
}

#[derive(Debug)]
pub struct NetworkGeneration {
    id: u64,
    config_epoch: u64,
    socket_factory: SocketFactory,
    tracker_http_client: Result<NetworkHttpClient, Arc<str>>,
    general_http_client: Result<NetworkHttpClient, Arc<str>>,
    rss_http_client: Result<NetworkHttpClient, Arc<str>>,
    web_seed_http_client: Result<NetworkHttpClient, Arc<str>>,
    bound_dns_resolver: Option<Arc<BoundDnsResolver>>,
    invalidated: AtomicBool,
    invalidation_tx: watch::Sender<bool>,
}

impl NetworkGeneration {
    fn unrestricted(id: u64) -> io::Result<Self> {
        Self::from_config(id, 1, &NetworkBindingConfig::default())
    }

    fn from_config(id: u64, config_epoch: u64, config: &NetworkBindingConfig) -> io::Result<Self> {
        Self::from_config_with_http_client_builder(id, config_epoch, config, |builder| {
            builder.build().map_err(io::Error::other)
        })
    }

    fn from_config_with_http_client_builder(
        id: u64,
        config_epoch: u64,
        config: &NetworkBindingConfig,
        mut build_http_client: impl FnMut(reqwest::ClientBuilder) -> io::Result<reqwest::Client>,
    ) -> io::Result<Self> {
        let (invalidation_tx, _) = watch::channel(false);
        let socket_factory = SocketFactory::from_config(config)?;
        socket_factory.preflight()?;
        let http_ipv4 = socket_factory.binding.enable_ipv4;
        let http_ipv6 = socket_factory.binding.enable_ipv6;
        let bound_dns_resolver = match config.dns_policy {
            DnsPolicy::System => None,
            DnsPolicy::Bound => {
                if config.mode == NetworkBindingMode::Any {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "bound DNS requires interface or local-address binding mode",
                    ));
                }
                Some(Arc::new(BoundDnsResolver::new(
                    socket_factory.clone(),
                    config.dns_servers.clone(),
                    http_ipv4,
                    http_ipv6,
                    invalidation_tx.subscribe(),
                )?))
            }
        };
        // Preserve reqwest's native resolver for the unrestricted dual-stack default.
        // A custom resolver is needed only to enforce a family restriction or bound DNS.
        let resolver = bound_dns_resolver
            .clone()
            .map(NetworkDnsResolver::Bound)
            .or_else(|| {
                (!http_ipv4 || !http_ipv6).then_some(NetworkDnsResolver::System(SystemDnsResolver))
            })
            .map(|resolver| Arc::new(FamilyFilteringResolver::new(resolver, http_ipv4, http_ipv6)));
        let tracker_http_client = build_generation_http_client(
            socket_factory
                .configure_http_client(reqwest::Client::builder(), resolver.clone())
                .redirect(http_redirect_policy(http_ipv4, http_ipv6))
                .user_agent(APP_USER_AGENT),
            http_ipv4,
            http_ipv6,
            &mut build_http_client,
        );
        let general_http_client = build_generation_http_client(
            socket_factory
                .configure_http_client(reqwest::Client::builder(), resolver.clone())
                .redirect(http_redirect_policy(http_ipv4, http_ipv6))
                .user_agent(APP_USER_AGENT)
                .timeout(GENERAL_HTTP_REQUEST_TIMEOUT),
            http_ipv4,
            http_ipv6,
            &mut build_http_client,
        );
        let rss_resolver = Arc::new(PublicFilteringResolver::new(FamilyFilteringResolver::new(
            bound_dns_resolver
                .clone()
                .map(NetworkDnsResolver::Bound)
                .unwrap_or(NetworkDnsResolver::System(SystemDnsResolver)),
            http_ipv4,
            http_ipv6,
        )));
        let rss_http_client = build_generation_http_client(
            socket_factory
                .configure_http_transport(reqwest::Client::builder())
                .no_proxy()
                .dns_resolver(rss_resolver)
                .redirect(rss_redirect_policy(http_ipv4, http_ipv6))
                .user_agent(APP_USER_AGENT)
                .timeout(GENERAL_HTTP_REQUEST_TIMEOUT),
            http_ipv4,
            http_ipv6,
            &mut build_http_client,
        )
        .map(|client| client.public_only());
        let web_seed_http_client = build_generation_http_client(
            socket_factory
                .configure_http_client(reqwest::Client::builder(), resolver)
                .redirect(http_redirect_policy(http_ipv4, http_ipv6))
                .user_agent(APP_USER_AGENT),
            http_ipv4,
            http_ipv6,
            &mut build_http_client,
        );

        Ok(Self {
            id,
            config_epoch,
            socket_factory,
            tracker_http_client,
            general_http_client,
            rss_http_client,
            web_seed_http_client,
            bound_dns_resolver,
            invalidated: AtomicBool::new(false),
            invalidation_tx,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn config_epoch(&self) -> u64 {
        self.config_epoch
    }

    pub fn socket_factory(&self) -> &SocketFactory {
        &self.socket_factory
    }

    pub fn tracker_http_client(&self) -> Result<&NetworkHttpClient, NetworkLeaseError> {
        generation_http_client(&self.tracker_http_client, "tracker")
    }

    pub fn general_http_client(&self) -> Result<&NetworkHttpClient, NetworkLeaseError> {
        generation_http_client(&self.general_http_client, "general-purpose")
    }

    pub fn rss_http_client(&self) -> Result<&NetworkHttpClient, NetworkLeaseError> {
        generation_http_client(&self.rss_http_client, "RSS")
    }

    pub fn web_seed_http_client(&self) -> Result<&NetworkHttpClient, NetworkLeaseError> {
        generation_http_client(&self.web_seed_http_client, "web-seed")
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

fn build_generation_http_client(
    builder: reqwest::ClientBuilder,
    ipv4: bool,
    ipv6: bool,
    build_http_client: &mut impl FnMut(reqwest::ClientBuilder) -> io::Result<reqwest::Client>,
) -> Result<NetworkHttpClient, Arc<str>> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_http_client(builder))) {
        Ok(Ok(client)) => Ok(NetworkHttpClient::new(client, ipv4, ipv6)),
        Ok(Err(error)) => Err(Arc::from(error.to_string())),
        Err(_) => Err(Arc::from("HTTP client construction panicked")),
    }
}

fn generation_http_client<'a>(
    client: &'a Result<NetworkHttpClient, Arc<str>>,
    purpose: &'static str,
) -> Result<&'a NetworkHttpClient, NetworkLeaseError> {
    client
        .as_ref()
        .map_err(|reason| NetworkLeaseError::HttpClientUnavailable {
            purpose,
            reason: reason.clone(),
        })
}

impl Drop for NetworkGeneration {
    fn drop(&mut self) {
        self.invalidate();
    }
}

#[derive(Debug, Clone)]
pub struct NetworkLease {
    generation: Arc<NetworkGeneration>,
    activation_id: Option<u64>,
    activation_invalidation_rx: Option<watch::Receiver<bool>>,
}

impl NetworkLease {
    pub fn generation_id(&self) -> u64 {
        self.generation.id()
    }

    pub(crate) fn activation_id(&self) -> Option<u64> {
        self.activation_id
    }

    pub(crate) fn with_activation(
        mut self,
        activation_id: u64,
        invalidation_rx: watch::Receiver<bool>,
    ) -> Self {
        self.activation_id = Some(activation_id);
        self.activation_invalidation_rx = Some(invalidation_rx);
        self
    }

    pub fn generation(&self) -> &Arc<NetworkGeneration> {
        &self.generation
    }

    pub fn ensure_valid(&self) -> Result<(), NetworkLeaseError> {
        if self.generation.is_invalidated()
            || self
                .activation_invalidation_rx
                .as_ref()
                .is_some_and(|invalidation_rx| *invalidation_rx.borrow())
        {
            Err(NetworkLeaseError::Invalidated {
                generation_id: self.generation.id(),
            })
        } else {
            Ok(())
        }
    }

    pub fn subscribe_invalidation(&self) -> watch::Receiver<bool> {
        self.activation_invalidation_rx
            .clone()
            .unwrap_or_else(|| self.generation.subscribe_invalidation())
    }

    pub fn ipv4_enabled(&self) -> bool {
        self.generation.socket_factory.binding.enable_ipv4
    }

    pub fn ipv6_enabled(&self) -> bool {
        self.generation.socket_factory.binding.enable_ipv6
    }

    pub fn address_family_enabled(&self, address: IpAddr) -> bool {
        match normalize_ip_address(address) {
            IpAddr::V4(_) => self.ipv4_enabled(),
            IpAddr::V6(_) => self.ipv6_enabled(),
        }
    }

    /// Runs an operation only while this generation remains current.
    ///
    /// Dropping the supplied future is intentional: network operations must not
    /// outlive the generation whose source/interface policy created them.
    pub async fn cancel_on_invalidation<F, T>(&self, operation: F) -> Result<T, NetworkLeaseError>
    where
        F: Future<Output = T>,
    {
        let mut invalidation_rx = self.subscribe_invalidation();
        self.ensure_valid()?;
        let output = tokio::select! {
            biased;
            _ = wait_for_invalidation(&mut invalidation_rx) => {
                return Err(NetworkLeaseError::Invalidated {
                    generation_id: self.generation_id(),
                });
            }
            output = operation => output,
        };
        self.ensure_valid()?;
        Ok(output)
    }

    pub async fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, NetworkLeaseError> {
        let mut invalidation_rx = self.subscribe_invalidation();
        self.ensure_valid()?;
        let addresses = if let Some(resolver) = &self.generation.bound_dns_resolver {
            resolver.resolve_ips(host).await.map(|addresses| {
                addresses
                    .into_iter()
                    .map(|address| SocketAddr::new(address, port))
                    .collect()
            })
        } else {
            tokio::select! {
                biased;
                _ = wait_for_invalidation(&mut invalidation_rx) => Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "network generation was invalidated during system DNS resolution",
                )),
                result = lookup_host((host, port)) => result.map(Iterator::collect),
            }
        }
        .and_then(|addresses| {
            filter_enabled_address_families(addresses, self.ipv4_enabled(), self.ipv6_enabled())
        })
        .map_err(|error| NetworkLeaseError::ResolutionFailed {
            host: Arc::from(host),
            port,
            reason: Arc::from(error.to_string()),
        })?;
        self.ensure_valid()?;
        Ok(addresses)
    }

    pub(crate) fn uses_bound_dns(&self) -> bool {
        self.generation.bound_dns_resolver.is_some()
    }

    pub(crate) fn uses_tokio_tcp_backend(&self) -> bool {
        self.generation.socket_factory.uses_tokio_tcp_backend()
    }

    pub async fn connect_tcp(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        self.cancel_on_invalidation(self.generation.socket_factory.connect_tcp(addr))
            .await
            .map_err(io::Error::other)?
    }

    pub async fn bind_tcp_listener(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        self.ensure_valid().map_err(io::Error::other)?;
        let listener = self
            .generation
            .socket_factory
            .bind_tcp_listener(addr)
            .await?;
        self.ensure_valid().map_err(io::Error::other)?;
        Ok(listener)
    }

    pub async fn bind_udp(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        self.ensure_valid().map_err(io::Error::other)?;
        let socket = self.generation.socket_factory.bind_udp(addr)?;
        self.ensure_valid().map_err(io::Error::other)?;
        Ok(socket)
    }

    pub fn tracker_http_client(&self) -> Result<NetworkHttpClient, NetworkLeaseError> {
        self.ensure_valid()?;
        Ok(self.generation.tracker_http_client()?.clone())
    }

    pub fn general_http_client(&self) -> Result<NetworkHttpClient, NetworkLeaseError> {
        self.ensure_valid()?;
        Ok(self.generation.general_http_client()?.clone())
    }

    pub fn rss_http_client(&self) -> Result<NetworkHttpClient, NetworkLeaseError> {
        self.ensure_valid()?;
        Ok(self.generation.rss_http_client()?.clone())
    }

    pub fn web_seed_http_client(&self) -> Result<NetworkHttpClient, NetworkLeaseError> {
        self.ensure_valid()?;
        Ok(self.generation.web_seed_http_client()?.clone())
    }
}

fn filter_enabled_address_families(
    addresses: Vec<SocketAddr>,
    ipv4: bool,
    ipv6: bool,
) -> io::Result<Vec<SocketAddr>> {
    let addresses: Vec<_> = addresses
        .into_iter()
        .map(normalize_socket_addr)
        .filter(|address| (address.is_ipv4() && ipv4) || (address.is_ipv6() && ipv6))
        .collect();
    if addresses.is_empty() {
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "DNS returned no addresses on an enabled address family",
        ))
    } else {
        Ok(addresses)
    }
}

pub(crate) fn normalize_ip_address(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(address)),
        address => address,
    }
}

pub(crate) fn normalize_socket_addr(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(address) => address
            .ip()
            .to_ipv4_mapped()
            .map(|ipv4| SocketAddr::new(IpAddr::V4(ipv4), address.port()))
            .unwrap_or(SocketAddr::V6(address)),
        address => address,
    }
}

pub(crate) async fn wait_for_invalidation(invalidation_rx: &mut watch::Receiver<bool>) {
    let _ = invalidation_rx.wait_for(|invalidated| *invalidated).await;
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
        let lease = NetworkLease {
            generation,
            activation_id: None,
            activation_invalidation_rx: None,
        };
        lease.ensure_valid()?;
        Ok(lease)
    }

    pub fn try_lease_generation(
        &self,
        expected_generation_id: u64,
    ) -> Result<NetworkLease, NetworkLeaseError> {
        let lease = self.try_lease()?;
        if lease.generation_id() != expected_generation_id {
            return Err(NetworkLeaseError::Invalidated {
                generation_id: expected_generation_id,
            });
        }
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

    pub async fn reconfigure(
        &self,
        config: NetworkBindingConfig,
    ) -> Result<(), mpsc::error::SendError<()>> {
        let (completion_tx, completion_rx) = oneshot::channel();
        self.command_tx
            .send(NetworkSupervisorCommand::ConfigurationChanged {
                config,
                completion_tx,
            })
            .await
            .map_err(|_| mpsc::error::SendError(()))?;
        completion_rx.await.map_err(|_| mpsc::error::SendError(()))
    }

    pub async fn interface_changed(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.command_tx
            .send(NetworkSupervisorCommand::InterfaceChanged)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    pub async fn retry_binding(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.command_tx
            .send(NetworkSupervisorCommand::RetryBinding)
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

    pub async fn block_generation(
        &self,
        generation_id: u64,
        reason: impl Into<Arc<str>>,
    ) -> Result<(), mpsc::error::SendError<()>> {
        self.command_tx
            .send(NetworkSupervisorCommand::BlockGeneration {
                generation_id,
                reason: NetworkBlockedReason::new(reason),
            })
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
    ConfigurationChanged {
        config: NetworkBindingConfig,
        completion_tx: oneshot::Sender<()>,
    },
    InterfaceChanged,
    RetryBinding,
    Block(NetworkBlockedReason),
    BlockGeneration {
        generation_id: u64,
        reason: NetworkBlockedReason,
    },
    Shutdown,
}

#[derive(Debug)]
pub struct NetworkSupervisor {
    next_generation_id: AtomicU64,
    desired_epoch: u64,
    desired_config: NetworkBindingConfig,
    last_resolved_binding: Option<ResolvedNetworkBinding>,
    state_tx: watch::Sender<NetworkState>,
    command_rx: mpsc::Receiver<NetworkSupervisorCommand>,
}

impl NetworkSupervisor {
    pub fn spawn_with_config(config: &NetworkBindingConfig) -> (NetworkHandle, JoinHandle<()>) {
        let resolved_binding = ResolvedNetworkBinding::resolve(config).ok();
        let initial_state = match NetworkGeneration::from_config(1, 1, config) {
            Ok(generation) => NetworkState::Ready(Arc::new(generation)),
            Err(error) => NetworkState::Blocked(NetworkBlockedReason::new(format!(
                "network binding configuration could not be activated: {error}"
            ))),
        };
        let last_resolved_binding = match &initial_state {
            NetworkState::Ready(generation) => Some(generation.socket_factory.binding.clone()),
            NetworkState::Blocked(_) => resolved_binding,
        };
        let (state_tx, state_rx) = watch::channel(initial_state);
        let (command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let supervisor = Self {
            next_generation_id: AtomicU64::new(2),
            desired_epoch: 1,
            desired_config: config.clone(),
            last_resolved_binding,
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
            desired_epoch: 1,
            desired_config: NetworkBindingConfig::default(),
            last_resolved_binding: Some(ResolvedNetworkBinding::unrestricted()),
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
        let mut binding_monitor = time::interval(BINDING_MONITOR_INTERVAL);
        binding_monitor.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let command = tokio::select! {
                biased;
                command = self.command_rx.recv() => command,
                _ = binding_monitor.tick() => {
                    self.refresh_binding_snapshot();
                    continue;
                }
            };
            match command {
                Some(NetworkSupervisorCommand::RebuildUnrestricted) => {
                    self.desired_epoch = self.desired_epoch.saturating_add(1);
                    self.desired_config = NetworkBindingConfig::default();
                    self.rebuild_desired("unrestricted network rebuild");
                }
                Some(NetworkSupervisorCommand::ConfigurationChanged {
                    config,
                    completion_tx,
                }) => {
                    self.desired_epoch = self.desired_epoch.saturating_add(1);
                    self.desired_config = config;
                    self.rebuild_desired("network configuration changed");
                    let _ = completion_tx.send(());
                }
                Some(NetworkSupervisorCommand::InterfaceChanged) => {
                    self.desired_epoch = self.desired_epoch.saturating_add(1);
                    self.rebuild_desired("network interface changed");
                }
                Some(NetworkSupervisorCommand::RetryBinding) => {
                    self.rebuild_desired("retrying network binding");
                }
                Some(NetworkSupervisorCommand::Block(reason)) => {
                    if let NetworkState::Ready(generation) = &*self.state_tx.borrow() {
                        self.last_resolved_binding =
                            Some(generation.socket_factory.binding.clone());
                    }
                    self.invalidate_current();
                    self.state_tx.send_replace(NetworkState::Blocked(reason));
                }
                Some(NetworkSupervisorCommand::BlockGeneration {
                    generation_id,
                    reason,
                }) => {
                    self.block_generation(generation_id, reason);
                }
                Some(NetworkSupervisorCommand::Shutdown) => {
                    self.invalidate_current();
                    self.state_tx
                        .send_replace(NetworkState::Blocked(NetworkBlockedReason::new(
                            "network supervisor shut down",
                        )));
                    break;
                }
                None => break,
            }
        }
        self.invalidate_current();
    }

    fn refresh_binding_snapshot(&mut self) {
        if self.desired_config.mode == NetworkBindingMode::Any {
            return;
        }

        let resolved = ResolvedNetworkBinding::resolve(&self.desired_config);
        let snapshot_changed = match (&*self.state_tx.borrow(), &resolved) {
            (NetworkState::Ready(generation), Ok(binding)) => !generation
                .socket_factory
                .binding
                .generation_equivalent(binding),
            (NetworkState::Ready(_), Err(_)) => true,
            (NetworkState::Blocked(_), Ok(binding)) => self
                .last_resolved_binding
                .as_ref()
                .is_none_or(|previous| !previous.generation_equivalent(binding)),
            (NetworkState::Blocked(_), Err(_)) => false,
        };
        if snapshot_changed {
            self.desired_epoch = self.desired_epoch.saturating_add(1);
            self.rebuild_desired("network interface snapshot changed");
        }
    }

    fn invalidate_current(&self) {
        if let NetworkState::Ready(generation) = &*self.state_tx.borrow() {
            generation.invalidate();
        }
    }

    fn block_generation(&mut self, generation_id: u64, reason: NetworkBlockedReason) {
        let current_generation_id = self.state_tx.borrow().generation_id();
        if current_generation_id != Some(generation_id) {
            tracing::debug!(
                attempted_generation_id = generation_id,
                current_generation_id = ?current_generation_id,
                "ignoring failure from an inactive network generation"
            );
            return;
        }
        if let NetworkState::Ready(generation) = &*self.state_tx.borrow() {
            self.last_resolved_binding = Some(generation.socket_factory.binding.clone());
        }
        self.invalidate_current();
        self.state_tx.send_replace(NetworkState::Blocked(reason));
    }

    fn rebuild_desired(&mut self, recovery_reason: &str) {
        self.invalidate_current();
        self.state_tx
            .send_replace(NetworkState::Blocked(NetworkBlockedReason::new(
                recovery_reason.to_string(),
            )));

        let candidate_epoch = self.desired_epoch;
        let generation_id = self.next_generation_id.fetch_add(1, Ordering::Relaxed);
        self.last_resolved_binding = ResolvedNetworkBinding::resolve(&self.desired_config).ok();
        let candidate =
            NetworkGeneration::from_config(generation_id, candidate_epoch, &self.desired_config);
        if candidate_epoch != self.desired_epoch {
            return;
        }
        match candidate {
            Ok(generation) => {
                self.last_resolved_binding = Some(generation.socket_factory.binding.clone());
                self.state_tx
                    .send_replace(NetworkState::Ready(Arc::new(generation)));
            }
            Err(error) => {
                self.state_tx
                    .send_replace(NetworkState::Blocked(NetworkBlockedReason::new(format!(
                        "network binding configuration could not be activated: {error}"
                    ))));
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SocketFactory {
    binding: ResolvedNetworkBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedNetworkBinding {
    mode: NetworkBindingMode,
    interface_name: Option<Arc<str>>,
    interface_index: Option<NonZeroU32>,
    enable_ipv4: bool,
    enable_ipv6: bool,
    ipv4_address: Option<Ipv4Addr>,
    ipv6_address: Option<Ipv6Addr>,
    http_local_address: Option<IpAddr>,
    interface_ipv4_addresses: Arc<[Ipv4Addr]>,
    interface_ipv6_addresses: Arc<[Ipv6Addr]>,
}

#[derive(Debug)]
struct InterfaceSnapshot {
    index: NonZeroU32,
    ipv4_addresses: Vec<Ipv4Addr>,
    ipv6_addresses: Vec<Ipv6Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInterfaceInfo {
    pub name: String,
    pub index: u32,
    pub is_up: bool,
    pub is_loopback: bool,
    pub ipv4_addresses: Vec<Ipv4Addr>,
    pub ipv6_addresses: Vec<Ipv6Addr>,
}

impl SocketFactory {
    fn unrestricted() -> Self {
        Self {
            binding: ResolvedNetworkBinding::unrestricted(),
        }
    }

    pub(crate) fn from_config(config: &NetworkBindingConfig) -> io::Result<Self> {
        Ok(Self {
            binding: ResolvedNetworkBinding::resolve(config)?,
        })
    }

    fn uses_tokio_tcp_backend(&self) -> bool {
        // Keep fresh/default installs on the same TCP construction path as main.
        // Strict modes need socket2 so their source/interface policy is applied first.
        self.binding.mode == NetworkBindingMode::Any
    }

    pub async fn connect_tcp(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        if self.uses_tokio_tcp_backend() {
            return TcpStream::connect(addr).await;
        }

        let addr = normalize_socket_addr(addr);
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

    pub async fn bind_tcp_listener(&self, addr: SocketAddr) -> io::Result<TcpListener> {
        if self.uses_tokio_tcp_backend() {
            return TcpListener::bind(addr).await;
        }

        let addr = normalize_socket_addr(addr);
        let socket = self.tcp_socket(addr)?;
        #[cfg(unix)]
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
        let addr = normalize_socket_addr(addr);
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
        self.preflight_with(|addr| {
            let tcp = self.tcp_socket(addr)?;
            self.bind_outgoing_source(&tcp, addr)?;
            let udp = Socket::new(domain_for(addr), Type::DGRAM, Some(Protocol::UDP))?;
            self.apply_interface_binding(&udp, addr)?;
            if self.source_address(addr).is_some() {
                udp.bind(&SockAddr::from(self.bound_local_addr(addr)?))?;
            }
            Ok(())
        })
    }

    fn preflight_with(
        &self,
        mut probe_family: impl FnMut(SocketAddr) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.binding.mode == NetworkBindingMode::Any {
            return Ok(());
        }

        for addr in self.enabled_probe_addresses() {
            probe_family(addr)?;
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
            socket.bind(&SockAddr::from(self.source_socket_addr(source, 0)?))?;
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
        self.source_socket_addr(source, requested.port())
    }

    fn source_address(&self, addr: SocketAddr) -> Option<IpAddr> {
        match addr {
            SocketAddr::V4(_) => self.binding.ipv4_address.map(IpAddr::V4),
            SocketAddr::V6(_) => self.binding.ipv6_address.map(IpAddr::V6),
        }
    }

    fn source_socket_addr(&self, source: IpAddr, port: u16) -> io::Result<SocketAddr> {
        match source {
            IpAddr::V6(address) if address.is_unicast_link_local() => {
                let scope_id = self.binding.interface_index.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "IPv6 link-local source requires a resolved interface scope",
                    )
                })?;
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    address,
                    port,
                    0,
                    scope_id.get(),
                )))
            }
            _ => Ok(SocketAddr::new(source, port)),
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
            Ok(())
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

    fn configure_http_transport(
        &self,
        mut builder: reqwest::ClientBuilder,
    ) -> reqwest::ClientBuilder {
        if self.binding.mode != NetworkBindingMode::Any {
            builder = builder.no_proxy();
        }
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

    fn configure_http_client(
        &self,
        builder: reqwest::ClientBuilder,
        resolver: Option<Arc<FamilyFilteringResolver>>,
    ) -> reqwest::ClientBuilder {
        let mut builder = self.configure_http_transport(builder);
        if let Some(resolver) = resolver {
            builder = builder.dns_resolver(resolver);
        }
        builder
    }
}

impl ResolvedNetworkBinding {
    fn generation_equivalent(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.interface_name == other.interface_name
            && self.interface_index == other.interface_index
            && self.enable_ipv4 == other.enable_ipv4
            && self.enable_ipv6 == other.enable_ipv6
            && self.ipv4_address == other.ipv4_address
            && self.ipv6_address == other.ipv6_address
            && self.http_local_address == other.http_local_address
            && (!self.enable_ipv4
                || self.ipv4_address.is_some()
                || self.interface_ipv4_addresses == other.interface_ipv4_addresses)
            && (!self.enable_ipv6
                || self.ipv6_address.is_some()
                || self.interface_ipv6_addresses == other.interface_ipv6_addresses)
    }

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
            interface_ipv4_addresses: Arc::from([]),
            interface_ipv6_addresses: Arc::from([]),
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
            interface_ipv4_addresses: Arc::from(snapshot.ipv4_addresses),
            interface_ipv6_addresses: Arc::from(snapshot.ipv6_addresses),
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
        let local_address = http_local_address.expect("validated local address");
        if matches!(
            local_address,
            IpAddr::V6(address) if address.is_unicast_link_local()
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPv6 link-local local-address mode cannot represent an interface scope; use interface binding mode",
            ));
        }
        if !local_address_is_assigned_to_host(local_address)? {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("configured local address {local_address} is not assigned to this host"),
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
            interface_ipv4_addresses: Arc::from([]),
            interface_ipv6_addresses: Arc::from([]),
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
    _snapshot: &InterfaceSnapshot,
) -> Option<IpAddr> {
    match (config.enable_ipv4, config.enable_ipv6) {
        (true, false) => Some(IpAddr::V4(
            config.ipv4_address.unwrap_or(Ipv4Addr::UNSPECIFIED),
        )),
        (false, true) => Some(IpAddr::V6(
            config.ipv6_address.unwrap_or(Ipv6Addr::UNSPECIFIED),
        )),
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
    visit_interface_addresses(|name, address, is_up, _| {
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

#[cfg(unix)]
pub fn available_network_interfaces() -> io::Result<Vec<NetworkInterfaceInfo>> {
    let mut interfaces = BTreeMap::<String, NetworkInterfaceInfo>::new();
    visit_interface_addresses(|name, address, is_up, is_loopback| {
        let interface =
            interfaces
                .entry(name.to_string())
                .or_insert_with(|| NetworkInterfaceInfo {
                    name: name.to_string(),
                    index: 0,
                    is_up,
                    is_loopback,
                    ipv4_addresses: Vec::new(),
                    ipv6_addresses: Vec::new(),
                });
        interface.is_up |= is_up;
        interface.is_loopback |= is_loopback;
        match address {
            IpAddr::V4(address) => interface.ipv4_addresses.push(address),
            IpAddr::V6(address) => interface.ipv6_addresses.push(address),
        }
    })?;

    let mut discovered = Vec::with_capacity(interfaces.len());
    for (_, mut interface) in interfaces {
        let name = CString::new(interface.name.as_str()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "operating-system interface name contains an interior NUL byte",
            )
        })?;
        interface.index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if interface.index == 0 {
            continue;
        }
        interface.ipv4_addresses.sort_unstable();
        interface.ipv4_addresses.dedup();
        interface.ipv6_addresses.sort_unstable();
        interface.ipv6_addresses.dedup();
        discovered.push(interface);
    }
    Ok(discovered)
}

#[cfg(not(unix))]
pub fn available_network_interfaces() -> io::Result<Vec<NetworkInterfaceInfo>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "network interface discovery is not supported on this operating system",
    ))
}

#[cfg(not(unix))]
fn interface_snapshot(interface_name: &str) -> io::Result<InterfaceSnapshot> {
    let _ = interface_name;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "strict interface binding is not supported on this operating system",
    ))
}

pub(crate) fn local_address_is_assigned_to_host(address: IpAddr) -> io::Result<bool> {
    if address.is_loopback() {
        Ok(true)
    } else {
        Ok(all_interface_addresses()?.contains(&address))
    }
}

#[cfg(unix)]
fn all_interface_addresses() -> io::Result<Vec<IpAddr>> {
    let mut addresses = Vec::new();
    visit_interface_addresses(|_, address, is_up, _| {
        if is_up {
            addresses.push(address);
        }
    })?;
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

#[cfg(windows)]
fn all_interface_addresses() -> io::Result<Vec<IpAddr>> {
    let networks = sysinfo::Networks::new_with_refreshed_list();
    let mut addresses = networks
        .values()
        .flat_map(|network| network.ip_networks())
        .map(|network| network.addr)
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

#[cfg(all(not(unix), not(windows)))]
fn all_interface_addresses() -> io::Result<Vec<IpAddr>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "local-address binding is not supported on this operating system",
    ))
}

#[cfg(target_os = "linux")]
fn linux_usable_ipv6_addresses() -> io::Result<BTreeMap<String, Vec<Ipv6Addr>>> {
    let state = std::fs::read_to_string("/proc/net/if_inet6")?;
    parse_linux_usable_ipv6_addresses(&state)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_usable_ipv6_addresses(state: &str) -> io::Result<BTreeMap<String, Vec<Ipv6Addr>>> {
    let mut usable = BTreeMap::<String, Vec<Ipv6Addr>>::new();
    for (line_index, line) in state.lines().enumerate() {
        let mut fields = line.split_whitespace();
        let address = fields.next();
        let _interface_index = fields.next();
        let _prefix_length = fields.next();
        let _scope = fields.next();
        let flags = fields.next();
        let interface_name = fields.next();
        let (Some(address), Some(flags), Some(interface_name)) = (address, flags, interface_name)
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid /proc/net/if_inet6 entry on line {}",
                    line_index + 1
                ),
            ));
        };
        if address.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid IPv6 address width in /proc/net/if_inet6 on line {}",
                    line_index + 1
                ),
            ));
        }
        let address = u128::from_str_radix(address, 16)
            .map(Ipv6Addr::from)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid IPv6 address in /proc/net/if_inet6 on line {}: {error}",
                        line_index + 1
                    ),
                )
            })?;
        let flags = u32::from_str_radix(flags, 16).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid address flags in /proc/net/if_inet6 on line {}: {error}",
                    line_index + 1
                ),
            )
        })?;
        let tentative = flags & LINUX_IFA_F_TENTATIVE != 0;
        let optimistic = flags & LINUX_IFA_F_OPTIMISTIC != 0;
        let dad_failed = flags & LINUX_IFA_F_DADFAILED != 0;
        if !dad_failed && (!tentative || optimistic) {
            usable
                .entry(interface_name.to_string())
                .or_default()
                .push(address);
        }
    }
    for addresses in usable.values_mut() {
        addresses.sort_unstable();
        addresses.dedup();
    }
    Ok(usable)
}

#[cfg(any(target_os = "linux", test))]
fn linux_ipv6_address_is_usable(
    usable: Option<&BTreeMap<String, Vec<Ipv6Addr>>>,
    interface_name: &str,
    address: Ipv6Addr,
) -> bool {
    usable.is_some_and(|usable| {
        usable
            .get(interface_name)
            .is_some_and(|addresses| addresses.contains(&address))
    })
}

#[cfg(unix)]
fn visit_interface_addresses(mut visit: impl FnMut(&str, IpAddr, bool, bool)) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    // getifaddrs exposes interface flags, not IPv6 address-state flags. If the
    // kernel state cannot be read, keep IPv4 discovery working but fail closed
    // by withholding IPv6 addresses whose DAD readiness cannot be established.
    let usable_ipv6_addresses = linux_usable_ipv6_addresses().ok();
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
            let is_loopback = entry.ifa_flags & (libc::IFF_LOOPBACK as u32) != 0;
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
                #[cfg(target_os = "linux")]
                if let IpAddr::V6(ipv6_address) = address {
                    if !linux_ipv6_address_is_usable(
                        usable_ipv6_addresses.as_ref(),
                        name.as_ref(),
                        ipv6_address,
                    ) {
                        current = entry.ifa_next;
                        continue;
                    }
                }
                visit(&name, address, is_up, is_loopback);
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
pub(crate) fn test_network_lease_with_generation(
    generation_id: u64,
) -> (NetworkHandle, NetworkLease) {
    let generation = Arc::new(
        NetworkGeneration::unrestricted(generation_id).expect("construct test network generation"),
    );
    let (_state_tx, state_rx) = watch::channel(NetworkState::Ready(generation));
    let (command_tx, _command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
    let handle = NetworkHandle {
        state_rx,
        command_tx,
    };
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
    return matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EINPROGRESS || code == libc::EALREADY
    );
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
    fn interface_binding_support_matches_implemented_targets() {
        assert_eq!(
            INTERFACE_BINDING_SUPPORTED,
            cfg!(any(
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
            ))
        );
    }

    #[test]
    fn linux_ipv6_address_state_parser_recognizes_only_usable_addresses() {
        let tentative = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let dad_failed = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);
        let ready = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 3);
        let optimistic = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 4);
        let state = concat!(
            "20010db8000000000000000000000001 02 40 00 40 interface-test0\n",
            "20010db8000000000000000000000002 02 40 00 08 interface-test0\n",
            "20010db8000000000000000000000003 02 40 00 80 interface-test0\n",
            "20010db8000000000000000000000004 02 40 00 44 interface-test0\n",
        );

        let usable = parse_linux_usable_ipv6_addresses(state).expect("parse address state");
        let addresses = usable
            .get("interface-test0")
            .expect("usable addresses for interface");

        assert!(!addresses.contains(&tentative));
        assert!(!addresses.contains(&dad_failed));
        assert!(addresses.contains(&ready));
        assert!(addresses.contains(&optimistic));
        assert!(!linux_ipv6_address_is_usable(
            None,
            "interface-test0",
            ready
        ));
        assert!(linux_ipv6_address_is_usable(
            Some(&usable),
            "interface-test0",
            ready
        ));
        assert!(!linux_ipv6_address_is_usable(
            Some(&usable),
            "interface-test0",
            tentative
        ));
    }

    #[test]
    fn http_client_rejects_literal_address_on_disabled_family() {
        let client = NetworkHttpClient::new(reqwest::Client::new(), true, false);

        assert!(client.get("http://192.0.2.10/").is_ok());
        assert!(client.get("http://[::ffff:192.0.2.10]/").is_ok());
        let error = client
            .get("http://[2001:db8::10]/")
            .expect_err("IPv6 literal must be rejected when IPv6 is disabled");

        assert!(matches!(
            &error,
            NetworkLeaseError::HttpRequestRejected { .. }
        ));
        assert!(error.to_string().contains("disabled address family"));

        let client = NetworkHttpClient::new(reqwest::Client::new(), false, true);
        assert!(client.get("http://[2001:db8::10]/").is_ok());
        assert!(client.get("http://192.0.2.10/").is_err());
        assert!(client.get("http://[::ffff:192.0.2.10]/").is_err());
    }

    #[test]
    fn unrestricted_http_policy_uses_resolved_dual_stack_families() {
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Any,
            enable_ipv4: true,
            enable_ipv6: false,
            ..NetworkBindingConfig::default()
        };
        let generation =
            NetworkGeneration::from_config_with_http_client_builder(1, 1, &config, |_| {
                reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .map_err(io::Error::other)
            })
            .expect("build unrestricted generation from stale family flags");
        let client = generation
            .general_http_client()
            .expect("unrestricted HTTP client");

        assert!(client.ipv4);
        assert!(client.ipv6);
        assert!(client.get("http://192.0.2.10/").is_ok());
        assert!(client.get("http://[2001:db8::10]/").is_ok());
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn constrained_http_client_ignores_configured_proxy_hops() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind direct HTTP target");
        let target_address = target.local_addr().expect("direct target address");
        let proxy = TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
            .await
            .expect("bind opposite-family proxy");
        let proxy_address = proxy.local_addr().expect("proxy address");
        let target_task = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await.expect("accept direct request");
            let mut request = [0_u8; 1024];
            let _ = stream
                .read(&mut request)
                .await
                .expect("read direct request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write direct response");
        });
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let factory = SocketFactory::from_config(&config).expect("build constrained factory");
        let client = factory
            .configure_http_client(
                reqwest::Client::builder().proxy(
                    reqwest::Proxy::all(format!("http://{proxy_address}"))
                        .expect("configure explicit proxy"),
                ),
                None,
            )
            .build()
            .expect("build constrained HTTP client");

        client
            .get(format!("http://{target_address}/"))
            .send()
            .await
            .expect("send request directly under constrained policy");
        target_task.await.expect("direct target task");
        assert!(time::timeout(Duration::from_millis(100), proxy.accept())
            .await
            .is_err());
    }

    #[test]
    fn ipv4_mapped_ipv6_socket_addresses_use_ipv4_policy() {
        let mapped = SocketAddr::new(
            IpAddr::V6(Ipv4Addr::new(192, 0, 2, 10).to_ipv6_mapped()),
            8080,
        );

        assert_eq!(
            normalize_socket_addr(mapped),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)), 8080)
        );
        assert!(filter_enabled_address_families(vec![mapped], true, false).is_ok());
        assert!(filter_enabled_address_families(vec![mapped], false, true).is_err());
    }

    #[test]
    fn rss_http_client_rejects_non_public_literal_and_credentialed_urls() {
        let client = NetworkHttpClient::new(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            true,
            true,
        )
        .public_only();
        for url in [
            "http://127.0.0.1/feed",
            "https://[::1]/feed",
            "http://192.168.1.2/feed",
            "https://user:secret@feed.test/feed",
            "file:///tmp/feed",
        ] {
            assert!(
                client.get(url).is_err(),
                "RSS URL should be rejected: {url}"
            );
        }
        assert!(client.get("https://feed.test/feed").is_ok());
    }

    #[tokio::test]
    async fn rss_resolver_rejects_private_answer_before_connecting() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind forbidden RSS destination");
        let destination = listener.local_addr().unwrap();
        let resolver = Arc::new(PublicFilteringResolver::new(FamilyFilteringResolver::new(
            NetworkDnsResolver::Fixed(vec![destination]),
            true,
            true,
        )));
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(resolver)
            .build()
            .expect("build RSS resolver test client");

        let request = client
            .get(format!("http://feed.test:{}/feed", destination.port()))
            .send()
            .await;
        assert!(request.is_err(), "private DNS answer must fail closed");
        assert!(
            time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "the forbidden listener must receive no connection"
        );
    }

    #[test]
    fn rss_redirect_policy_rejects_private_literal_hops() {
        let private = reqwest::Url::parse("http://10.0.0.8/feed").unwrap();
        assert!(validate_public_http_url(&private).is_err());
        let public = reqwest::Url::parse("https://feed.test/feed").unwrap();
        assert!(validate_public_http_url(&public).is_ok());
    }

    #[tokio::test]
    async fn http_client_rejects_redirect_to_disabled_address_family() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local redirect server");
        let address = listener.local_addr().expect("read redirect server address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.expect("read HTTP request");
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://[2001:db8::10]/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write redirect response");
        });
        let client = reqwest::Client::builder()
            .redirect(http_redirect_policy(true, false))
            .build()
            .expect("build test HTTP client");
        let client = NetworkHttpClient::new(client, true, false);

        let error = client
            .get(format!("http://{address}/"))
            .expect("allow initial IPv4 URL")
            .send()
            .await
            .expect_err("redirect to disabled IPv6 family must fail");

        assert!(error.is_redirect());
        server.await.expect("redirect server task");
    }

    #[tokio::test]
    async fn generation_http_clients_use_the_documented_user_agent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local HTTP server");
        let address = listener.local_addr().expect("read HTTP server address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).await.expect("read HTTP request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request).into_owned());
                stream
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write HTTP response");
            }
            requests
        });

        let generation = NetworkGeneration::unrestricted(1).expect("build generation");
        for client in [
            generation
                .tracker_http_client()
                .expect("tracker HTTP client"),
            generation
                .general_http_client()
                .expect("general HTTP client"),
            generation
                .web_seed_http_client()
                .expect("web-seed HTTP client"),
        ] {
            client
                .get(format!("http://{address}/"))
                .expect("build local request")
                .send()
                .await
                .expect("send local request");
        }

        let requests = server.await.expect("join HTTP server");
        assert_eq!(requests.len(), 3);
        let expected = format!("user-agent: {APP_USER_AGENT}");
        assert!(requests
            .iter()
            .all(|request| request.to_ascii_lowercase().contains(&expected)));
        assert_eq!(
            GENERAL_HTTP_REQUEST_TIMEOUT,
            std::time::Duration::from_secs(20)
        );
    }

    #[test]
    fn http_client_build_failures_do_not_block_unrestricted_sockets() {
        let config = NetworkBindingConfig::default();
        let generation = Arc::new(
            NetworkGeneration::from_config_with_http_client_builder(7, 11, &config, |_| {
                Err(io::Error::other("simulated HTTP client build failure"))
            })
            .expect("HTTP client failure must not reject the network generation"),
        );
        let status = NetworkState::Ready(generation.clone()).runtime_status(&config);
        let lease = NetworkLease {
            generation,
            activation_id: None,
            activation_invalidation_rx: None,
        };

        assert_eq!(status.phase, NetworkRuntimePhase::Ready);
        for error in [
            lease
                .tracker_http_client()
                .expect_err("tracker client failure"),
            lease
                .general_http_client()
                .expect_err("general client failure"),
            lease
                .web_seed_http_client()
                .expect_err("web-seed client failure"),
        ] {
            assert!(matches!(
                error,
                NetworkLeaseError::HttpClientUnavailable { .. }
            ));
            assert!(error
                .to_string()
                .contains("simulated HTTP client build failure"));
        }

        lease
            .ensure_valid()
            .expect("unrestricted lease remains available");
        assert!(lease.generation().socket_factory().uses_tokio_tcp_backend());
    }

    #[test]
    fn http_client_build_panics_do_not_block_unrestricted_sockets() {
        let config = NetworkBindingConfig::default();
        let generation = Arc::new(
            NetworkGeneration::from_config_with_http_client_builder(7, 11, &config, |_| {
                panic!("simulated HTTP client build panic")
            })
            .expect("HTTP client panic must not reject the network generation"),
        );
        let status = NetworkState::Ready(generation.clone()).runtime_status(&config);
        let lease = NetworkLease {
            generation,
            activation_id: None,
            activation_invalidation_rx: None,
        };

        assert_eq!(status.phase, NetworkRuntimePhase::Ready);
        let error = lease
            .general_http_client()
            .expect_err("panicked client must remain unavailable");
        assert!(matches!(
            error,
            NetworkLeaseError::HttpClientUnavailable { .. }
        ));
        assert!(error
            .to_string()
            .contains("HTTP client construction panicked"));
        lease
            .ensure_valid()
            .expect("unrestricted lease remains available");
    }

    #[cfg(unix)]
    #[test]
    fn connect_in_progress_recognizes_platform_errno_constants() {
        for errno in [libc::EINPROGRESS, libc::EALREADY] {
            assert!(connect_is_in_progress(&io::Error::from_raw_os_error(errno)));
        }
        assert!(!connect_is_in_progress(&io::Error::from_raw_os_error(
            libc::EINVAL
        )));
    }

    #[cfg(unix)]
    #[test]
    fn interface_discovery_returns_unique_indexed_interfaces() {
        let interfaces = available_network_interfaces().expect("discover network interfaces");

        assert!(!interfaces.is_empty());
        assert!(interfaces.iter().all(|interface| interface.index > 0));
        assert!(interfaces.iter().all(|interface| {
            !interface.ipv4_addresses.is_empty() || !interface.ipv6_addresses.is_empty()
        }));
        assert!(interfaces
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires the privileged integration_tests/network_binding/run_netns_leak_test.sh harness"]
    async fn linux_network_namespace_strict_binding_probe() {
        use std::env;
        use std::process::Command;
        use tokio::io::AsyncWriteExt;

        let interface = env::var("SUPERSEEDR_NETNS_INTERFACE")
            .expect("SUPERSEEDR_NETNS_INTERFACE must name the isolated VPN interface");
        let tcp_target: SocketAddr = env::var("SUPERSEEDR_NETNS_TCP_TARGET")
            .expect("SUPERSEEDR_NETNS_TCP_TARGET must be set")
            .parse()
            .expect("TCP target must be a socket address");
        let udp_target: SocketAddr = env::var("SUPERSEEDR_NETNS_UDP_TARGET")
            .expect("SUPERSEEDR_NETNS_UDP_TARGET must be set")
            .parse()
            .expect("UDP target must be a socket address");
        let clear_target: SocketAddr = env::var("SUPERSEEDR_NETNS_CLEAR_TARGET")
            .expect("SUPERSEEDR_NETNS_CLEAR_TARGET must be set")
            .parse()
            .expect("clear target must be a socket address");
        let dns_server: SocketAddr = env::var("SUPERSEEDR_NETNS_DNS_SERVER")
            .expect("SUPERSEEDR_NETNS_DNS_SERVER must be set")
            .parse()
            .expect("DNS server must be a socket address");
        let dns_host =
            env::var("SUPERSEEDR_NETNS_DNS_HOST").unwrap_or_else(|_| "probe.invalid".to_string());

        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some(interface.clone()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: DnsPolicy::Bound,
            dns_servers: vec![dns_server],
        };
        let (handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&config);
        let lease = handle
            .try_lease()
            .expect("strict generation should be ready");
        let mut invalidation_rx = lease.subscribe_invalidation();

        let resolved = lease
            .resolve(&dns_host, tcp_target.port())
            .await
            .expect("bound DNS resolution should succeed");
        assert!(resolved
            .iter()
            .any(|address| address.ip() == tcp_target.ip()));

        let mut tcp = lease
            .connect_tcp(tcp_target)
            .await
            .expect("bound TCP connection should succeed");
        tcp.write_all(b"strict-binding-probe")
            .await
            .expect("write bound TCP probe");
        let udp = lease
            .bind_udp(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .await
            .expect("bind generation-owned UDP socket");
        udp.send_to(b"strict-binding-probe", udp_target)
            .await
            .expect("send bound UDP probe");

        let _ = time::timeout(Duration::from_millis(500), lease.connect_tcp(clear_target)).await;
        let _ = udp
            .send_to(b"must-not-use-clear-interface", clear_target)
            .await;

        let status = Command::new("ip")
            .args(["link", "set", "dev", &interface, "down"])
            .status()
            .expect("execute ip link down inside the client namespace");
        assert!(status.success(), "ip link down must succeed");
        handle
            .interface_changed()
            .await
            .expect("notify supervisor of interface loss");
        let mut state_rx = handle.subscribe();
        wait_for_network_state(&mut state_rx, |state| {
            matches!(state, NetworkState::Blocked(_))
        })
        .await;
        invalidation_rx
            .changed()
            .await
            .expect("old generation should be invalidated");
        assert!(*invalidation_rx.borrow());
        assert!(matches!(
            handle.try_lease(),
            Err(NetworkLeaseError::Blocked(_))
        ));

        handle.shutdown().await.expect("shutdown supervisor");
        supervisor_task.await.expect("join supervisor");
    }

    #[test]
    fn ready_runtime_status_reports_generation_and_resolved_policy() {
        let config = NetworkBindingConfig::default();
        let generation = Arc::new(
            NetworkGeneration::from_config(7, 11, &config)
                .expect("construct unrestricted generation"),
        );

        let status = NetworkState::Ready(generation).runtime_status(&config);

        assert_eq!(status.phase, NetworkRuntimePhase::Ready);
        assert_eq!(status.mode, NetworkBindingMode::Any);
        assert_eq!(status.generation_id, Some(7));
        assert_eq!(status.config_epoch, Some(11));
        assert_eq!(status.interface, None);
        assert_eq!(status.interface_index, None);
        assert_eq!(status.blocked_reason, None);
        assert_eq!(status.warning, None);
    }

    #[test]
    fn blocked_runtime_status_retains_requested_policy_and_failure() {
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some("interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };

        let status = NetworkState::Blocked(NetworkBlockedReason::new("permission denied"))
            .runtime_status(&config);

        assert_eq!(status.phase, NetworkRuntimePhase::Blocked);
        assert_eq!(status.mode, NetworkBindingMode::Interface);
        assert_eq!(status.interface.as_deref(), Some("interface-test"));
        assert_eq!(status.generation_id, None);
        assert_eq!(status.config_epoch, None);
        assert_eq!(status.blocked_reason.as_deref(), Some("permission denied"));
        assert!(status
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("System DNS")));
    }

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

            let normalized_source = source.replace("\r\n", "\n");
            let production = normalized_source
                .split_once("\n#[cfg(test)]\nmod tests")
                .map_or(normalized_source.as_str(), |(production, _)| production);
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
    async fn invalidation_cancels_an_inflight_generation_operation() {
        let (handle, task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let lease = handle.try_lease().unwrap();
        let operation_lease = lease.clone();
        let operation = tokio::spawn(async move {
            operation_lease
                .cancel_on_invalidation(std::future::pending::<()>())
                .await
        });

        handle.block("test cancellation").await.unwrap();
        let error = time::timeout(Duration::from_millis(500), operation)
            .await
            .expect("operation should cancel promptly")
            .expect("operation task")
            .expect_err("invalidated operation must fail");
        assert_eq!(
            error,
            NetworkLeaseError::Invalidated {
                generation_id: lease.generation_id()
            }
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn invalidation_wait_observes_a_value_published_before_subscription() {
        let (invalidation_tx, _) = watch::channel(false);
        invalidation_tx.send_replace(true);
        let mut invalidation_rx = invalidation_tx.subscribe();

        time::timeout(
            Duration::from_millis(500),
            wait_for_invalidation(&mut invalidation_rx),
        )
        .await
        .expect("already-published invalidation should be observed promptly");
    }

    #[tokio::test]
    async fn system_dns_resolution_rejects_a_disabled_literal_family() {
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let (handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&config);
        let lease = handle.try_lease().expect("strict IPv4 generation");

        assert_eq!(
            lease.resolve("127.0.0.1", 4242).await.unwrap(),
            vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 4242))]
        );
        let error = lease
            .resolve("::1", 4242)
            .await
            .expect_err("system resolution must omit disabled IPv6 results");
        assert!(error.to_string().contains("enabled address family"));

        handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[test]
    fn exact_source_generation_equivalence_ignores_secondary_address_changes() {
        let selected = Ipv4Addr::new(192, 0, 2, 10);
        let pinned = ResolvedNetworkBinding {
            mode: NetworkBindingMode::Interface,
            interface_name: Some(Arc::from("interface-test")),
            interface_index: Some(NonZeroU32::new(7).unwrap()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(selected),
            ipv6_address: None,
            http_local_address: Some(IpAddr::V4(selected)),
            interface_ipv4_addresses: Arc::from([selected, Ipv4Addr::new(192, 0, 2, 20)]),
            interface_ipv6_addresses: Arc::from([]),
        };
        let mut secondary_changed = pinned.clone();
        secondary_changed.interface_ipv4_addresses =
            Arc::from([selected, Ipv4Addr::new(192, 0, 2, 30)]);

        assert!(pinned.generation_equivalent(&secondary_changed));

        let mut automatic = pinned.clone();
        automatic.ipv4_address = None;
        automatic.http_local_address = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let mut automatic_changed = secondary_changed;
        automatic_changed.ipv4_address = None;
        automatic_changed.http_local_address = Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(!automatic.generation_equivalent(&automatic_changed));
    }

    #[test]
    fn interface_link_local_source_preserves_its_scope_id() {
        let source = "fe80::42".parse::<Ipv6Addr>().unwrap();
        let factory = SocketFactory {
            binding: ResolvedNetworkBinding {
                mode: NetworkBindingMode::Interface,
                interface_name: Some(Arc::from("interface-test")),
                interface_index: Some(NonZeroU32::new(7).unwrap()),
                enable_ipv4: false,
                enable_ipv6: true,
                ipv4_address: None,
                ipv6_address: Some(source),
                http_local_address: Some(IpAddr::V6(source)),
                interface_ipv4_addresses: Arc::from([]),
                interface_ipv6_addresses: Arc::from([source]),
            },
        };

        let SocketAddr::V6(scoped) = factory
            .source_socket_addr(IpAddr::V6(source), 4242)
            .expect("scope link-local source")
        else {
            panic!("link-local source must remain IPv6");
        };
        assert_eq!(*scoped.ip(), source);
        assert_eq!(scoped.port(), 4242);
        assert_eq!(scoped.scope_id(), 7);
    }

    #[test]
    fn local_address_mode_rejects_unscoped_ipv6_link_local_source() {
        let source = "fe80::42".parse::<Ipv6Addr>().unwrap();
        let error = SocketFactory::from_config(&NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: false,
            enable_ipv6: true,
            ipv4_address: None,
            ipv6_address: Some(source),
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        })
        .expect_err("unscoped link-local source must be rejected");

        assert!(error.to_string().contains("interface scope"));
    }

    #[test]
    fn interface_http_family_selection_does_not_pin_an_arbitrary_address() {
        let snapshot = InterfaceSnapshot {
            index: NonZeroU32::new(1).unwrap(),
            ipv4_addresses: vec![Ipv4Addr::new(192, 0, 2, 20), Ipv4Addr::new(192, 0, 2, 10)],
            ipv6_addresses: Vec::new(),
        };
        let family_only = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some("test-interface".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        assert_eq!(
            single_family_http_address(&family_only, &snapshot),
            Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        );

        let explicit = NetworkBindingConfig {
            ipv4_address: Some(Ipv4Addr::new(192, 0, 2, 10)),
            ..family_only
        };
        assert_eq!(
            single_family_http_address(&explicit, &snapshot),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)))
        );
    }

    #[test]
    fn unrestricted_preflight_does_not_gate_network_activation() {
        let factory = SocketFactory::from_config(&NetworkBindingConfig::default()).unwrap();
        let mut probe_count = 0usize;
        factory
            .preflight_with(|_| {
                probe_count += 1;
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulated socket restriction",
                ))
            })
            .expect("default unrestricted mode must not gate network activation");
        assert_eq!(probe_count, 0);
    }

    #[test]
    fn unrestricted_tcp_uses_tokio_while_strict_tcp_uses_bound_sockets() {
        let unrestricted = SocketFactory::from_config(&NetworkBindingConfig::default()).unwrap();
        assert!(unrestricted.uses_tokio_tcp_backend());

        let strict = SocketFactory::from_config(&NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        })
        .unwrap();
        assert!(!strict.uses_tokio_tcp_backend());
    }

    #[test]
    fn strict_preflight_rejects_an_unavailable_requested_family() {
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let factory = SocketFactory::from_config(&config).unwrap();
        let error = factory
            .preflight_with(|_| {
                Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "simulated unavailable strict family",
                ))
            })
            .expect_err("strict mode must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
    }

    #[tokio::test]
    async fn reconfigure_invalidates_then_recovers_with_a_new_epoch() {
        let (handle, task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let original = handle.try_lease().unwrap();
        let mut state_rx = handle.subscribe();
        let missing = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some("missing-interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };

        handle.reconfigure(missing).await.unwrap();
        wait_for_network_state(&mut state_rx, |state| {
            matches!(state, NetworkState::Blocked(reason) if missing_interface_reason(reason))
        })
        .await;
        assert!(original.ensure_valid().is_err());
        assert!(handle.try_lease().is_err());

        handle
            .reconfigure(NetworkBindingConfig::default())
            .await
            .unwrap();
        wait_for_network_state(&mut state_rx, |state| {
            matches!(state, NetworkState::Ready(_))
        })
        .await;
        let recovered = handle.try_lease().unwrap();
        assert!(recovered.generation_id() > original.generation_id());
        assert_eq!(recovered.generation().config_epoch(), 3);

        handle.shutdown().await.unwrap();
        task.await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn binding_snapshot_refresh_replaces_a_stale_interface_generation() {
        let (interface, _) = loopback_interface();
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some(interface),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let mut stale_generation =
            NetworkGeneration::from_config(1, 1, &config).expect("initial generation");
        let mut stale_addresses = stale_generation
            .socket_factory
            .binding
            .interface_ipv4_addresses
            .to_vec();
        stale_addresses.push(Ipv4Addr::new(192, 0, 2, 1));
        stale_generation
            .socket_factory
            .binding
            .interface_ipv4_addresses = Arc::from(stale_addresses);
        let stale_generation = Arc::new(stale_generation);
        let (state_tx, _) = watch::channel(NetworkState::Ready(stale_generation.clone()));
        let (_command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let mut supervisor = NetworkSupervisor {
            next_generation_id: AtomicU64::new(2),
            desired_epoch: 1,
            desired_config: config,
            last_resolved_binding: Some(stale_generation.socket_factory.binding.clone()),
            state_tx,
            command_rx,
        };

        supervisor.refresh_binding_snapshot();

        assert!(stale_generation.is_invalidated());
        let state = supervisor.state_tx.borrow().clone();
        let NetworkState::Ready(replacement) = state else {
            panic!("stale binding snapshot did not recover");
        };
        assert_eq!(replacement.id(), 2);
        assert_eq!(replacement.config_epoch(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn binding_snapshot_refresh_ignores_disabled_family_address_changes() {
        let (interface, _) = loopback_interface();
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some(interface),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let mut generation =
            NetworkGeneration::from_config(1, 1, &config).expect("initial generation");
        generation.socket_factory.binding.interface_ipv6_addresses =
            Arc::from([Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)]);
        let generation = Arc::new(generation);
        let (state_tx, _) = watch::channel(NetworkState::Ready(generation.clone()));
        let (_command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let mut supervisor = NetworkSupervisor {
            next_generation_id: AtomicU64::new(2),
            desired_epoch: 1,
            desired_config: config,
            last_resolved_binding: Some(generation.socket_factory.binding.clone()),
            state_tx,
            command_rx,
        };

        supervisor.refresh_binding_snapshot();

        assert!(!generation.is_invalidated());
        assert_eq!(supervisor.desired_epoch, 1);
        let state = supervisor.state_tx.borrow().clone();
        let NetworkState::Ready(current) = state else {
            panic!("disabled-family address churn should retain the generation");
        };
        assert!(Arc::ptr_eq(&current, &generation));
    }

    #[cfg(unix)]
    #[test]
    fn binding_snapshot_refresh_does_not_retry_unchanged_blocked_binding() {
        let (interface, _) = loopback_interface();
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some(interface),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let binding = ResolvedNetworkBinding::resolve(&config).expect("resolve loopback binding");
        let (state_tx, _) = watch::channel(NetworkState::Blocked(NetworkBlockedReason::new(
            "simulated listener failure",
        )));
        let (_command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let mut supervisor = NetworkSupervisor {
            next_generation_id: AtomicU64::new(2),
            desired_epoch: 1,
            desired_config: config,
            last_resolved_binding: Some(binding),
            state_tx,
            command_rx,
        };

        supervisor.refresh_binding_snapshot();

        assert_eq!(supervisor.desired_epoch, 1);
        assert_eq!(supervisor.next_generation_id.load(Ordering::Relaxed), 2);
        assert!(matches!(
            &*supervisor.state_tx.borrow(),
            NetworkState::Blocked(reason) if reason.to_string() == "simulated listener failure"
        ));
    }

    #[test]
    fn stale_generation_failure_does_not_block_newer_generation() {
        let current_generation =
            Arc::new(NetworkGeneration::unrestricted(2).expect("current generation"));
        let (state_tx, _) = watch::channel(NetworkState::Ready(current_generation.clone()));
        let (_command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let mut supervisor = NetworkSupervisor {
            next_generation_id: AtomicU64::new(3),
            desired_epoch: 2,
            desired_config: NetworkBindingConfig::default(),
            last_resolved_binding: Some(current_generation.socket_factory.binding.clone()),
            state_tx,
            command_rx,
        };

        supervisor.block_generation(1, NetworkBlockedReason::new("stale listener bind failure"));

        assert!(!current_generation.is_invalidated());
        assert!(matches!(
            &*supervisor.state_tx.borrow(),
            NetworkState::Ready(generation) if Arc::ptr_eq(generation, &current_generation)
        ));

        supervisor.block_generation(
            current_generation.id(),
            NetworkBlockedReason::new("current listener bind failure"),
        );
        assert!(current_generation.is_invalidated());
        assert!(matches!(
            &*supervisor.state_tx.borrow(),
            NetworkState::Blocked(reason) if reason.to_string() == "current listener bind failure"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bound_dns_policy_routes_network_lease_resolution_through_configured_server() {
        let dns_server = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local DNS server");
        let dns_server_addr = dns_server.local_addr().expect("DNS server address");
        let dns_task = tokio::spawn(async move {
            let mut query = vec![0_u8; 512];
            let (len, peer) = dns_server
                .recv_from(&mut query)
                .await
                .expect("receive DNS query");
            query.truncate(len);
            query[2..4].copy_from_slice(&0x8180_u16.to_be_bytes());
            query[6..8].copy_from_slice(&1_u16.to_be_bytes());
            query.extend_from_slice(&[0xc0, 0x0c]);
            query.extend_from_slice(&1_u16.to_be_bytes());
            query.extend_from_slice(&1_u16.to_be_bytes());
            query.extend_from_slice(&60_u32.to_be_bytes());
            query.extend_from_slice(&4_u16.to_be_bytes());
            query.extend_from_slice(&[127, 0, 0, 1]);
            dns_server
                .send_to(&query, peer)
                .await
                .expect("send DNS response");
        });
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: DnsPolicy::Bound,
            dns_servers: vec![dns_server_addr],
        };
        let (handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&config);
        let lease = handle.try_lease().expect("bound DNS generation");

        assert_eq!(
            lease.resolve("resolver.test", 4242).await.unwrap(),
            vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 4242))]
        );

        dns_task.await.unwrap();
        handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn bound_dns_policy_rejects_unrestricted_network_mode() {
        let config = NetworkBindingConfig {
            dns_policy: DnsPolicy::Bound,
            dns_servers: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 53))],
            ..NetworkBindingConfig::default()
        };
        let (handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&config);

        let error = handle
            .try_lease()
            .expect_err("bound DNS must require strict binding");
        assert!(error
            .to_string()
            .contains("bound DNS requires interface or local-address binding mode"));

        handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn missing_strict_interface_starts_blocked_without_fallback() {
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some("missing-interface-test".to_string()),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let (handle, task) = NetworkSupervisor::spawn_with_config(&config);

        let error = handle.try_lease().expect_err("strict binding must block");
        assert!(missing_interface_reason(&error));

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
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };
        let factory = SocketFactory::from_config(&config).expect("resolve loopback address");
        factory.preflight().expect("preflight loopback address");

        let listener = factory
            .bind_tcp_listener(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            .await
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

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn local_address_reconfigure_switches_ipv4_to_ipv6_and_back() {
        fn local_address_config(address: IpAddr) -> NetworkBindingConfig {
            NetworkBindingConfig {
                mode: NetworkBindingMode::LocalAddress,
                interface: None,
                enable_ipv4: address.is_ipv4(),
                enable_ipv6: address.is_ipv6(),
                ipv4_address: match address {
                    IpAddr::V4(address) => Some(address),
                    IpAddr::V6(_) => None,
                },
                ipv6_address: match address {
                    IpAddr::V4(_) => None,
                    IpAddr::V6(address) => Some(address),
                },
                dns_policy: DnsPolicy::System,
                dns_servers: Vec::new(),
            }
        }

        async fn assert_bound_socket_paths(lease: &NetworkLease, expected: IpAddr) {
            let unspecified = match expected {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            };
            let listener = lease
                .bind_tcp_listener(SocketAddr::new(unspecified, 0))
                .await
                .expect("bind strict TCP listener");
            let listener_address = listener.local_addr().expect("strict listener address");
            assert_eq!(listener_address.ip(), expected);

            let (client, (server, remote_address)) =
                tokio::try_join!(lease.connect_tcp(listener_address), listener.accept(),)
                    .expect("complete strict TCP loopback connection");
            assert_eq!(client.local_addr().expect("client address").ip(), expected);
            assert_eq!(server.local_addr().expect("server address").ip(), expected);
            assert_eq!(remote_address.ip(), expected);

            let udp = lease
                .bind_udp(SocketAddr::new(unspecified, 0))
                .await
                .expect("bind strict UDP socket");
            assert_eq!(udp.local_addr().expect("UDP address").ip(), expected);
        }

        let (handle, supervisor_task) =
            NetworkSupervisor::spawn_unrestricted().expect("start unrestricted generation");
        let mut state_rx = handle.subscribe();
        let mut previous_lease = handle.try_lease().expect("unrestricted lease");
        let mut generation_ids = vec![previous_lease.generation_id()];

        for expected in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ] {
            let config = local_address_config(expected);
            let previous_generation_id = previous_lease.generation_id();
            handle
                .reconfigure(config.clone())
                .await
                .expect("switch strict local address");
            wait_for_network_state(&mut state_rx, |state| {
                matches!(state, NetworkState::Ready(generation) if generation.id() > previous_generation_id)
            })
            .await;

            assert!(matches!(
                previous_lease.ensure_valid(),
                Err(NetworkLeaseError::Invalidated { generation_id })
                    if generation_id == previous_generation_id
            ));
            let current_lease = handle.try_lease().expect("replacement strict lease");
            let status = state_rx.borrow().runtime_status(&config);
            assert_eq!(status.mode, NetworkBindingMode::LocalAddress);
            assert_eq!(status.enable_ipv4, expected.is_ipv4());
            assert_eq!(status.enable_ipv6, expected.is_ipv6());
            assert_eq!(
                status
                    .selected_ipv4_address
                    .map(IpAddr::V4)
                    .or_else(|| status.selected_ipv6_address.map(IpAddr::V6)),
                Some(expected)
            );
            assert_bound_socket_paths(&current_lease, expected).await;

            generation_ids.push(current_lease.generation_id());
            previous_lease = current_lease;
        }

        assert!(generation_ids.windows(2).all(|ids| ids[0] < ids[1]));
        handle.shutdown().await.expect("shutdown supervisor");
        supervisor_task.await.expect("join supervisor");
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
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
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
        visit_interface_addresses(|name, address, is_up, _| {
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

    fn missing_interface_reason(reason: &impl std::fmt::Display) -> bool {
        let reason = reason.to_string();
        reason.contains("was not found") || reason.contains("not supported")
    }

    async fn wait_for_network_state(
        state_rx: &mut watch::Receiver<NetworkState>,
        predicate: impl Fn(&NetworkState) -> bool,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if predicate(&state_rx.borrow()) {
                    return;
                }
                state_rx
                    .changed()
                    .await
                    .expect("network state channel open");
            }
        })
        .await
        .expect("network state transition");
    }
}
