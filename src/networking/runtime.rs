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
#[cfg(windows)]
use std::collections::HashSet;
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

#[cfg(windows)]
#[path = "windows.rs"]
mod windows_backend;

const SUPERVISOR_COMMAND_CAPACITY: usize = 8;
const BINDING_MONITOR_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
// Parallel App tests share process-global socket registries. Give each test
// supervisor a disjoint range so unrelated generations can never alias.
const TEST_GENERATION_ID_RANGE: u64 = 1_000_000;
#[cfg(test)]
static NEXT_TEST_GENERATION_ID: AtomicU64 = AtomicU64::new(1);
const APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const GENERAL_HTTP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
#[cfg(windows)]
const WINDOWS_HTTP_FAMILY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(windows)]
const WINDOWS_HTTP_REDIRECT_LIMIT: usize = 10;
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
    target_os = "watchos",
    windows
));

pub(crate) const DUAL_FAMILY_EXACT_SOURCE_SUPPORTED: bool = cfg!(windows);

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
                    interface: binding.interface_identity.as_deref().map(str::to_owned),
                    interface_display_name: binding
                        .interface_display_name
                        .as_deref()
                        .map(str::to_owned),
                    ipv4_interface_index: binding.ipv4.interface_index.map(NonZeroU32::get),
                    ipv6_interface_index: binding.ipv6.interface_index.map(NonZeroU32::get),
                    enable_ipv4: binding.ipv4.enabled,
                    enable_ipv6: binding.ipv6.enabled,
                    configured_ipv4_address: binding.ipv4.configured_source,
                    configured_ipv6_address: binding.ipv6.configured_source,
                    selected_ipv4_address: binding.ipv4.effective_source,
                    selected_ipv6_address: binding.ipv6.effective_source,
                    interface_ipv4_addresses: binding.ipv4.eligible_sources.to_vec(),
                    interface_ipv6_addresses: binding.ipv6.eligible_sources.to_vec(),
                    ipv4_host_policy: binding.ipv4.host_policy,
                    ipv6_host_policy: binding.ipv6.host_policy,
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
                interface_display_name: None,
                ipv4_interface_index: None,
                ipv6_interface_index: None,
                enable_ipv4: config.enable_ipv4,
                enable_ipv6: config.enable_ipv6,
                configured_ipv4_address: config.ipv4_address,
                configured_ipv6_address: config.ipv6_address,
                selected_ipv4_address: None,
                selected_ipv6_address: None,
                interface_ipv4_addresses: Vec::new(),
                interface_ipv6_addresses: Vec::new(),
                ipv4_host_policy: NetworkFamilyHostPolicy::default(),
                ipv6_host_policy: NetworkFamilyHostPolicy::default(),
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
    client: Option<reqwest::Client>,
    #[cfg(windows)]
    windows_strict: Option<Arc<WindowsStrictHttpClient>>,
    ipv4: bool,
    ipv6: bool,
    public_only: bool,
}

#[derive(Debug)]
pub struct NetworkHttpRequest {
    inner: NetworkHttpRequestInner,
}

#[derive(Debug)]
enum NetworkHttpRequestInner {
    Direct(reqwest::RequestBuilder),
    #[cfg(windows)]
    WindowsStrict {
        client: Arc<WindowsStrictHttpClient>,
        url: reqwest::Url,
        headers: reqwest::header::HeaderMap,
        header_error: Option<reqwest::header::InvalidHeaderValue>,
        public_only: bool,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkHttpRequestError {
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error(transparent)]
    Policy(#[from] NetworkLeaseError),
    #[error("invalid HTTP header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),
}

impl NetworkHttpRequestError {
    #[cfg(test)]
    fn is_redirect(&self) -> bool {
        matches!(self, Self::Request(error) if error.is_redirect())
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsStrictHttpClient {
    ipv4: Option<reqwest::Client>,
    ipv6: Option<reqwest::Client>,
    generation_id: u64,
    invalidation_rx: watch::Receiver<bool>,
}

impl NetworkHttpRequest {
    pub fn header(self, name: reqwest::header::HeaderName, value: impl AsRef<str>) -> Self {
        let inner = match self.inner {
            NetworkHttpRequestInner::Direct(request) => {
                NetworkHttpRequestInner::Direct(request.header(name, value.as_ref()))
            }
            #[cfg(windows)]
            NetworkHttpRequestInner::WindowsStrict {
                client,
                url,
                mut headers,
                mut header_error,
                public_only,
            } => {
                match reqwest::header::HeaderValue::from_str(value.as_ref()) {
                    Ok(value) => {
                        headers.insert(name, value);
                    }
                    Err(error) => header_error = Some(error),
                }
                NetworkHttpRequestInner::WindowsStrict {
                    client,
                    url,
                    headers,
                    header_error,
                    public_only,
                }
            }
        };
        Self { inner }
    }

    pub async fn send(self) -> Result<reqwest::Response, NetworkHttpRequestError> {
        match self.inner {
            NetworkHttpRequestInner::Direct(request) => Ok(request.send().await?),
            #[cfg(windows)]
            NetworkHttpRequestInner::WindowsStrict {
                client,
                url,
                headers,
                header_error,
                public_only,
            } => {
                if let Some(error) = header_error {
                    return Err(error.into());
                }
                client.send(url, headers, public_only).await
            }
        }
    }
}

impl NetworkHttpClient {
    fn new(client: reqwest::Client, ipv4: bool, ipv6: bool) -> Self {
        Self {
            client: Some(client),
            #[cfg(windows)]
            windows_strict: None,
            ipv4,
            ipv6,
            public_only: false,
        }
    }

    fn public_only(mut self) -> Self {
        self.public_only = true;
        self
    }

    #[cfg(windows)]
    fn strict_windows(
        ipv4: Option<reqwest::Client>,
        ipv6: Option<reqwest::Client>,
        generation_id: u64,
        invalidation_rx: watch::Receiver<bool>,
    ) -> Self {
        let has_ipv4 = ipv4.is_some();
        let has_ipv6 = ipv6.is_some();
        Self {
            client: None,
            windows_strict: Some(Arc::new(WindowsStrictHttpClient {
                ipv4,
                ipv6,
                generation_id,
                invalidation_rx,
            })),
            ipv4: has_ipv4,
            ipv6: has_ipv6,
            public_only: false,
        }
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

    pub fn get(&self, url: impl AsRef<str>) -> Result<NetworkHttpRequest, NetworkLeaseError> {
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
        #[cfg(windows)]
        if let Some(client) = &self.windows_strict {
            return Ok(NetworkHttpRequest {
                inner: NetworkHttpRequestInner::WindowsStrict {
                    client: client.clone(),
                    url,
                    headers: reqwest::header::HeaderMap::new(),
                    header_error: None,
                    public_only: self.public_only,
                },
            });
        }
        Ok(NetworkHttpRequest {
            inner: NetworkHttpRequestInner::Direct(
                self.client
                    .as_ref()
                    .expect("direct HTTP client backend")
                    .get(url),
            ),
        })
    }
}

#[cfg(windows)]
impl WindowsStrictHttpClient {
    async fn send(
        &self,
        mut url: reqwest::Url,
        mut headers: reqwest::header::HeaderMap,
        public_only: bool,
    ) -> Result<reqwest::Response, NetworkHttpRequestError> {
        let mut visited = HashSet::new();
        let mut redirect_count = 0usize;

        loop {
            self.ensure_valid()?;
            validate_http_url_family(&url, self.ipv4.is_some(), self.ipv6.is_some())?;
            validate_windows_http_scheme(&url)?;
            if public_only {
                validate_public_http_url(&url)?;
            }
            if let Some(authorization) = self.take_url_authorization(&mut url)? {
                headers
                    .entry(reqwest::header::AUTHORIZATION)
                    .or_insert(authorization);
            }
            let normalized = url.as_str().to_owned();
            if !visited.insert(normalized) {
                return Err(windows_http_rejection(&url, "redirect loop detected").into());
            }

            headers = strip_hop_by_hop_headers(headers);
            let literal_family = url
                .host_str()
                .and_then(|host| host.parse::<IpAddr>().ok())
                .map(normalize_ip_address);
            let clients = [(true, self.ipv4.as_ref()), (false, self.ipv6.as_ref())];
            let mut last_transport_error = None;
            let mut response = None;
            for (ipv4, client) in clients {
                let Some(client) = client else {
                    continue;
                };
                if literal_family.is_some_and(|address| address.is_ipv4() != ipv4) {
                    continue;
                }
                self.ensure_valid()?;
                let mut invalidation_rx = self.invalidation_rx.clone();
                let request = client.get(url.clone()).headers(headers.clone()).send();
                let result = tokio::select! {
                    result = request => result,
                    _ = wait_for_invalidation(&mut invalidation_rx) => {
                        return Err(NetworkLeaseError::Invalidated {
                            generation_id: self.generation_id,
                        }.into());
                    }
                };
                match result {
                    Ok(received) => {
                        self.ensure_valid()?;
                        response = Some(received);
                        break;
                    }
                    Err(error) => {
                        self.ensure_valid()?;
                        if reqwest_error_is_policy_failure(&error) {
                            return Err(NetworkLeaseError::HttpRequestRejected {
                                url: Arc::from(url.as_str()),
                                reason: Arc::from(format!(
                                    "HTTP resolver or transport policy rejected the hop: {error}"
                                )),
                            }
                            .into());
                        }
                        last_transport_error = Some(error);
                    }
                }
            }
            let response = match response {
                Some(response) => response,
                None => {
                    return Err(last_transport_error
                        .expect("strict Windows HTTP client has an enabled family")
                        .into())
                }
            };

            if !response.status().is_redirection() {
                self.ensure_valid()?;
                return Ok(response);
            }
            let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                self.ensure_valid()?;
                return Ok(response);
            };
            if redirect_count == WINDOWS_HTTP_REDIRECT_LIMIT {
                return Err(windows_http_rejection(&url, "redirect limit exceeded").into());
            }
            let location = location
                .to_str()
                .map_err(|_| windows_http_rejection(&url, "redirect Location is not valid text"))?;
            let next_url =
                url.join(location)
                    .map_err(|error| NetworkLeaseError::HttpRequestRejected {
                        url: Arc::from(url.as_str()),
                        reason: Arc::from(format!("invalid redirect Location: {error}")),
                    })?;
            self.ensure_valid()?;
            headers = redirect_headers(headers, &url, &next_url);
            url = next_url;
            redirect_count += 1;
        }
    }

    fn ensure_valid(&self) -> Result<(), NetworkLeaseError> {
        if *self.invalidation_rx.borrow() {
            Err(NetworkLeaseError::Invalidated {
                generation_id: self.generation_id,
            })
        } else {
            Ok(())
        }
    }

    fn take_url_authorization(
        &self,
        url: &mut reqwest::Url,
    ) -> Result<Option<reqwest::header::HeaderValue>, NetworkHttpRequestError> {
        if url.username().is_empty() && url.password().is_none() {
            return Ok(None);
        }
        let client = self
            .ipv4
            .as_ref()
            .or(self.ipv6.as_ref())
            .expect("strict Windows HTTP client has an enabled family");
        let request = client.get(url.clone()).build()?;
        let authorization = request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .cloned()
            .ok_or_else(|| windows_http_rejection(url, "URL credentials are not valid UTF-8"))?;
        *url = request.url().clone();
        Ok(Some(authorization))
    }
}

#[cfg(windows)]
fn validate_windows_http_scheme(url: &reqwest::Url) -> Result<(), NetworkLeaseError> {
    if matches!(url.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(windows_http_rejection(url, "URL must use HTTP or HTTPS"))
    }
}

#[cfg(windows)]
fn windows_http_rejection(url: &reqwest::Url, reason: &'static str) -> NetworkLeaseError {
    NetworkLeaseError::HttpRequestRejected {
        url: Arc::from(url.as_str()),
        reason: Arc::from(reason),
    }
}

#[cfg(windows)]
fn reqwest_error_is_policy_failure(error: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if current
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
        {
            return true;
        }
        source = current.source();
    }
    false
}

#[cfg(any(windows, test))]
fn redirect_headers(
    mut headers: reqwest::header::HeaderMap,
    previous: &reqwest::Url,
    next: &reqwest::Url,
) -> reqwest::header::HeaderMap {
    if http_origin_changed(previous, next) {
        for name in [
            "authorization",
            "cookie",
            "cookie2",
            "proxy-authorization",
            "www-authenticate",
        ] {
            headers.remove(name);
        }
    }
    strip_hop_by_hop_headers(headers)
}

#[cfg(any(windows, test))]
fn http_origin_changed(previous: &reqwest::Url, next: &reqwest::Url) -> bool {
    !previous.scheme().eq_ignore_ascii_case(next.scheme())
        || !previous
            .host_str()
            .zip(next.host_str())
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
        || previous.port_or_known_default() != next.port_or_known_default()
}

#[cfg(any(windows, test))]
fn strip_hop_by_hop_headers(mut headers: reqwest::header::HeaderMap) -> reqwest::header::HeaderMap {
    let connection_headers = headers
        .get_all(reqwest::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        "connection",
        "host",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
    headers
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
    if matches!(
        address,
        IpAddr::V6(address) if address.to_ipv4_mapped().is_some()
    ) {
        return Err(NetworkLeaseError::HttpRequestRejected {
            url: Arc::from(url.as_str()),
            reason: Arc::from("IPv4-mapped IPv6 URL literals are not supported"),
        });
    }
    let enabled = match address {
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
        let http_ipv4 = socket_factory.binding.ipv4.enabled;
        let http_ipv6 = socket_factory.binding.ipv6.enabled;
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
        #[cfg(windows)]
        let (tracker_http_client, general_http_client, rss_http_client, web_seed_http_client) =
            if config.mode == NetworkBindingMode::Interface {
                build_windows_generation_http_clients(
                    id,
                    &socket_factory,
                    bound_dns_resolver.clone(),
                    invalidation_tx.subscribe(),
                    &mut build_http_client,
                )
            } else {
                build_default_generation_http_clients(
                    &socket_factory,
                    bound_dns_resolver.clone(),
                    http_ipv4,
                    http_ipv6,
                    &mut build_http_client,
                )
            };
        #[cfg(not(windows))]
        let (tracker_http_client, general_http_client, rss_http_client, web_seed_http_client) =
            build_default_generation_http_clients(
                &socket_factory,
                bound_dns_resolver.clone(),
                http_ipv4,
                http_ipv6,
                &mut build_http_client,
            );
        #[cfg(windows)]
        if config.mode == NetworkBindingMode::Interface {
            for (purpose, client) in [
                ("tracker", &tracker_http_client),
                ("general-purpose", &general_http_client),
                ("RSS", &rss_http_client),
                ("web-seed", &web_seed_http_client),
            ] {
                if let Err(reason) = client {
                    return Err(io::Error::other(format!(
                        "strict Windows {purpose} HTTP client could not be constructed: {reason}"
                    )));
                }
            }
        }

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

fn configure_rss_http_client(
    socket_factory: &SocketFactory,
    builder: reqwest::ClientBuilder,
    resolver: FamilyFilteringResolver,
    http_ipv4: bool,
    http_ipv6: bool,
) -> reqwest::ClientBuilder {
    let builder = socket_factory
        // Any mode keeps reqwest's normal proxy behavior for upgrade compatibility.
        // Strict binding modes still disable proxies in configure_http_transport.
        .configure_http_transport(builder);
    let builder = if socket_factory.binding.mode == NetworkBindingMode::Any {
        // Preserve legacy resolution in Any mode, including private proxy hosts.
        builder.dns_resolver(Arc::new(resolver))
    } else {
        builder.dns_resolver(Arc::new(PublicFilteringResolver::new(resolver)))
    };
    builder
        .redirect(rss_redirect_policy(http_ipv4, http_ipv6))
        .user_agent(APP_USER_AGENT)
        .timeout(GENERAL_HTTP_REQUEST_TIMEOUT)
}

type GenerationHttpClients = (
    Result<NetworkHttpClient, Arc<str>>,
    Result<NetworkHttpClient, Arc<str>>,
    Result<NetworkHttpClient, Arc<str>>,
    Result<NetworkHttpClient, Arc<str>>,
);

fn build_default_generation_http_clients(
    socket_factory: &SocketFactory,
    bound_dns_resolver: Option<Arc<BoundDnsResolver>>,
    http_ipv4: bool,
    http_ipv6: bool,
    build_http_client: &mut impl FnMut(reqwest::ClientBuilder) -> io::Result<reqwest::Client>,
) -> GenerationHttpClients {
    // Preserve reqwest's native resolver for the unrestricted dual-stack default.
    // A custom resolver is needed only to enforce a family restriction or bound DNS.
    let resolver = bound_dns_resolver
        .clone()
        .map(NetworkDnsResolver::Bound)
        .or_else(|| {
            (!http_ipv4 || !http_ipv6).then_some(NetworkDnsResolver::System(SystemDnsResolver))
        })
        .map(|resolver| Arc::new(FamilyFilteringResolver::new(resolver, http_ipv4, http_ipv6)));
    let tracker = build_generation_http_client(
        socket_factory
            .configure_http_client(reqwest::Client::builder(), resolver.clone())
            .redirect(http_redirect_policy(http_ipv4, http_ipv6))
            .user_agent(APP_USER_AGENT),
        http_ipv4,
        http_ipv6,
        build_http_client,
    );
    let general = build_generation_http_client(
        socket_factory
            .configure_http_client(reqwest::Client::builder(), resolver.clone())
            .redirect(http_redirect_policy(http_ipv4, http_ipv6))
            .user_agent(APP_USER_AGENT)
            .timeout(GENERAL_HTTP_REQUEST_TIMEOUT),
        http_ipv4,
        http_ipv6,
        build_http_client,
    );
    let rss_resolver = FamilyFilteringResolver::new(
        bound_dns_resolver
            .clone()
            .map(NetworkDnsResolver::Bound)
            .unwrap_or(NetworkDnsResolver::System(SystemDnsResolver)),
        http_ipv4,
        http_ipv6,
    );
    let rss = build_generation_http_client(
        configure_rss_http_client(
            socket_factory,
            reqwest::Client::builder(),
            rss_resolver,
            http_ipv4,
            http_ipv6,
        ),
        http_ipv4,
        http_ipv6,
        build_http_client,
    )
    .map(NetworkHttpClient::public_only);
    let web_seed = build_generation_http_client(
        socket_factory
            .configure_http_client(reqwest::Client::builder(), resolver)
            .redirect(http_redirect_policy(http_ipv4, http_ipv6))
            .user_agent(APP_USER_AGENT),
        http_ipv4,
        http_ipv6,
        build_http_client,
    );
    (tracker, general, rss, web_seed)
}

#[cfg(windows)]
fn build_windows_generation_http_clients(
    generation_id: u64,
    socket_factory: &SocketFactory,
    bound_dns_resolver: Option<Arc<BoundDnsResolver>>,
    invalidation_rx: watch::Receiver<bool>,
    build_http_client: &mut impl FnMut(reqwest::ClientBuilder) -> io::Result<reqwest::Client>,
) -> GenerationHttpClients {
    let resolver = bound_dns_resolver
        .map(NetworkDnsResolver::Bound)
        .unwrap_or(NetworkDnsResolver::System(SystemDnsResolver));
    let tracker = build_windows_generation_http_client(
        generation_id,
        socket_factory,
        resolver.clone(),
        invalidation_rx.clone(),
        false,
        None,
        build_http_client,
    );
    let general = build_windows_generation_http_client(
        generation_id,
        socket_factory,
        resolver.clone(),
        invalidation_rx.clone(),
        false,
        Some(GENERAL_HTTP_REQUEST_TIMEOUT),
        build_http_client,
    );
    let rss = build_windows_generation_http_client(
        generation_id,
        socket_factory,
        resolver.clone(),
        invalidation_rx.clone(),
        true,
        Some(GENERAL_HTTP_REQUEST_TIMEOUT),
        build_http_client,
    )
    .map(NetworkHttpClient::public_only);
    let web_seed = build_windows_generation_http_client(
        generation_id,
        socket_factory,
        resolver,
        invalidation_rx,
        false,
        None,
        build_http_client,
    );
    (tracker, general, rss, web_seed)
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn build_windows_generation_http_client(
    generation_id: u64,
    socket_factory: &SocketFactory,
    resolver: NetworkDnsResolver,
    invalidation_rx: watch::Receiver<bool>,
    public_only: bool,
    timeout: Option<std::time::Duration>,
    build_http_client: &mut impl FnMut(reqwest::ClientBuilder) -> io::Result<reqwest::Client>,
) -> Result<NetworkHttpClient, Arc<str>> {
    let build_family = |source: IpAddr,
                        ipv4: bool,
                        resolver: NetworkDnsResolver,
                        build_http_client: &mut _|
     -> Result<reqwest::Client, Arc<str>> {
        let family_resolver = FamilyFilteringResolver::new(resolver, ipv4, !ipv4);
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .local_address(source)
            .connect_timeout(WINDOWS_HTTP_FAMILY_CONNECT_TIMEOUT)
            .user_agent(APP_USER_AGENT);
        builder = if public_only {
            builder.dns_resolver(Arc::new(PublicFilteringResolver::new(family_resolver)))
        } else {
            builder.dns_resolver(Arc::new(family_resolver))
        };
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        build_reqwest_http_client(builder, build_http_client)
    };

    let ipv4 = if socket_factory.binding.ipv4.enabled {
        let source = socket_factory
            .binding
            .ipv4
            .effective_source
            .ok_or_else(|| {
                Arc::<str>::from("strict Windows IPv4 HTTP client has no effective source")
            })?;
        Some(build_family(
            IpAddr::V4(source),
            true,
            resolver.clone(),
            build_http_client,
        )?)
    } else {
        None
    };
    let ipv6 = if socket_factory.binding.ipv6.enabled {
        let source = socket_factory
            .binding
            .ipv6
            .effective_source
            .ok_or_else(|| {
                Arc::<str>::from("strict Windows IPv6 HTTP client has no effective source")
            })?;
        Some(build_family(
            IpAddr::V6(source),
            false,
            resolver,
            build_http_client,
        )?)
    } else {
        None
    };
    Ok(NetworkHttpClient::strict_windows(
        ipv4,
        ipv6,
        generation_id,
        invalidation_rx,
    ))
}

fn build_reqwest_http_client(
    builder: reqwest::ClientBuilder,
    build_http_client: &mut impl FnMut(reqwest::ClientBuilder) -> io::Result<reqwest::Client>,
) -> Result<reqwest::Client, Arc<str>> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_http_client(builder))) {
        Ok(Ok(client)) => Ok(client),
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
        self.generation.socket_factory.binding.ipv4.enabled
    }

    pub fn ipv6_enabled(&self) -> bool {
        self.generation.socket_factory.binding.ipv6.enabled
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
        let resolution = async {
            if let Some(resolver) = &self.generation.bound_dns_resolver {
                resolver.resolve_ips(host).await.map(|addresses| {
                    addresses
                        .into_iter()
                        .map(|address| SocketAddr::new(address, port))
                        .collect()
                })
            } else {
                lookup_host((host, port)).await.map(Iterator::collect)
            }
        };
        let addresses = self
            .cancel_on_invalidation(resolution)
            .await?
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
        self.block_generation_with_retry(generation_id, reason, false)
            .await
    }

    pub async fn block_generation_with_retry(
        &self,
        generation_id: u64,
        reason: impl Into<Arc<str>>,
        retry_binding: bool,
    ) -> Result<(), mpsc::error::SendError<()>> {
        self.command_tx
            .send(NetworkSupervisorCommand::BlockGeneration {
                generation_id,
                reason: NetworkBlockedReason::new(reason),
                retry_binding,
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
        retry_binding: bool,
    },
    Shutdown,
}

struct PlatformNetworkChangeMonitor {
    #[cfg(windows)]
    _notifier: Option<windows_backend::NetworkChangeNotifier>,
    #[cfg(windows)]
    receiver: Option<mpsc::UnboundedReceiver<()>>,
}

impl PlatformNetworkChangeMonitor {
    fn new() -> Self {
        #[cfg(windows)]
        {
            match windows_backend::NetworkChangeNotifier::new() {
                Ok((notifier, receiver)) => Self {
                    _notifier: Some(notifier),
                    receiver: Some(receiver),
                },
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "Windows network change notifications are unavailable; reconciliation polling remains active"
                    );
                    Self {
                        _notifier: None,
                        receiver: None,
                    }
                }
            }
        }
        #[cfg(not(windows))]
        Self {}
    }

    async fn changed(&mut self) {
        #[cfg(windows)]
        if let Some(receiver) = &mut self.receiver {
            if receiver.recv().await.is_some() {
                return;
            }
        }
        std::future::pending::<()>().await;
    }
}

#[derive(Debug)]
pub struct NetworkSupervisor {
    next_generation_id: AtomicU64,
    desired_epoch: u64,
    desired_config: NetworkBindingConfig,
    last_resolved_binding: Option<ResolvedNetworkBinding>,
    retry_blocked_binding: bool,
    state_tx: watch::Sender<NetworkState>,
    command_rx: mpsc::Receiver<NetworkSupervisorCommand>,
}

impl NetworkSupervisor {
    pub fn spawn_with_config(config: &NetworkBindingConfig) -> (NetworkHandle, JoinHandle<()>) {
        let initial_generation_id = supervisor_initial_generation_id();
        let resolved_binding = ResolvedNetworkBinding::resolve(config).ok();
        let (initial_state, last_resolved_binding, retry_blocked_binding) =
            initial_generation_state(
                NetworkGeneration::from_config(initial_generation_id, 1, config),
                resolved_binding,
            );
        let (state_tx, state_rx) = watch::channel(initial_state);
        let (command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let supervisor = Self {
            next_generation_id: AtomicU64::new(initial_generation_id + 1),
            desired_epoch: 1,
            desired_config: config.clone(),
            last_resolved_binding,
            retry_blocked_binding,
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
        let initial_generation_id = supervisor_initial_generation_id();
        let generation = Arc::new(NetworkGeneration::unrestricted(initial_generation_id)?);
        let (state_tx, state_rx) = watch::channel(NetworkState::Ready(generation));
        let (command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let supervisor = Self {
            next_generation_id: AtomicU64::new(initial_generation_id + 1),
            desired_epoch: 1,
            desired_config: NetworkBindingConfig::default(),
            last_resolved_binding: Some(ResolvedNetworkBinding::unrestricted()),
            retry_blocked_binding: false,
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
        let mut network_change_monitor = PlatformNetworkChangeMonitor::new();
        loop {
            let command = tokio::select! {
                biased;
                command = self.command_rx.recv() => command,
                _ = network_change_monitor.changed() => {
                    self.refresh_binding_snapshot();
                    continue;
                }
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
                    self.retry_blocked_binding = false;
                    self.invalidate_current();
                    self.state_tx.send_replace(NetworkState::Blocked(reason));
                }
                Some(NetworkSupervisorCommand::BlockGeneration {
                    generation_id,
                    reason,
                    retry_binding,
                }) => {
                    self.block_generation(generation_id, reason, retry_binding);
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
        if self.retry_blocked_binding
            && matches!(&*self.state_tx.borrow(), NetworkState::Blocked(_))
        {
            self.retry_blocked_binding = false;
            self.rebuild_desired("retrying transient listener binding failure");
            return;
        }
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

    fn block_generation(
        &mut self,
        generation_id: u64,
        reason: NetworkBlockedReason,
        retry_binding: bool,
    ) {
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
        self.retry_blocked_binding = retry_binding;
        self.invalidate_current();
        self.state_tx.send_replace(NetworkState::Blocked(reason));
    }

    fn rebuild_desired(&mut self, recovery_reason: &str) {
        self.retry_blocked_binding = false;
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
                // Resolution succeeded, so construction or preflight may recover
                // without producing a different interface snapshot.
                self.retry_blocked_binding = generation_build_failure_is_retryable(
                    self.last_resolved_binding.as_ref(),
                    &error,
                );
                self.state_tx
                    .send_replace(NetworkState::Blocked(NetworkBlockedReason::new(format!(
                        "network binding configuration could not be activated: {error}"
                    ))));
            }
        }
    }
}

#[cfg(not(test))]
fn supervisor_initial_generation_id() -> u64 {
    1
}

#[cfg(test)]
fn supervisor_initial_generation_id() -> u64 {
    NEXT_TEST_GENERATION_ID.fetch_add(TEST_GENERATION_ID_RANGE, Ordering::Relaxed)
}

fn generation_build_failure_is_retryable(
    resolved_binding: Option<&ResolvedNetworkBinding>,
    error: &io::Error,
) -> bool {
    resolved_binding.is_some()
        && !matches!(
            error.kind(),
            io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
        )
}

fn initial_generation_state(
    generation: io::Result<NetworkGeneration>,
    resolved_binding: Option<ResolvedNetworkBinding>,
) -> (NetworkState, Option<ResolvedNetworkBinding>, bool) {
    match generation {
        Ok(generation) => {
            let resolved_binding = Some(generation.socket_factory.binding.clone());
            (
                NetworkState::Ready(Arc::new(generation)),
                resolved_binding,
                false,
            )
        }
        Err(error) => {
            let retry_blocked_binding =
                generation_build_failure_is_retryable(resolved_binding.as_ref(), &error);
            (
                NetworkState::Blocked(NetworkBlockedReason::new(format!(
                    "network binding configuration could not be activated: {error}"
                ))),
                resolved_binding,
                retry_blocked_binding,
            )
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
    interface_identity: Option<Arc<str>>,
    interface_display_name: Option<Arc<str>>,
    ipv4: ResolvedAddressFamily<Ipv4Addr>,
    ipv6: ResolvedAddressFamily<Ipv6Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedAddressFamily<T> {
    enabled: bool,
    interface_index: Option<NonZeroU32>,
    configured_source: Option<T>,
    effective_source: Option<T>,
    eligible_sources: Arc<[T]>,
    host_policy: NetworkFamilyHostPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InterfaceAddressFamily<T> {
    interface_index: Option<NonZeroU32>,
    eligible_sources: Vec<T>,
    host_policy: NetworkFamilyHostPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InterfaceSnapshot {
    identity: Arc<str>,
    display_name: Arc<str>,
    ipv4: InterfaceAddressFamily<Ipv4Addr>,
    ipv6: InterfaceAddressFamily<Ipv6Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        self.verify_local_endpoint(stream.local_addr()?, addr)?;
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
        #[cfg(windows)]
        if self.binding.mode == NetworkBindingMode::Interface {
            windows_backend::set_exclusive_address_use(&socket)?;
        }
        socket.bind(&SockAddr::from(self.bound_local_addr(addr)?))?;
        if let Some(local_addr) = socket.local_addr()?.as_socket() {
            self.verify_local_endpoint(local_addr, addr)?;
        }
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
        if let Some(local_addr) = socket.local_addr()?.as_socket() {
            self.verify_local_endpoint(local_addr, addr)?;
        }
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
        if self.binding.ipv4.enabled {
            addresses.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
        }
        if self.binding.ipv6.enabled {
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
            SocketAddr::V4(_) => self.binding.ipv4.effective_source.map(IpAddr::V4),
            SocketAddr::V6(_) => self.binding.ipv6.effective_source.map(IpAddr::V6),
        }
    }

    fn source_socket_addr(&self, source: IpAddr, port: u16) -> io::Result<SocketAddr> {
        match source {
            IpAddr::V6(address) if address.is_unicast_link_local() => {
                let scope_id = self.binding.ipv6.interface_index.ok_or_else(|| {
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
            SocketAddr::V4(_) => self.binding.ipv4.enabled,
            SocketAddr::V6(_) => self.binding.ipv6.enabled,
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

    fn verify_local_endpoint(
        &self,
        local: SocketAddr,
        requested_family: SocketAddr,
    ) -> io::Result<()> {
        #[cfg(not(windows))]
        let _ = local;
        #[cfg(windows)]
        if self.binding.mode == NetworkBindingMode::Interface {
            let expected = self.source_address(requested_family).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "strict Windows interface binding has no effective source address",
                )
            })?;
            if normalize_ip_address(local.ip()) != expected {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "Windows socket local endpoint {} does not match selected source {expected}",
                        local.ip()
                    ),
                ));
            }
        }
        let _ = requested_family;
        Ok(())
    }

    fn apply_interface_binding(&self, socket: &Socket, addr: SocketAddr) -> io::Result<()> {
        self.ensure_family_enabled(addr)?;
        let Some(_interface_name) = self.binding.interface_identity.as_deref() else {
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
            let index = match addr {
                SocketAddr::V4(_) => self.binding.ipv4.interface_index,
                SocketAddr::V6(_) => self.binding.ipv6.interface_index,
            }
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "resolved interface has no index")
            })?;
            match addr {
                SocketAddr::V4(_) => socket.bind_device_by_index_v4(Some(index)),
                SocketAddr::V6(_) => socket.bind_device_by_index_v6(Some(index)),
            }
        }

        #[cfg(windows)]
        {
            let index = match addr {
                SocketAddr::V4(_) => self.binding.ipv4.interface_index,
                SocketAddr::V6(_) => self.binding.ipv6.interface_index,
            }
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "resolved interface has no family index",
                )
            })?;
            windows_backend::apply_interface_binding(socket, addr, index)
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
            target_os = "watchos",
            windows
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
        if let Some(local_address) = self.binding.http_local_address() {
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
        if let Some(interface_name) = self.binding.interface_identity.as_deref() {
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
            && self.interface_identity == other.interface_identity
            && self.interface_display_name == other.interface_display_name
            && self.ipv4.generation_equivalent(&other.ipv4)
            && self.ipv6.generation_equivalent(&other.ipv6)
    }

    fn unrestricted() -> Self {
        Self {
            mode: NetworkBindingMode::Any,
            interface_identity: None,
            interface_display_name: None,
            ipv4: ResolvedAddressFamily::unrestricted(),
            ipv6: ResolvedAddressFamily::unrestricted(),
        }
    }

    fn http_local_address(&self) -> Option<IpAddr> {
        match (self.ipv4.enabled, self.ipv6.enabled) {
            (true, false) => Some(IpAddr::V4(
                self.ipv4.effective_source.unwrap_or(Ipv4Addr::UNSPECIFIED),
            )),
            (false, true) => Some(IpAddr::V6(
                self.ipv6.effective_source.unwrap_or(Ipv6Addr::UNSPECIFIED),
            )),
            _ => None,
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
        if config.enable_ipv4 && snapshot.ipv4.eligible_sources.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("interface {interface_name} has no IPv4 address"),
            ));
        }
        if config.enable_ipv6 && snapshot.ipv6.eligible_sources.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("interface {interface_name} has no IPv6 address"),
            ));
        }
        #[cfg(windows)]
        {
            if config.enable_ipv4 {
                if snapshot.ipv4.interface_index.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        format!("Windows adapter {interface_name} has no live IPv4 index"),
                    ));
                }
                windows_backend::validate_host_policy("IPv4", snapshot.ipv4.host_policy)?;
            }
            if config.enable_ipv6 {
                if snapshot.ipv6.interface_index.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        format!("Windows adapter {interface_name} has no live IPv6 index"),
                    ));
                }
                windows_backend::validate_host_policy("IPv6", snapshot.ipv6.host_policy)?;
            }
        }
        validate_explicit_addresses(config, Some((&snapshot, interface_name)))?;
        reject_dual_family_exact_source(config)?;

        let InterfaceSnapshot {
            identity,
            display_name,
            ipv4,
            ipv6,
        } = snapshot;
        #[cfg(windows)]
        let ipv4_effective_source = config
            .enable_ipv4
            .then(|| {
                windows_backend::select_effective_ipv4_source(
                    config.ipv4_address,
                    &ipv4.eligible_sources,
                )
            })
            .flatten();
        #[cfg(not(windows))]
        let ipv4_effective_source = config.ipv4_address;
        #[cfg(windows)]
        let ipv6_effective_source = config
            .enable_ipv6
            .then(|| {
                windows_backend::select_effective_source(
                    config.ipv6_address,
                    &ipv6.eligible_sources,
                )
            })
            .flatten();
        #[cfg(not(windows))]
        let ipv6_effective_source = config.ipv6_address;
        Ok(Self {
            mode: NetworkBindingMode::Interface,
            interface_identity: Some(identity),
            interface_display_name: Some(display_name),
            ipv4: ResolvedAddressFamily {
                enabled: config.enable_ipv4,
                interface_index: ipv4.interface_index,
                configured_source: config.ipv4_address,
                effective_source: ipv4_effective_source,
                eligible_sources: Arc::from(ipv4.eligible_sources),
                host_policy: ipv4.host_policy,
            },
            ipv6: ResolvedAddressFamily {
                enabled: config.enable_ipv6,
                interface_index: ipv6.interface_index,
                configured_source: config.ipv6_address,
                effective_source: ipv6_effective_source,
                eligible_sources: Arc::from(ipv6.eligible_sources),
                host_policy: ipv6.host_policy,
            },
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
            interface_identity: None,
            interface_display_name: None,
            ipv4: ResolvedAddressFamily {
                enabled: config.enable_ipv4,
                interface_index: None,
                configured_source: config.ipv4_address,
                effective_source: config.ipv4_address,
                eligible_sources: Arc::from([]),
                host_policy: NetworkFamilyHostPolicy::default(),
            },
            ipv6: ResolvedAddressFamily {
                enabled: config.enable_ipv6,
                interface_index: None,
                configured_source: config.ipv6_address,
                effective_source: config.ipv6_address,
                eligible_sources: Arc::from([]),
                host_policy: NetworkFamilyHostPolicy::default(),
            },
        })
    }
}

impl<T: PartialEq> ResolvedAddressFamily<T> {
    fn generation_equivalent(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && (!self.enabled
                || (self.interface_index == other.interface_index
                    && self.configured_source == other.configured_source
                    && self.effective_source == other.effective_source
                    && self.host_policy == other.host_policy
                    && (self.configured_source.is_some()
                        || self.eligible_sources == other.eligible_sources)))
    }
}

impl<T> ResolvedAddressFamily<T> {
    fn unrestricted() -> Self {
        Self {
            enabled: true,
            interface_index: None,
            configured_source: None,
            effective_source: None,
            eligible_sources: Arc::from([]),
            host_policy: NetworkFamilyHostPolicy::default(),
        }
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
    if !DUAL_FAMILY_EXACT_SOURCE_SUPPORTED
        && config.enable_ipv4
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
            if !snapshot.ipv4.eligible_sources.contains(&address) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("IPv4 address {address} is not assigned to interface {interface_name}"),
                ));
            }
        }
        if let Some(address) = config.ipv6_address {
            if !snapshot.ipv6.eligible_sources.contains(&address) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("IPv6 address {address} is not assigned to interface {interface_name}"),
                ));
            }
        }
    }
    Ok(())
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
    let mut is_loopback = false;
    visit_interface_addresses(|name, address, is_up, address_is_loopback| {
        if name == interface_name {
            is_loopback |= address_is_loopback;
            if is_up {
                match address {
                    IpAddr::V4(address) => ipv4_addresses.push(address),
                    IpAddr::V6(address) => ipv6_addresses.push(address),
                }
            }
        }
    })?;
    if is_loopback {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("interface {interface_name} is a loopback device"),
        ));
    }
    ipv4_addresses.sort_unstable();
    ipv4_addresses.dedup();
    ipv6_addresses.sort_unstable();
    ipv6_addresses.dedup();
    Ok(InterfaceSnapshot {
        identity: Arc::from(interface_name),
        display_name: Arc::from(interface_name),
        ipv4: InterfaceAddressFamily {
            interface_index: Some(index),
            eligible_sources: ipv4_addresses,
            host_policy: NetworkFamilyHostPolicy::default(),
        },
        ipv6: InterfaceAddressFamily {
            interface_index: Some(index),
            eligible_sources: ipv6_addresses,
            host_policy: NetworkFamilyHostPolicy::default(),
        },
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
                    identity: name.to_string(),
                    display_name: name.to_string(),
                    ipv4_index: None,
                    ipv6_index: None,
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
        let name = CString::new(interface.identity.as_str()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "operating-system interface name contains an interior NUL byte",
            )
        })?;
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if index == 0 {
            continue;
        }
        interface.ipv4_addresses.sort_unstable();
        interface.ipv4_addresses.dedup();
        interface.ipv6_addresses.sort_unstable();
        interface.ipv6_addresses.dedup();
        interface.ipv4_index = (!interface.ipv4_addresses.is_empty()).then_some(index);
        interface.ipv6_index = (!interface.ipv6_addresses.is_empty()).then_some(index);
        discovered.push(interface);
    }
    Ok(discovered)
}

#[cfg(all(not(unix), not(windows)))]
pub fn available_network_interfaces() -> io::Result<Vec<NetworkInterfaceInfo>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "network interface discovery is not supported on this operating system",
    ))
}

#[cfg(windows)]
pub fn available_network_interfaces() -> io::Result<Vec<NetworkInterfaceInfo>> {
    windows_backend::available_network_interfaces()
}

#[cfg(all(not(unix), not(windows)))]
fn interface_snapshot(interface_name: &str) -> io::Result<InterfaceSnapshot> {
    let _ = interface_name;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "strict interface binding is not supported on this operating system",
    ))
}

#[cfg(windows)]
fn interface_snapshot(interface_name: &str) -> io::Result<InterfaceSnapshot> {
    windows_backend::interface_snapshot(interface_name)
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
    windows_backend::all_interface_addresses()
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
                target_os = "watchos",
                target_os = "windows"
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
        let test_client = || {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build test HTTP client")
        };
        let client = NetworkHttpClient::new(test_client(), true, false);

        assert!(client.get("http://192.0.2.10/").is_ok());
        assert!(client.get("http://[::ffff:192.0.2.10]/").is_err());
        let error = client
            .get("http://[2001:db8::10]/")
            .expect_err("IPv6 literal must be rejected when IPv6 is disabled");

        assert!(matches!(
            &error,
            NetworkLeaseError::HttpRequestRejected { .. }
        ));
        assert!(error.to_string().contains("disabled address family"));

        let client = NetworkHttpClient::new(test_client(), false, true);
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

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn unrestricted_rss_http_client_preserves_configured_proxy_hops() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind RSS proxy");
        let proxy_address = proxy.local_addr().expect("read RSS proxy address");
        let proxy_task = tokio::spawn(async move {
            let (mut stream, _) = proxy.accept().await.expect("accept proxied RSS request");
            let mut request = [0_u8; 1024];
            let read = stream
                .read(&mut request)
                .await
                .expect("read proxied RSS request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write proxied RSS response");
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        let factory = SocketFactory::from_config(&NetworkBindingConfig::default())
            .expect("build unrestricted socket factory");
        let resolver = FamilyFilteringResolver::new(
            NetworkDnsResolver::Fixed(vec![proxy_address]),
            true,
            true,
        );
        let builder = configure_rss_http_client(
            &factory,
            reqwest::Client::builder().proxy(
                reqwest::Proxy::all(format!("http://proxy.test:{}", proxy_address.port()))
                    .expect("configure RSS proxy"),
            ),
            resolver,
            true,
            true,
        );
        let client = NetworkHttpClient::new(
            builder.build().expect("build unrestricted RSS client"),
            true,
            true,
        )
        .public_only();

        let response = client
            .get("http://feed.test/rss")
            .expect("build public RSS request")
            .send()
            .await
            .expect("send RSS request through configured proxy");

        assert!(response.status().is_success());
        let request = proxy_task.await.expect("join RSS proxy task");
        assert!(request.starts_with("GET http://feed.test/rss "));
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

    #[cfg(windows)]
    fn windows_strict_http_test_client() -> (NetworkHttpClient, watch::Sender<bool>) {
        let (invalidation_tx, invalidation_rx) = watch::channel(false);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .local_address(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .build()
            .expect("build strict Windows test HTTP client");
        (
            NetworkHttpClient::strict_windows(Some(client), None, 41, invalidation_rx),
            invalidation_tx,
        )
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_strict_http_follows_relative_redirect_and_preserves_safe_headers() {
        use reqwest::header::{AUTHORIZATION, RANGE};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind redirect fixture");
        let address = listener.local_addr().expect("read fixture address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in [
                b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .as_slice(),
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).await.expect("read request");
                    request.extend_from_slice(&chunk[..read]);
                    if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
                stream.write_all(response).await.expect("write response");
            }
            requests
        });
        let (client, _invalidation_tx) = windows_strict_http_test_client();
        let response = client
            .get(format!("http://{address}/start"))
            .expect("build strict request")
            .header(RANGE, "bytes=0-15")
            .header(AUTHORIZATION, "Bearer fixture-token")
            .send()
            .await
            .expect("follow strict redirect");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let requests = server.await.expect("join redirect fixture");
        assert!(requests[1].starts_with("get /final http/1.1"));
        assert!(requests[1].contains("range: bytes=0-15"));
        assert!(requests[1].contains("authorization: bearer fixture-token"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_strict_http_preserves_url_credentials_on_same_origin_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind credential redirect fixture");
        let address = listener.local_addr().expect("read fixture address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for hop in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).await.expect("read request");
                    request.extend_from_slice(&chunk[..read]);
                    if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
                let response = if hop == 0 {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{address}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            requests
        });
        let (client, _invalidation_tx) = windows_strict_http_test_client();
        let response = client
            .get(format!("http://fixture-user:fixture-pass@{address}/start"))
            .expect("build credentialed strict request")
            .send()
            .await
            .expect("follow same-origin credential redirect");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let requests = server.await.expect("join credential redirect fixture");
        let authorization = |request: &str| {
            request
                .lines()
                .find(|line| line.starts_with("authorization:"))
                .map(str::to_owned)
        };
        let first = authorization(&requests[0]).expect("first request authorization");
        assert_eq!(authorization(&requests[1]).as_deref(), Some(first.as_str()));
    }

    #[test]
    fn redirect_headers_strip_sensitive_and_hop_by_hop_values() {
        use reqwest::header::{
            HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONNECTION, COOKIE, RANGE,
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer fixture-token"),
        );
        headers.insert(COOKIE, HeaderValue::from_static("session=fixture"));
        headers.insert(RANGE, HeaderValue::from_static("bytes=16-31"));
        headers.insert(CONNECTION, HeaderValue::from_static("x-remove"));
        headers.insert(
            HeaderName::from_static("x-remove"),
            HeaderValue::from_static("hop"),
        );
        let previous = reqwest::Url::parse("https://source.test:8443/start").unwrap();
        let next = reqwest::Url::parse("https://destination.test:8443/final").unwrap();
        let headers = redirect_headers(headers, &previous, &next);

        assert!(!headers.contains_key(AUTHORIZATION));
        assert!(!headers.contains_key(COOKIE));
        assert!(!headers.contains_key(CONNECTION));
        assert!(!headers.contains_key("x-remove"));
        assert_eq!(headers.get(RANGE).unwrap(), "bytes=16-31");
    }

    #[test]
    fn redirect_origin_comparison_uses_scheme_host_and_effective_port() {
        let base = reqwest::Url::parse("https://source.test/start").unwrap();
        let same_origin = reqwest::Url::parse("https://SOURCE.test/final").unwrap();
        let cross_port = reqwest::Url::parse("https://source.test:8443/final").unwrap();
        let cross_scheme = reqwest::Url::parse("http://source.test/final").unwrap();
        let scheme_only = reqwest::Url::parse("http://source.test:443/final").unwrap();

        assert!(!http_origin_changed(&base, &same_origin));
        assert!(http_origin_changed(&base, &cross_port));
        assert!(http_origin_changed(&base, &cross_scheme));
        assert!(http_origin_changed(&base, &scheme_only));
    }

    #[test]
    fn redirect_headers_strip_credentials_when_only_the_scheme_changes() {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, COOKIE, RANGE};

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer fixture-token"),
        );
        headers.insert(COOKIE, HeaderValue::from_static("session=fixture"));
        headers.insert(RANGE, HeaderValue::from_static("bytes=0-15"));
        let previous = reqwest::Url::parse("https://source.test/start").unwrap();
        let next = reqwest::Url::parse("http://source.test:443/final").unwrap();

        let headers = redirect_headers(headers, &previous, &next);

        assert!(!headers.contains_key(AUTHORIZATION));
        assert!(!headers.contains_key(COOKIE));
        assert_eq!(headers.get(RANGE).unwrap(), "bytes=0-15");
    }

    #[test]
    fn http_url_family_rejects_ipv4_mapped_ipv6_literals() {
        let url = reqwest::Url::parse("http://[::ffff:192.0.2.42]/fixture").unwrap();

        let error = validate_http_url_family(&url, true, false)
            .expect_err("mapped IPv6 literal must not reach an IPv4-bound HTTP client");

        let NetworkLeaseError::HttpRequestRejected { reason, .. } = error else {
            panic!("expected HTTP policy rejection");
        };
        assert_eq!(
            reason.as_ref(),
            "IPv4-mapped IPv6 URL literals are not supported"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_strict_http_rejects_redirect_loops() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind redirect-loop fixture");
        let address = listener.local_addr().expect("read fixture address");
        let server = tokio::spawn(async move {
            for location in ["/second", "/first"] {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await.expect("read request");
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write redirect");
            }
        });
        let (client, _invalidation_tx) = windows_strict_http_test_client();
        let error = client
            .get(format!("http://{address}/first"))
            .expect("build strict request")
            .send()
            .await
            .expect_err("redirect loop must fail");
        assert!(matches!(
            error,
            NetworkHttpRequestError::Policy(NetworkLeaseError::HttpRequestRejected { .. })
        ));
        server.await.expect("join redirect-loop fixture");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_strict_http_enforces_redirect_hop_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind redirect-limit fixture");
        let address = listener.local_addr().expect("read fixture address");
        let server = tokio::spawn(async move {
            for hop in 0..=WINDOWS_HTTP_REDIRECT_LIMIT {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await.expect("read request");
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: /hop{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    hop + 1
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write redirect");
            }
        });
        let (client, _invalidation_tx) = windows_strict_http_test_client();
        let error = client
            .get(format!("http://{address}/hop0"))
            .expect("build strict request")
            .send()
            .await
            .expect_err("redirect limit must fail");
        let NetworkHttpRequestError::Policy(NetworkLeaseError::HttpRequestRejected {
            reason, ..
        }) = error
        else {
            panic!("unexpected redirect-limit error");
        };
        assert!(reason.contains("redirect limit"));
        server.await.expect("join redirect-limit fixture");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_strict_http_falls_back_family_only_after_transport_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))
            .expect("create IPv6 fallback fixture");
        socket.set_only_v6(true).expect("make fixture IPv6-only");
        socket
            .bind(&SockAddr::from(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                0,
            )))
            .expect("bind IPv6 fallback fixture");
        socket.listen(8).expect("listen on IPv6 fixture");
        socket.set_nonblocking(true).unwrap();
        let listener = TcpListener::from_std(socket.into()).expect("adopt IPv6 fixture");
        let address = listener.local_addr().expect("read fixture address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept fallback request");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write response");
        });
        let ipv4 = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .local_address(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .resolve(
                "family-fixture.test",
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port()),
            )
            .build()
            .expect("build IPv4 fixture client");
        let ipv6 = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .local_address(IpAddr::V6(Ipv6Addr::LOCALHOST))
            .resolve("family-fixture.test", address)
            .build()
            .expect("build IPv6 fixture client");
        let (invalidation_tx, invalidation_rx) = watch::channel(false);
        let client = NetworkHttpClient::strict_windows(Some(ipv4), Some(ipv6), 42, invalidation_rx);
        let response = client
            .get(format!("http://family-fixture.test:{}/", address.port()))
            .expect("build fallback request")
            .send()
            .await
            .expect("fall back to IPv6 after IPv4 transport failure");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        drop(invalidation_tx);
        server.await.expect("join fallback fixture");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_strict_http_does_not_fallback_after_resolver_policy_failure() {
        let listener = TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
            .await
            .expect("bind forbidden fallback fixture");
        let address = listener.local_addr().expect("read fixture address");
        let private_answer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port());
        let ipv4_resolver = Arc::new(PublicFilteringResolver::new(FamilyFilteringResolver::new(
            NetworkDnsResolver::Fixed(vec![private_answer]),
            true,
            false,
        )));
        let ipv4 = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .local_address(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .dns_resolver(ipv4_resolver)
            .build()
            .expect("build rejecting IPv4 client");
        let ipv6 = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .local_address(IpAddr::V6(Ipv6Addr::LOCALHOST))
            .resolve("policy-fixture.test", address)
            .build()
            .expect("build IPv6 fallback client");
        let (_invalidation_tx, invalidation_rx) = watch::channel(false);
        let client = NetworkHttpClient::strict_windows(Some(ipv4), Some(ipv6), 43, invalidation_rx)
            .public_only();
        let error = client
            .get(format!("http://policy-fixture.test:{}/", address.port()))
            .expect("build policy request")
            .send()
            .await
            .expect_err("resolver policy failure must be terminal");
        assert!(matches!(
            error,
            NetworkHttpRequestError::Policy(NetworkLeaseError::HttpRequestRejected { .. })
        ));
        assert!(time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_strict_http_invalidation_cancels_an_inflight_request() {
        use tokio::io::AsyncReadExt;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind cancellation fixture");
        let address = listener.local_addr().expect("read fixture address");
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            let _ = accepted_tx.send(());
            time::sleep(Duration::from_secs(5)).await;
        });
        let (client, invalidation_tx) = windows_strict_http_test_client();
        let request = client
            .get(format!("http://{address}/wait"))
            .expect("build strict request");
        let request_task = tokio::spawn(request.send());
        accepted_rx.await.expect("observe accepted request");
        invalidation_tx.send_replace(true);
        let error = request_task
            .await
            .expect("join request")
            .expect_err("invalidation must cancel request");
        assert!(matches!(
            error,
            NetworkHttpRequestError::Policy(NetworkLeaseError::Invalidated { generation_id: 41 })
        ));
        server.abort();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_windows_factory_binds_listeners_and_udp_to_the_selected_source() {
        let interfaces = available_network_interfaces().expect("discover Windows interfaces");
        let mut activated = None;
        for interface in interfaces
            .into_iter()
            .filter(|interface| interface.is_up && !interface.is_loopback)
        {
            let (enable_ipv4, enable_ipv6, expected_source) =
                if let Some(address) = interface.ipv4_addresses.first().copied() {
                    (true, false, IpAddr::V4(address))
                } else if let Some(address) = interface.ipv6_addresses.first().copied() {
                    (false, true, IpAddr::V6(address))
                } else {
                    continue;
                };
            let config = NetworkBindingConfig {
                mode: NetworkBindingMode::Interface,
                interface: Some(interface.identity),
                enable_ipv4,
                enable_ipv6,
                ipv4_address: None,
                ipv6_address: None,
                dns_policy: DnsPolicy::System,
                dns_servers: Vec::new(),
            };
            let Ok(factory) = SocketFactory::from_config(&config) else {
                continue;
            };
            factory
                .preflight()
                .expect("preflight native Windows factory");
            activated = Some((factory, expected_source));
            break;
        }
        let Some((factory, expected_source)) = activated else {
            return;
        };

        let requested = SocketAddr::new(
            if expected_source.is_ipv4() {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            } else {
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            },
            0,
        );
        let udp = factory
            .bind_udp(requested)
            .expect("bind strict Windows UDP");
        assert_eq!(udp.local_addr().unwrap().ip(), expected_source);
        let listener = factory
            .bind_tcp_listener(requested)
            .await
            .expect("bind strict Windows TCP listener");
        assert_eq!(listener.local_addr().unwrap().ip(), expected_source);
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
        assert!(interfaces
            .iter()
            .all(|interface| { interface.ipv4_index.is_some() || interface.ipv6_index.is_some() }));
        assert!(interfaces.iter().all(|interface| {
            !interface.ipv4_addresses.is_empty() || !interface.ipv6_addresses.is_empty()
        }));
        assert!(interfaces
            .windows(2)
            .all(|pair| pair[0].identity < pair[1].identity));
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

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires the elevated integration_tests/network_binding/run_windows_host_validation.ps1 harness"]
    async fn windows_host_strict_binding_probe() {
        use std::env;
        use tokio::io::AsyncWriteExt;

        fn required_env(name: &str) -> String {
            env::var(name).unwrap_or_else(|_| panic!("{name} must be set by the Windows harness"))
        }

        let case = required_env("SUPERSEEDR_WINDOWS_CASE");
        let interface = required_env("SUPERSEEDR_WINDOWS_INTERFACE");
        let address_family =
            env::var("SUPERSEEDR_WINDOWS_FAMILY").unwrap_or_else(|_| "ipv4".to_string());
        let (enable_ipv4, enable_ipv6) = match address_family.as_str() {
            "ipv4" => (true, false),
            "ipv6" => (false, true),
            "dual" => (true, true),
            other => panic!("unsupported Windows probe family {other}"),
        };
        let dns_server = env::var("SUPERSEEDR_WINDOWS_DNS_SERVER").ok().map(|value| {
            value
                .parse::<SocketAddr>()
                .expect("parse Windows DNS server")
        });
        let unrestricted = case.starts_with("any-");
        let config = NetworkBindingConfig {
            mode: if unrestricted {
                NetworkBindingMode::Any
            } else {
                NetworkBindingMode::Interface
            },
            interface: (!unrestricted).then_some(interface),
            enable_ipv4,
            enable_ipv6,
            ipv4_address: None,
            ipv6_address: None,
            dns_policy: if case == "bound-dns" {
                DnsPolicy::Bound
            } else {
                DnsPolicy::System
            },
            dns_servers: dns_server.into_iter().collect(),
        };
        let (handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&config);

        if case == "activation-blocked" {
            let error = handle
                .try_lease()
                .expect_err("Windows strict activation should fail closed");
            if let Ok(expected) = env::var("SUPERSEEDR_WINDOWS_EXPECTED_ERROR") {
                assert!(
                    error.to_string().contains(&expected),
                    "blocked reason {error:?} did not contain {expected:?}"
                );
            }
            println!("WINDOWS_BINDING_PROBE case={case} blocked={error}");
            handle
                .shutdown()
                .await
                .expect("shutdown blocked supervisor");
            supervisor_task.await.expect("join blocked supervisor");
            return;
        }

        let lease = handle
            .try_lease()
            .expect("Windows strict generation should be ready");
        let status = handle.subscribe().borrow().runtime_status(&config);
        let mut selected_sources = [
            status.selected_ipv4_address.map(IpAddr::V4),
            status.selected_ipv6_address.map(IpAddr::V6),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !unrestricted {
            assert!(
                !selected_sources.is_empty(),
                "strict probe selected no source"
            );
        }

        match case.as_str() {
            "tcp" | "any-tcp" => {
                let target = required_env("SUPERSEEDR_WINDOWS_TCP_TARGET")
                    .parse::<SocketAddr>()
                    .expect("parse Windows TCP target");
                let mut stream = lease
                    .connect_tcp(target)
                    .await
                    .expect("connect strict Windows TCP probe");
                let local_source = stream.local_addr().unwrap().ip();
                if unrestricted {
                    selected_sources.push(local_source);
                } else {
                    assert!(selected_sources.contains(&local_source));
                }
                stream
                    .write_all(b"windows-binding-probe")
                    .await
                    .expect("write strict Windows TCP probe");
            }
            "peer-tcp" => {
                let target = required_env("SUPERSEEDR_WINDOWS_TCP_TARGET")
                    .parse::<SocketAddr>()
                    .expect("parse Windows peer TCP target");
                let connection =
                    crate::networking::transport::TcpPeerTransport::connect(&lease, target)
                        .await
                        .expect("connect Windows TCP peer transport");
                assert_eq!(connection.remote_addr, target);
            }
            "udp" => {
                let target = required_env("SUPERSEEDR_WINDOWS_UDP_TARGET")
                    .parse::<SocketAddr>()
                    .expect("parse Windows UDP target");
                let bind_address = if target.is_ipv4() {
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
                } else {
                    SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
                };
                let socket = lease
                    .bind_udp(bind_address)
                    .await
                    .expect("bind strict Windows UDP probe");
                assert!(selected_sources.contains(&socket.local_addr().unwrap().ip()));
                let query = [
                    0x51, 0x4c, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07,
                    b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00, 0x00,
                    0x01, 0x00, 0x01,
                ];
                socket
                    .send_to(&query, target)
                    .await
                    .expect("send strict Windows UDP probe");
                let mut response = [0_u8; 512];
                let (received, peer) =
                    time::timeout(Duration::from_secs(10), socket.recv_from(&mut response))
                        .await
                        .expect("strict Windows UDP response timed out")
                        .expect("receive strict Windows UDP response");
                assert!(received >= 12);
                assert_eq!(normalize_socket_addr(peer), normalize_socket_addr(target));
            }
            "dht" => {
                #[cfg(feature = "dht")]
                {
                    use crate::dht::krpc::{KrpcPingArgs, KrpcQueryKind};
                    use crate::dht::transport::{TransportActor, TransportConfig};
                    use crate::dht::types::{AddressFamily, NodeId};

                    let target = required_env("SUPERSEEDR_WINDOWS_UDP_TARGET")
                        .parse::<SocketAddr>()
                        .expect("parse Windows DHT target");
                    let family = if target.is_ipv4() {
                        AddressFamily::Ipv4
                    } else {
                        AddressFamily::Ipv6
                    };
                    let bind_addr = if target.is_ipv4() {
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
                    } else {
                        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
                    };
                    let (transport, _events) = TransportActor::bind(
                        &lease,
                        TransportConfig {
                            family,
                            bind_addr,
                            query_timeout: Duration::from_secs(2),
                            ..TransportConfig::default()
                        },
                    )
                    .await
                    .expect("bind Windows DHT transport");
                    assert!(selected_sources.contains(&transport.local_addr().unwrap().ip()));
                    for byte in [0x51, 0x52, 0x53] {
                        transport
                            .send_query_deferred(
                                target,
                                KrpcQueryKind::Ping,
                                KrpcPingArgs::new(NodeId::new([byte; 20])),
                            )
                            .await
                            .expect("send Windows DHT query");
                        time::sleep(Duration::from_millis(150)).await;
                    }
                }
                #[cfg(not(feature = "dht"))]
                panic!("Windows DHT probe requires the dht feature");
            }
            "utp" => {
                let target = required_env("SUPERSEEDR_WINDOWS_UDP_TARGET")
                    .parse::<SocketAddr>()
                    .expect("parse Windows uTP target");
                match time::timeout(
                    Duration::from_secs(5),
                    crate::networking::utp::UtpPeerTransport::connect(&lease, target),
                )
                .await
                {
                    Ok(Ok(_connection)) => println!("WINDOWS_BINDING_PROBE uTP connected"),
                    Ok(Err(error)) if error.kind() == io::ErrorKind::TimedOut => {
                        println!("WINDOWS_BINDING_PROBE uTP peer did not respond")
                    }
                    Ok(Err(error)) => panic!("Windows uTP connect failed before capture: {error}"),
                    Err(_) => println!("WINDOWS_BINDING_PROBE uTP connect timed out after send"),
                }
            }
            "udp-tracker" => {
                let url = required_env("SUPERSEEDR_WINDOWS_UDP_TRACKER_URL");
                match time::timeout(
                    Duration::from_secs(8),
                    crate::tracker::client::announce_started(
                        &lease,
                        url,
                        &[0x52; 20],
                        "-SS1000-QUALIFY-0000".to_string(),
                        49_151,
                        1,
                    ),
                )
                .await
                {
                    Ok(Ok(_response)) => println!("WINDOWS_BINDING_PROBE UDP tracker responded"),
                    Ok(Err(error)) => println!("WINDOWS_BINDING_PROBE UDP tracker ended: {error}"),
                    Err(_) => println!("WINDOWS_BINDING_PROBE UDP tracker timed out after send"),
                }
            }
            "bound-dns" => {
                let host = env::var("SUPERSEEDR_WINDOWS_DNS_HOST")
                    .unwrap_or_else(|_| "example.com".to_string());
                for _ in 0..3 {
                    let addresses = lease
                        .resolve(&host, 443)
                        .await
                        .expect("resolve through strict Windows bound DNS");
                    assert!(!addresses.is_empty());
                }
            }
            "listener" | "identity-rename" => {
                for source in &selected_sources {
                    let listener = lease
                        .bind_tcp_listener(SocketAddr::new(
                            if source.is_ipv4() {
                                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                            } else {
                                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
                            },
                            0,
                        ))
                        .await
                        .expect("bind strict Windows listener");
                    assert_eq!(listener.local_addr().unwrap().ip(), *source);
                }
            }
            "recovery" => {
                let target = required_env("SUPERSEEDR_WINDOWS_TCP_TARGET")
                    .parse::<SocketAddr>()
                    .expect("parse Windows recovery target");
                let marker_directory =
                    std::path::PathBuf::from(required_env("SUPERSEEDR_WINDOWS_MARKER_DIRECTORY"));
                let initial_generation = lease.generation_id();
                let mut invalidation_rx = lease.subscribe_invalidation();
                let mut state_rx = handle.subscribe();
                let initial_stream = lease
                    .connect_tcp(target)
                    .await
                    .expect("initial Windows generation should connect");
                assert!(selected_sources.contains(&initial_stream.local_addr().unwrap().ip()));
                drop(initial_stream);
                std::fs::write(
                    marker_directory.join("ready.marker"),
                    initial_generation.to_string(),
                )
                .expect("publish initial Windows generation marker");

                time::timeout(Duration::from_secs(45), async {
                    loop {
                        if matches!(&*state_rx.borrow(), NetworkState::Blocked(_)) {
                            break;
                        }
                        state_rx
                            .changed()
                            .await
                            .expect("Windows recovery state channel open");
                    }
                })
                .await
                .expect("Windows generation did not block after adapter loss");
                time::timeout(Duration::from_secs(5), invalidation_rx.changed())
                    .await
                    .expect("old Windows generation invalidation timed out")
                    .expect("old Windows generation invalidation channel open");
                assert!(*invalidation_rx.borrow());
                assert!(matches!(
                    handle.try_lease(),
                    Err(NetworkLeaseError::Blocked(_))
                ));
                assert!(lease.connect_tcp(target).await.is_err());
                std::fs::write(
                    marker_directory.join("blocked.marker"),
                    initial_generation.to_string(),
                )
                .expect("publish blocked Windows generation marker");

                let replacement = time::timeout(Duration::from_secs(60), async {
                    loop {
                        if let Ok(candidate) = handle.try_lease() {
                            if candidate.generation_id() > initial_generation {
                                break candidate;
                            }
                        }
                        state_rx
                            .changed()
                            .await
                            .expect("Windows recovery state channel open");
                    }
                })
                .await
                .expect("Windows generation did not recover after adapter restoration");
                let replacement_generation = replacement.generation_id();
                let stream = replacement
                    .connect_tcp(target)
                    .await
                    .expect("recovered Windows generation should connect");
                let replacement_status = handle.subscribe().borrow().runtime_status(&config);
                let replacement_sources = [
                    replacement_status.selected_ipv4_address.map(IpAddr::V4),
                    replacement_status.selected_ipv6_address.map(IpAddr::V6),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                assert!(
                    !replacement_sources.is_empty(),
                    "recovered Windows generation selected no source"
                );
                let recovered_source = stream.local_addr().unwrap().ip();
                assert!(replacement_sources.contains(&recovered_source));
                time::sleep(Duration::from_secs(2)).await;
                assert_eq!(
                    handle
                        .try_lease()
                        .expect("stable recovered Windows generation")
                        .generation_id(),
                    replacement_generation
                );
                std::fs::write(
                    marker_directory.join("recovered-source.marker"),
                    recovered_source.to_string(),
                )
                .expect("publish recovered Windows source marker");
                std::fs::write(
                    marker_directory.join("recovered.marker"),
                    format!("{initial_generation}->{replacement_generation}"),
                )
                .expect("publish recovered Windows generation marker");
                println!(
                    "WINDOWS_BINDING_PROBE case={case} generation={initial_generation}->{replacement_generation} sources={replacement_sources:?}"
                );
                handle
                    .shutdown()
                    .await
                    .expect("shutdown Windows supervisor");
                supervisor_task.await.expect("join Windows supervisor");
                return;
            }
            "http-tracker-announce" => {
                let url = required_env("SUPERSEEDR_WINDOWS_HTTP_TRACKER_URL");
                let _ = crate::tracker::client::announce_started(
                    &lease,
                    url,
                    &[0x53; 20],
                    "-SS1000-QUALIFY-0001".to_string(),
                    49_152,
                    1,
                )
                .await;
            }
            "http-general" | "http-tracker" | "http-rss" | "http-web-seed" | "http-redirect"
            | "http-proxy-bypass" | "any-http" => {
                let url = required_env("SUPERSEEDR_WINDOWS_HTTP_URL");
                let client = match case.as_str() {
                    "http-tracker" => lease.tracker_http_client(),
                    "http-rss" => lease.rss_http_client(),
                    "http-web-seed" => lease.web_seed_http_client(),
                    _ => lease.general_http_client(),
                }
                .expect("obtain strict Windows HTTP client");
                let response = client
                    .get(&url)
                    .expect("build strict Windows HTTP request")
                    .send()
                    .await
                    .expect("send strict Windows HTTP request");
                assert!(
                    response.status().is_success(),
                    "strict Windows HTTP probe returned {}",
                    response.status()
                );
            }
            other => panic!("unsupported Windows probe case {other}"),
        }

        println!(
            "WINDOWS_BINDING_PROBE case={case} generation={} sources={selected_sources:?}",
            lease.generation_id()
        );
        handle
            .shutdown()
            .await
            .expect("shutdown Windows supervisor");
        supervisor_task.await.expect("join Windows supervisor");
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
        assert_eq!(status.ipv4_interface_index, None);
        assert_eq!(status.ipv6_interface_index, None);
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
        assert_ne!(lease.generation_id(), 0);
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
    fn generation_equivalence_distinguishes_configured_and_automatic_sources() {
        let selected = Ipv4Addr::new(192, 0, 2, 10);
        let pinned = ResolvedNetworkBinding {
            mode: NetworkBindingMode::Interface,
            interface_identity: Some(Arc::from("interface-test")),
            interface_display_name: Some(Arc::from("Interface Test")),
            ipv4: ResolvedAddressFamily {
                enabled: true,
                interface_index: Some(NonZeroU32::new(7).unwrap()),
                configured_source: Some(selected),
                effective_source: Some(selected),
                eligible_sources: Arc::from([selected, Ipv4Addr::new(192, 0, 2, 20)]),
                host_policy: NetworkFamilyHostPolicy::default(),
            },
            ipv6: ResolvedAddressFamily {
                enabled: false,
                interface_index: Some(NonZeroU32::new(7).unwrap()),
                configured_source: None,
                effective_source: None,
                eligible_sources: Arc::from([]),
                host_policy: NetworkFamilyHostPolicy::default(),
            },
        };
        let mut secondary_changed = pinned.clone();
        secondary_changed.ipv4.eligible_sources =
            Arc::from([selected, Ipv4Addr::new(192, 0, 2, 30)]);

        assert!(pinned.generation_equivalent(&secondary_changed));

        let mut automatic = pinned.clone();
        automatic.ipv4.configured_source = None;
        automatic.ipv4.effective_source = Some(selected);
        let mut automatic_changed = secondary_changed;
        automatic_changed.ipv4.configured_source = None;
        automatic_changed.ipv4.effective_source = Some(selected);
        assert!(!automatic.generation_equivalent(&automatic_changed));

        let mut disabled_family_changed = pinned.clone();
        disabled_family_changed.ipv6.interface_index = NonZeroU32::new(99);
        disabled_family_changed.ipv6.host_policy.weak_host_send = Some(true);
        assert!(pinned.generation_equivalent(&disabled_family_changed));

        let mut display_changed = pinned.clone();
        display_changed.interface_display_name = Some(Arc::from("Renamed Interface"));
        assert!(!pinned.generation_equivalent(&display_changed));
    }

    #[test]
    fn interface_link_local_source_preserves_its_scope_id() {
        let source = "fe80::42".parse::<Ipv6Addr>().unwrap();
        let factory = SocketFactory {
            binding: ResolvedNetworkBinding {
                mode: NetworkBindingMode::Interface,
                interface_identity: Some(Arc::from("interface-test")),
                interface_display_name: Some(Arc::from("Interface Test")),
                ipv4: ResolvedAddressFamily {
                    enabled: false,
                    interface_index: Some(NonZeroU32::new(7).unwrap()),
                    configured_source: None,
                    effective_source: None,
                    eligible_sources: Arc::from([]),
                    host_policy: NetworkFamilyHostPolicy::default(),
                },
                ipv6: ResolvedAddressFamily {
                    enabled: true,
                    interface_index: Some(NonZeroU32::new(7).unwrap()),
                    configured_source: Some(source),
                    effective_source: Some(source),
                    eligible_sources: Arc::from([source]),
                    host_policy: NetworkFamilyHostPolicy::default(),
                },
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
        let mut binding = ResolvedNetworkBinding {
            mode: NetworkBindingMode::Interface,
            interface_identity: Some(Arc::from("test-interface")),
            interface_display_name: Some(Arc::from("Test Interface")),
            ipv4: ResolvedAddressFamily {
                enabled: true,
                interface_index: NonZeroU32::new(1),
                eligible_sources: vec![Ipv4Addr::new(192, 0, 2, 20), Ipv4Addr::new(192, 0, 2, 10)]
                    .into(),
                configured_source: None,
                effective_source: None,
                host_policy: NetworkFamilyHostPolicy::default(),
            },
            ipv6: ResolvedAddressFamily {
                enabled: false,
                interface_index: NonZeroU32::new(1),
                eligible_sources: Arc::from([]),
                configured_source: None,
                effective_source: None,
                host_policy: NetworkFamilyHostPolicy::default(),
            },
        };
        assert_eq!(
            binding.http_local_address(),
            Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        );

        let explicit = Ipv4Addr::new(192, 0, 2, 10);
        binding.ipv4.configured_source = Some(explicit);
        binding.ipv4.effective_source = Some(explicit);
        assert_eq!(binding.http_local_address(), Some(IpAddr::V4(explicit)));
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
        let Some((interface, _)) = non_loopback_ipv4_interface() else {
            return;
        };
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
            .ipv4
            .eligible_sources
            .to_vec();
        stale_addresses.push(Ipv4Addr::new(192, 0, 2, 1));
        stale_generation
            .socket_factory
            .binding
            .ipv4
            .eligible_sources = Arc::from(stale_addresses);
        let stale_generation = Arc::new(stale_generation);
        let (state_tx, _) = watch::channel(NetworkState::Ready(stale_generation.clone()));
        let (_command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let mut supervisor = NetworkSupervisor {
            next_generation_id: AtomicU64::new(2),
            desired_epoch: 1,
            desired_config: config,
            last_resolved_binding: Some(stale_generation.socket_factory.binding.clone()),
            retry_blocked_binding: false,
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
        let Some((interface, _)) = non_loopback_ipv4_interface() else {
            return;
        };
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
        generation.socket_factory.binding.ipv6.eligible_sources =
            Arc::from([Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)]);
        let generation = Arc::new(generation);
        let (state_tx, _) = watch::channel(NetworkState::Ready(generation.clone()));
        let (_command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let mut supervisor = NetworkSupervisor {
            next_generation_id: AtomicU64::new(2),
            desired_epoch: 1,
            desired_config: config,
            last_resolved_binding: Some(generation.socket_factory.binding.clone()),
            retry_blocked_binding: false,
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
    fn binding_snapshot_retries_unchanged_blocked_binding_only_when_marked_transient() {
        let Some((interface, _)) = non_loopback_ipv4_interface() else {
            return;
        };
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
            retry_blocked_binding: false,
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

        supervisor.retry_blocked_binding = true;
        supervisor.refresh_binding_snapshot();

        assert!(matches!(
            &*supervisor.state_tx.borrow(),
            NetworkState::Ready(generation) if generation.id() == 2
        ));
    }

    #[test]
    fn generation_build_failure_retries_only_after_binding_resolution() {
        let resolved = ResolvedNetworkBinding::unrestricted();
        let transient = io::Error::new(io::ErrorKind::AddrNotAvailable, "temporary preflight");
        let invalid = io::Error::new(io::ErrorKind::InvalidInput, "invalid policy");
        assert!(generation_build_failure_is_retryable(
            Some(&resolved),
            &transient
        ));
        assert!(!generation_build_failure_is_retryable(None, &transient));
        assert!(!generation_build_failure_is_retryable(
            Some(&resolved),
            &invalid
        ));
    }

    #[test]
    fn initial_generation_retries_a_transient_build_failure() {
        let resolved = ResolvedNetworkBinding::unrestricted();
        let (state, last_resolved_binding, retry_blocked_binding) = initial_generation_state(
            Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "temporary source-address preflight failure",
            )),
            Some(resolved.clone()),
        );

        assert!(matches!(state, NetworkState::Blocked(_)));
        assert_eq!(last_resolved_binding, Some(resolved));
        assert!(retry_blocked_binding);
    }

    #[test]
    fn binding_monitor_retries_a_transient_listener_failure_in_any_mode() {
        let config = NetworkBindingConfig::default();
        let (state_tx, _) = watch::channel(NetworkState::Blocked(NetworkBlockedReason::new(
            "temporary listener address conflict",
        )));
        let (_command_tx, command_rx) = mpsc::channel(SUPERVISOR_COMMAND_CAPACITY);
        let mut supervisor = NetworkSupervisor {
            next_generation_id: AtomicU64::new(2),
            desired_epoch: 1,
            desired_config: config,
            last_resolved_binding: Some(ResolvedNetworkBinding::unrestricted()),
            retry_blocked_binding: true,
            state_tx,
            command_rx,
        };

        supervisor.refresh_binding_snapshot();

        assert!(!supervisor.retry_blocked_binding);
        assert!(matches!(
            &*supervisor.state_tx.borrow(),
            NetworkState::Ready(generation) if generation.id() == 2
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
            retry_blocked_binding: false,
            state_tx,
            command_rx,
        };

        supervisor.block_generation(
            1,
            NetworkBlockedReason::new("stale listener bind failure"),
            false,
        );

        assert!(!current_generation.is_invalidated());
        assert!(matches!(
            &*supervisor.state_tx.borrow(),
            NetworkState::Ready(generation) if Arc::ptr_eq(generation, &current_generation)
        ));

        supervisor.block_generation(
            current_generation.id(),
            NetworkBlockedReason::new("current listener bind failure"),
            false,
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
    async fn activation_replacement_cancels_bound_dns_resolution() {
        let dns_server = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind stalled DNS fixture");
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::LocalAddress,
            interface: None,
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(Ipv4Addr::LOCALHOST),
            ipv6_address: None,
            dns_policy: DnsPolicy::Bound,
            dns_servers: vec![dns_server.local_addr().expect("DNS fixture address")],
        };
        let (handle, supervisor_task) = NetworkSupervisor::spawn_with_config(&config);
        let (mut publisher, _activation) =
            crate::networking::activation::NetworkActivationPublisher::channel();
        let active = publisher
            .activate(handle.try_lease().expect("bound DNS generation"), 41_020)
            .expect("activate bound DNS scope");
        let generation_id = active.scope().id().generation_id();
        let lease = active.scope().lease().clone();
        let resolution = tokio::spawn(async move { lease.resolve("resolver.test", 4242).await });

        let mut query = [0_u8; 512];
        time::timeout(Duration::from_secs(1), dns_server.recv_from(&mut query))
            .await
            .expect("bound DNS query should be sent")
            .expect("receive bound DNS query");
        publisher.pending(Some(generation_id));

        let error = time::timeout(Duration::from_millis(250), resolution)
            .await
            .expect("activation replacement should cancel bound DNS promptly")
            .expect("join bound DNS resolution")
            .expect_err("invalidated activation must reject bound DNS result");
        assert!(matches!(
            error,
            NetworkLeaseError::Invalidated {
                generation_id: invalidated_generation
            } if invalidated_generation == generation_id
        ));

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
        let Some((interface_name, interface_address)) = non_loopback_ipv4_interface() else {
            return;
        };
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
            factory.binding.ipv4.interface_index
        );
    }

    #[cfg(unix)]
    #[test]
    fn interface_policy_rejects_a_loopback_device() {
        let (interface_name, interface_address) = loopback_interface();
        let config = NetworkBindingConfig {
            mode: NetworkBindingMode::Interface,
            interface: Some(interface_name),
            enable_ipv4: true,
            enable_ipv6: false,
            ipv4_address: Some(interface_address),
            ipv6_address: None,
            dns_policy: DnsPolicy::System,
            dns_servers: Vec::new(),
        };

        let error = SocketFactory::from_config(&config)
            .expect_err("loopback interface mode must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("loopback device"));
    }

    #[cfg(unix)]
    fn non_loopback_ipv4_interface() -> Option<(String, Ipv4Addr)> {
        available_network_interfaces()
            .expect("discover network interfaces")
            .into_iter()
            .filter(|interface| interface.is_up && !interface.is_loopback)
            .find_map(|interface| {
                interface
                    .ipv4_addresses
                    .first()
                    .copied()
                    .map(|address| (interface.identity, address))
            })
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
