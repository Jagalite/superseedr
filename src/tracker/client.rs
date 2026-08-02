// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::errors::TrackerError;
use crate::networking::runtime::NetworkLease;
use crate::tracker::Peers;
use crate::tracker::RawTrackerResponse;
use crate::tracker::TrackerEvent;
use crate::tracker::TrackerResponse;

use rand::RngExt;
use reqwest::header;
use reqwest::StatusCode;
use reqwest::Url;
use serde_bencode::from_bytes;
use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinSet;
use tokio::time::timeout;

const UDP_PROTOCOL_ID: u64 = 0x41727101980;
const UDP_CONNECT_ACTION: u32 = 0;
const UDP_ANNOUNCE_ACTION: u32 = 1;
const UDP_ERROR_ACTION: u32 = 3;
const TRACKER_PEER_DNS_TIMEOUT: Duration = Duration::from_secs(1);
const TRACKER_PEER_DNS_CONCURRENCY: usize = 8;
const UDP_TRACKER_DNS_TIMEOUT: Duration = Duration::from_secs(1);
const UDP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const UDP_REQUEST_RETRIES: usize = 3;

pub async fn announce_started(
    network_lease: &NetworkLease,
    announce_link: String,
    hashed_info_dict: &[u8],
    client_id: String,
    client_port: u16,
    torrent_size_left: usize,
) -> Result<TrackerResponse, TrackerError> {
    make_announce_request(AnnounceParams {
        network_lease: network_lease.clone(),
        announce_link,
        hashed_info_dict: hashed_info_dict.to_vec(),
        client_id,
        client_port,
        uploaded: 0,
        downloaded: 0,
        left: torrent_size_left,
        num_peers_want: 50,
        event: Some(TrackerEvent::Started),
    })
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn announce_periodic(
    network_lease: &NetworkLease,
    announce_link: String,
    hashed_info_dict: &[u8],
    client_id: String,
    client_port: u16,
    uploaded: usize,
    downloaded: usize,
    torrent_size_left: usize,
) -> Result<TrackerResponse, TrackerError> {
    make_announce_request(AnnounceParams {
        network_lease: network_lease.clone(),
        announce_link,
        hashed_info_dict: hashed_info_dict.to_vec(),
        client_id,
        client_port,
        uploaded,
        downloaded,
        left: torrent_size_left,
        num_peers_want: 50,
        event: None,
    })
    .await
}

pub async fn announce_completed(
    network_lease: &NetworkLease,
    announce_link: String,
    hashed_info_dict: &[u8],
    client_id: String,
    client_port: u16,
    uploaded: usize,
    downloaded: usize,
) -> Result<TrackerResponse, TrackerError> {
    make_announce_request(AnnounceParams {
        network_lease: network_lease.clone(),
        announce_link,
        hashed_info_dict: hashed_info_dict.to_vec(),
        client_id,
        client_port,
        uploaded,
        downloaded,
        left: 0,
        num_peers_want: 0,
        event: Some(TrackerEvent::Completed),
    })
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn announce_stopped(
    network_lease: &NetworkLease,
    announce_link: String,
    hashed_info_dict: &[u8],
    client_id: String,
    client_port: u16,
    uploaded: usize,
    downloaded: usize,
    torrent_size_left: usize,
) {
    let _ = make_announce_request(AnnounceParams {
        network_lease: network_lease.clone(),
        announce_link,
        hashed_info_dict: hashed_info_dict.to_vec(),
        client_id,
        client_port,
        uploaded,
        downloaded,
        left: torrent_size_left,
        num_peers_want: 0,
        event: Some(TrackerEvent::Stopped),
    })
    .await;
}

struct AnnounceParams {
    network_lease: NetworkLease,
    announce_link: String,
    hashed_info_dict: Vec<u8>,
    client_id: String,
    client_port: u16,
    uploaded: usize,
    downloaded: usize,
    left: usize,
    num_peers_want: usize,
    event: Option<TrackerEvent>,
}

async fn make_announce_request(params: AnnounceParams) -> Result<TrackerResponse, TrackerError> {
    match tracker_scheme(&params.announce_link)? {
        TrackerScheme::Http => make_http_announce_request(&params).await,
        TrackerScheme::Udp => make_udp_announce_request(&params).await,
    }
}

async fn make_http_announce_request(
    params: &AnnounceParams,
) -> Result<TrackerResponse, TrackerError> {
    let mut link = format!(
        "{}?info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&numwant={}&compact=1",
        params.announce_link,
        encode_url_nn(&params.hashed_info_dict),
        encode_url_nn(params.client_id.as_bytes()),
        params.client_port,
        params.uploaded,
        params.downloaded,
        params.left,
        params.num_peers_want,
    );

    if let Some(event_val) = params.event {
        link.push_str(&format!("&event={}", event_val));
    }

    let client = params
        .network_lease
        .tracker_http_client()
        .map_err(|error| TrackerError::Protocol(error.to_string()))?;
    let response = params
        .network_lease
        .cancel_on_invalidation(client.get(link).send())
        .await
        .map_err(|error| TrackerError::Protocol(error.to_string()))??;
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if !status.is_success() {
        return Err(TrackerError::Protocol(format!(
            "HTTP tracker returned status {}{}",
            status,
            format_content_type_suffix(content_type.as_deref())
        )));
    }
    let response = params
        .network_lease
        .cancel_on_invalidation(response.bytes())
        .await
        .map_err(|error| TrackerError::Protocol(error.to_string()))??;
    parse_http_tracker_response(&response, &params.network_lease)
        .await
        .map_err(|error| {
            classify_http_tracker_error(error, &response, status, content_type.as_deref())
        })
}

async fn parse_http_tracker_response(
    response: &[u8],
    network_lease: &NetworkLease,
) -> Result<TrackerResponse, TrackerError> {
    let raw_response: RawTrackerResponse = from_bytes(response)?;

    if let Some(reason) = raw_response.failure_reason {
        return Err(TrackerError::Tracker(reason));
    }

    let mut peers = Vec::new();

    if let Some(peer_list) = raw_response.peers {
        match peer_list {
            Peers::Compact(bytes) => {
                peers.extend(parse_compact_ipv4_peers(&bytes)?);
            }
            Peers::Dicts(dicts) => {
                peers.extend(resolve_tracker_peer_dicts(dicts, network_lease).await);
            }
        }
    }

    if let Some(v6_bytes) = raw_response.peers6 {
        peers.extend(parse_compact_ipv6_peers(&v6_bytes)?);
    }

    Ok(TrackerResponse {
        failure_reason: None,
        warning_message: raw_response.warning_message,
        interval: raw_response.interval,
        min_interval: raw_response.min_interval,
        tracker_id: raw_response.tracker_id,
        complete: raw_response.complete,
        incomplete: raw_response.incomplete,
        peers,
    })
}

async fn resolve_tracker_peer_dicts(
    dicts: Vec<crate::tracker::PeerDictModel>,
    network_lease: &NetworkLease,
) -> Vec<SocketAddr> {
    let mut peers = Vec::new();
    let mut hostname_peers = Vec::new();

    for peer in dicts {
        if let Ok(ip) = peer.ip.parse::<IpAddr>() {
            peers.push(SocketAddr::new(ip, peer.port));
            continue;
        }

        hostname_peers.push((peer.ip, peer.port));
    }

    let mut hostname_peers = hostname_peers.into_iter();
    let mut hostname_resolutions = JoinSet::new();

    loop {
        while hostname_resolutions.len() < TRACKER_PEER_DNS_CONCURRENCY {
            let Some((hostname, port)) = hostname_peers.next() else {
                break;
            };
            let network_lease = network_lease.clone();
            hostname_resolutions.spawn(async move {
                let hostname_for_lookup = hostname.clone();
                resolve_tracker_peer_hostname_with_lookup(
                    hostname.as_str(),
                    port,
                    TRACKER_PEER_DNS_TIMEOUT,
                    async move {
                        network_lease
                            .resolve(&hostname_for_lookup, port)
                            .await
                            .map_err(io::Error::other)
                    },
                )
                .await
            });
        }

        let Some(resolved) = hostname_resolutions.join_next().await else {
            break;
        };

        if let Ok(resolved) = resolved {
            peers.extend(resolved);
        }
    }

    peers
}

async fn resolve_tracker_peer_hostname_with_lookup<F>(
    hostname: &str,
    port: u16,
    lookup_timeout: Duration,
    lookup: F,
) -> Vec<SocketAddr>
where
    F: Future<Output = io::Result<Vec<SocketAddr>>>,
{
    match timeout(lookup_timeout, lookup).await {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(error)) => {
            tracing::debug!(
                host = hostname,
                port,
                error = %error,
                "Skipping tracker peer hostname after failed DNS lookup."
            );
            Vec::new()
        }
        Err(_) => {
            tracing::debug!(
                host = hostname,
                port,
                timeout_ms = lookup_timeout.as_millis(),
                "Skipping tracker peer hostname after DNS lookup timeout."
            );
            Vec::new()
        }
    }
}

fn classify_http_tracker_error(
    error: TrackerError,
    response: &[u8],
    status: StatusCode,
    content_type: Option<&str>,
) -> TrackerError {
    match error {
        TrackerError::Bencode(_) => {
            let preview = response_preview(response);
            let preview_suffix = preview
                .as_deref()
                .map(|value| format!("; body starts with {:?}", value))
                .unwrap_or_default();
            let html_hint = content_type
                .filter(|value| value.starts_with("text/html"))
                .map(|_| " (received HTML, likely not a tracker response)")
                .unwrap_or("");
            TrackerError::Protocol(format!(
                "HTTP tracker returned non-bencoded response (status {}{}{}{})",
                status,
                format_content_type_suffix(content_type),
                html_hint,
                preview_suffix
            ))
        }
        other => other,
    }
}

fn format_content_type_suffix(content_type: Option<&str>) -> String {
    content_type
        .map(|value| format!(", content-type {}", value))
        .unwrap_or_default()
}

fn response_preview(response: &[u8]) -> Option<String> {
    let preview = String::from_utf8_lossy(&response[..response.len().min(80)]);
    let preview = preview
        .chars()
        .map(|ch| {
            if ch.is_control() && !ch.is_whitespace() {
                '.'
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    (!preview.is_empty()).then_some(preview)
}

async fn make_udp_announce_request(
    params: &AnnounceParams,
) -> Result<TrackerResponse, TrackerError> {
    let url = Url::parse(&params.announce_link)
        .map_err(|error| TrackerError::InvalidUrl(error.to_string()))?;
    let resolved_addrs = resolve_udp_tracker_addrs(&url, &params.network_lease).await?;

    params
        .network_lease
        .cancel_on_invalidation(retry_udp_announce_across_addrs(
            &resolved_addrs,
            |tracker_addr| try_udp_announce_once_to_addr(params, tracker_addr),
        ))
        .await
        .map_err(|error| TrackerError::Protocol(error.to_string()))?
}

async fn resolve_udp_tracker_addrs(
    url: &Url,
    network_lease: &NetworkLease,
) -> Result<Vec<SocketAddr>, TrackerError> {
    let host = url
        .host_str()
        .ok_or_else(|| TrackerError::InvalidUrl("tracker URL is missing a host".to_string()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| TrackerError::InvalidUrl("tracker URL is missing a port".to_string()))?;

    resolve_udp_tracker_addrs_with_lookup(host, port, UDP_TRACKER_DNS_TIMEOUT, async {
        network_lease
            .resolve(host, port)
            .await
            .map_err(io::Error::other)
    })
    .await
}

async fn resolve_udp_tracker_addrs_with_lookup<F>(
    host: &str,
    port: u16,
    lookup_timeout: Duration,
    lookup: F,
) -> Result<Vec<SocketAddr>, TrackerError>
where
    F: Future<Output = io::Result<Vec<SocketAddr>>>,
{
    match timeout(lookup_timeout, lookup).await {
        Ok(Ok(resolved_addrs)) if resolved_addrs.is_empty() => Err(TrackerError::Protocol(
            "tracker host resolved to no socket addresses".to_string(),
        )),
        Ok(Ok(resolved_addrs)) => Ok(resolved_addrs),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err(TrackerError::Protocol(format!(
            "UDP tracker host DNS lookup timed out for {}:{}",
            host, port
        ))),
    }
}

async fn retry_udp_announce_across_addrs<F, Fut>(
    tracker_addrs: &[SocketAddr],
    mut attempt: F,
) -> Result<TrackerResponse, TrackerError>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: Future<Output = Result<TrackerResponse, TrackerError>>,
{
    let mut last_error = None;
    for _ in 0..UDP_REQUEST_RETRIES {
        for &tracker_addr in tracker_addrs {
            match attempt(tracker_addr).await {
                Ok(response) => return Ok(response),
                Err(error) => last_error = Some(error),
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        TrackerError::Protocol("UDP tracker announce failed without an error".to_string())
    }))
}

async fn try_udp_announce_once_to_addr(
    params: &AnnounceParams,
    tracker_addr: SocketAddr,
) -> Result<TrackerResponse, TrackerError> {
    let bind_addr = match tracker_addr {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    };
    let socket = params.network_lease.bind_udp(bind_addr).await?;
    socket.connect(tracker_addr).await?;
    params
        .network_lease
        .ensure_valid()
        .map_err(|error| TrackerError::Protocol(error.to_string()))?;
    try_udp_announce_once(&socket, params, tracker_addr).await
}

async fn try_udp_announce_once(
    socket: &UdpSocket,
    params: &AnnounceParams,
    tracker_addr: SocketAddr,
) -> Result<TrackerResponse, TrackerError> {
    let connection_id = match timeout(UDP_REQUEST_TIMEOUT, send_udp_connect_request(socket)).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(TrackerError::Protocol(
                "UDP tracker connect request timed out".to_string(),
            ));
        }
    };

    match timeout(
        UDP_REQUEST_TIMEOUT,
        send_udp_announce_request(socket, connection_id, params, tracker_addr),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(TrackerError::Protocol(
            "UDP tracker announce request timed out".to_string(),
        )),
    }
}

async fn send_udp_connect_request(socket: &UdpSocket) -> Result<u64, TrackerError> {
    let transaction_id = rand::rng().random::<u32>();
    let mut request = [0u8; 16];
    request[..8].copy_from_slice(&UDP_PROTOCOL_ID.to_be_bytes());
    request[8..12].copy_from_slice(&UDP_CONNECT_ACTION.to_be_bytes());
    request[12..16].copy_from_slice(&transaction_id.to_be_bytes());

    socket.send(&request).await?;

    let mut response = [0u8; 2048];
    let len = socket.recv(&mut response).await?;
    parse_udp_connect_response(&response[..len], transaction_id)
}

fn parse_udp_connect_response(response: &[u8], transaction_id: u32) -> Result<u64, TrackerError> {
    if response.len() < 16 {
        return Err(TrackerError::Protocol(
            "UDP tracker connect response was too short".to_string(),
        ));
    }

    let action = u32::from_be_bytes(response[0..4].try_into().unwrap());
    let returned_transaction_id = u32::from_be_bytes(response[4..8].try_into().unwrap());
    if returned_transaction_id != transaction_id {
        return Err(TrackerError::Protocol(
            "UDP tracker connect transaction ID mismatch".to_string(),
        ));
    }

    if action == UDP_ERROR_ACTION {
        return Err(TrackerError::Tracker(
            String::from_utf8_lossy(&response[8..]).into_owned(),
        ));
    }

    if action != UDP_CONNECT_ACTION {
        return Err(TrackerError::Protocol(format!(
            "unexpected UDP tracker connect action {}",
            action
        )));
    }

    Ok(u64::from_be_bytes(response[8..16].try_into().unwrap()))
}

async fn send_udp_announce_request(
    socket: &UdpSocket,
    connection_id: u64,
    params: &AnnounceParams,
    tracker_addr: SocketAddr,
) -> Result<TrackerResponse, TrackerError> {
    let transaction_id = rand::rng().random::<u32>();
    let mut request = [0u8; 98];
    request[..8].copy_from_slice(&connection_id.to_be_bytes());
    request[8..12].copy_from_slice(&UDP_ANNOUNCE_ACTION.to_be_bytes());
    request[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    request[16..36].copy_from_slice(&fixed_width_bytes(&params.hashed_info_dict, 20));
    request[36..56].copy_from_slice(&fixed_width_bytes(params.client_id.as_bytes(), 20));
    request[56..64].copy_from_slice(&(params.downloaded as u64).to_be_bytes());
    request[64..72].copy_from_slice(&(params.left as u64).to_be_bytes());
    request[72..80].copy_from_slice(&(params.uploaded as u64).to_be_bytes());
    request[80..84].copy_from_slice(&udp_event_code(params.event).to_be_bytes());
    request[84..88].copy_from_slice(&0u32.to_be_bytes());
    request[88..92].copy_from_slice(&rand::rng().random::<u32>().to_be_bytes());
    request[92..96].copy_from_slice(&(params.num_peers_want as i32).to_be_bytes());
    request[96..98].copy_from_slice(&params.client_port.to_be_bytes());

    socket.send(&request).await?;

    let mut response = [0u8; 4096];
    let len = socket.recv(&mut response).await?;
    parse_udp_announce_response(&response[..len], transaction_id, tracker_addr)
}

fn parse_udp_announce_response(
    response: &[u8],
    transaction_id: u32,
    tracker_addr: SocketAddr,
) -> Result<TrackerResponse, TrackerError> {
    if response.len() < 20 {
        return Err(TrackerError::Protocol(
            "UDP tracker announce response was too short".to_string(),
        ));
    }

    let action = u32::from_be_bytes(response[0..4].try_into().unwrap());
    let returned_transaction_id = u32::from_be_bytes(response[4..8].try_into().unwrap());
    if returned_transaction_id != transaction_id {
        return Err(TrackerError::Protocol(
            "UDP tracker announce transaction ID mismatch".to_string(),
        ));
    }

    if action == UDP_ERROR_ACTION {
        return Err(TrackerError::Tracker(
            String::from_utf8_lossy(&response[8..]).into_owned(),
        ));
    }

    if action != UDP_ANNOUNCE_ACTION {
        return Err(TrackerError::Protocol(format!(
            "unexpected UDP tracker announce action {}",
            action
        )));
    }

    let interval = u32::from_be_bytes(response[8..12].try_into().unwrap()) as i64;
    let incomplete = u32::from_be_bytes(response[12..16].try_into().unwrap()) as i64;
    let complete = u32::from_be_bytes(response[16..20].try_into().unwrap()) as i64;
    let peer_bytes = &response[20..];

    let peers = if tracker_addr.is_ipv4() {
        parse_compact_ipv4_peers(peer_bytes)?
    } else {
        parse_compact_ipv6_peers(peer_bytes)?
    };

    Ok(TrackerResponse {
        failure_reason: None,
        warning_message: None,
        interval,
        min_interval: None,
        tracker_id: None,
        complete,
        incomplete,
        peers,
    })
}

fn parse_compact_ipv4_peers(bytes: &[u8]) -> Result<Vec<SocketAddr>, TrackerError> {
    let chunks = bytes.chunks_exact(6);
    if !chunks.remainder().is_empty() {
        return Err(TrackerError::Protocol(
            "compact IPv4 peer list had trailing bytes".to_string(),
        ));
    }

    Ok(chunks
        .map(|chunk| {
            let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            SocketAddr::new(IpAddr::V4(ip), port)
        })
        .collect())
}

fn parse_compact_ipv6_peers(bytes: &[u8]) -> Result<Vec<SocketAddr>, TrackerError> {
    let chunks = bytes.chunks_exact(18);
    if !chunks.remainder().is_empty() {
        return Err(TrackerError::Protocol(
            "compact IPv6 peer list had trailing bytes".to_string(),
        ));
    }

    Ok(chunks
        .map(|chunk| {
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&chunk[..16]);
            let ip = Ipv6Addr::from(addr);
            let port = u16::from_be_bytes([chunk[16], chunk[17]]);
            SocketAddr::new(IpAddr::V6(ip), port)
        })
        .collect())
}

fn fixed_width_bytes(bytes: &[u8], len: usize) -> Vec<u8> {
    let mut fixed = vec![0u8; len];
    let copy_len = len.min(bytes.len());
    fixed[..copy_len].copy_from_slice(&bytes[..copy_len]);
    fixed
}

fn udp_event_code(event: Option<TrackerEvent>) -> u32 {
    match event {
        None => 0,
        Some(TrackerEvent::Completed) => 1,
        Some(TrackerEvent::Started) => 2,
        Some(TrackerEvent::Stopped) => 3,
    }
}

fn tracker_scheme(url: &str) -> Result<TrackerScheme, TrackerError> {
    let parsed = Url::parse(url).map_err(|error| TrackerError::InvalidUrl(error.to_string()))?;
    match parsed.scheme() {
        "http" | "https" => Ok(TrackerScheme::Http),
        "udp" => Ok(TrackerScheme::Udp),
        scheme => Err(TrackerError::Protocol(format!(
            "unsupported tracker scheme {}",
            scheme
        ))),
    }
}

enum TrackerScheme {
    Http,
    Udp,
}

fn encode_url_nn(param: &[u8]) -> String {
    let allowed_chars: HashSet<u8> =
        "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-_~"
            .bytes()
            .collect();

    param
        .iter()
        .map(|&byte| {
            if allowed_chars.contains(&byte) {
                return String::from(byte as char);
            }
            format!("%{:02X}", &byte)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::announce_completed;
    use super::announce_started;
    use super::classify_http_tracker_error;
    use super::format_content_type_suffix;
    use super::parse_compact_ipv4_peers;
    use super::parse_compact_ipv6_peers;
    use super::parse_http_tracker_response;
    use super::resolve_tracker_peer_hostname_with_lookup;
    use super::resolve_udp_tracker_addrs_with_lookup;
    use super::retry_udp_announce_across_addrs;
    use crate::errors::TrackerError;
    use crate::networking::runtime::{NetworkHandle, NetworkLease, NetworkSupervisor};
    use crate::tracker::TrackerResponse;
    use reqwest::StatusCode;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, UdpSocket};
    use tokio::sync::oneshot;
    use tokio::time::{sleep, timeout, Duration};

    fn unrestricted_network_lease() -> (NetworkHandle, NetworkLease) {
        let (handle, _task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let lease = handle.try_lease().unwrap();
        (handle, lease)
    }

    #[tokio::test]
    async fn parse_http_tracker_response_supports_ipv6_compact_peers() {
        let mut encoded = b"d8:intervali120e6:peers618:".to_vec();
        encoded.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        encoded.extend_from_slice(&51413u16.to_be_bytes());
        encoded.push(b'e');

        let (_network_handle, network_lease) = unrestricted_network_lease();
        let response = parse_http_tracker_response(&encoded, &network_lease)
            .await
            .expect("parse tracker response");

        assert_eq!(
            response.peers,
            vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 51413)]
        );
    }

    #[tokio::test]
    async fn parse_http_tracker_response_resolves_hostname_dict_peers() {
        let encoded = b"d8:intervali120e5:peersld2:ip9:localhost4:porti51413eeee".to_vec();

        let (_network_handle, network_lease) = unrestricted_network_lease();
        let response = parse_http_tracker_response(&encoded, &network_lease)
            .await
            .expect("parse tracker response");

        assert!(
            response
                .peers
                .iter()
                .any(|peer| peer.port() == 51413 && peer.ip().is_loopback()),
            "expected localhost dict peer to resolve to a loopback address, got {:?}",
            response.peers
        );
    }

    #[tokio::test]
    async fn resolve_tracker_peer_hostname_timeout_returns_empty() {
        let resolved = resolve_tracker_peer_hostname_with_lookup(
            "slow.test",
            51413,
            Duration::from_millis(1),
            async {
                sleep(Duration::from_millis(25)).await;
                Ok(vec![SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    51413,
                )])
            },
        )
        .await;

        assert!(resolved.is_empty());
    }

    #[tokio::test]
    async fn resolve_udp_tracker_addrs_timeout_returns_protocol_error() {
        let error = resolve_udp_tracker_addrs_with_lookup(
            "tracker.local",
            6969,
            Duration::from_millis(1),
            async {
                sleep(Duration::from_millis(25)).await;
                Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6969)])
            },
        )
        .await
        .expect_err("timeout should fail");

        assert!(matches!(
            error,
            TrackerError::Protocol(message) if message.contains("DNS lookup timed out")
        ));
    }

    #[tokio::test]
    async fn retry_udp_announce_across_addrs_tries_next_address_before_retrying_first() {
        let first = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10001);
        let second = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10002);
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let expected = TrackerResponse {
            failure_reason: None,
            warning_message: None,
            interval: 30,
            min_interval: None,
            tracker_id: None,
            complete: 0,
            incomplete: 0,
            peers: Vec::new(),
        };

        let response = retry_udp_announce_across_addrs(&[first, second], {
            let attempts = Arc::clone(&attempts);
            let expected = expected.clone();
            move |tracker_addr| {
                let attempts = Arc::clone(&attempts);
                let expected = expected.clone();
                async move {
                    attempts.lock().expect("attempt lock").push(tracker_addr);
                    if tracker_addr == second {
                        Ok(expected)
                    } else {
                        Err(TrackerError::Protocol("first address failed".to_string()))
                    }
                }
            }
        })
        .await
        .expect("second address should succeed on first round");

        assert_eq!(*attempts.lock().expect("attempt lock"), vec![first, second]);
        assert_eq!(response, expected);
    }

    #[test]
    fn parse_compact_ipv4_peers_rejects_trailing_bytes() {
        let error = parse_compact_ipv4_peers(&[127, 0, 0, 1, 0x1A, 0xE1, 0xFF])
            .expect_err("trailing bytes should fail");
        assert!(matches!(error, TrackerError::Protocol(_)));
    }

    #[test]
    fn parse_compact_ipv6_peers_rejects_trailing_bytes() {
        let mut payload = Vec::from(Ipv6Addr::LOCALHOST.octets());
        payload.extend_from_slice(&51413u16.to_be_bytes());
        payload.push(0xFF);

        let error = parse_compact_ipv6_peers(&payload).expect_err("trailing bytes should fail");
        assert!(matches!(error, TrackerError::Protocol(_)));
    }

    #[test]
    fn classify_http_tracker_error_surfaces_html_response_context() {
        let error = classify_http_tracker_error(
            TrackerError::Bencode(serde_bencode::Error::InvalidValue("invalid".to_string())),
            b"<html><body>challenge</body></html>",
            StatusCode::OK,
            Some("text/html; charset=utf-8"),
        );

        let message = error.to_string();
        assert!(message.contains("non-bencoded response"));
        assert!(message.contains("received HTML"));
        assert!(message.contains("content-type text/html; charset=utf-8"));
    }

    #[test]
    fn format_content_type_suffix_omits_missing_header() {
        assert_eq!(format_content_type_suffix(None), "");
    }

    #[tokio::test]
    async fn announce_started_supports_udp_trackers() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind fake tracker");
        let tracker_addr = socket.local_addr().expect("fake tracker addr");

        let server = tokio::spawn(async move {
            let mut buf = [0u8; 2048];

            let (len, peer) = socket.recv_from(&mut buf).await.expect("recv connect");
            assert_eq!(len, 16);
            let connect_transaction_id = u32::from_be_bytes(buf[12..16].try_into().unwrap());

            let mut connect_response = [0u8; 16];
            connect_response[..4].copy_from_slice(&0u32.to_be_bytes());
            connect_response[4..8].copy_from_slice(&connect_transaction_id.to_be_bytes());
            connect_response[8..16].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
            socket
                .send_to(&connect_response, peer)
                .await
                .expect("send connect response");

            let (len, peer) = socket.recv_from(&mut buf).await.expect("recv announce");
            assert_eq!(len, 98);
            let announce_transaction_id = u32::from_be_bytes(buf[12..16].try_into().unwrap());

            let mut announce_response = Vec::with_capacity(26);
            announce_response.extend_from_slice(&1u32.to_be_bytes());
            announce_response.extend_from_slice(&announce_transaction_id.to_be_bytes());
            announce_response.extend_from_slice(&30u32.to_be_bytes());
            announce_response.extend_from_slice(&4u32.to_be_bytes());
            announce_response.extend_from_slice(&9u32.to_be_bytes());
            announce_response.extend_from_slice(&[127, 0, 0, 1]);
            announce_response.extend_from_slice(&6881u16.to_be_bytes());
            socket
                .send_to(&announce_response, peer)
                .await
                .expect("send announce response");
        });

        let (_network_handle, network_lease) = unrestricted_network_lease();
        let response = announce_started(
            &network_lease,
            format!("udp://{}/announce", tracker_addr),
            &[0x11; 20],
            "-SS0001-123456789012".to_string(),
            51413,
            4096,
        )
        .await
        .expect("udp announce should succeed");

        server.await.expect("fake tracker task");

        assert_eq!(response.interval, 30);
        assert_eq!(response.incomplete, 4);
        assert_eq!(response.complete, 9);
        assert_eq!(
            response.peers,
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6881)]
        );
    }

    #[tokio::test]
    async fn http_tracker_request_is_canceled_when_its_generation_is_invalidated() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind slow HTTP tracker");
        let tracker_addr = listener.local_addr().expect("HTTP tracker address");
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept HTTP tracker request");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await.expect("read HTTP request");
            let _ = request_seen_tx.send(());
            std::future::pending::<()>().await;
        });

        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let network_lease = network_handle.try_lease().unwrap();
        let announce_lease = network_lease.clone();
        let announce = tokio::spawn(async move {
            announce_started(
                &announce_lease,
                format!("http://{tracker_addr}/announce"),
                &[0x22; 20],
                "-SS0001-123456789012".to_string(),
                51413,
                4096,
            )
            .await
        });

        request_seen_rx.await.expect("slow tracker saw request");
        network_handle
            .block("test HTTP cancellation")
            .await
            .unwrap();
        let error = timeout(Duration::from_millis(500), announce)
            .await
            .expect("HTTP announce should cancel promptly")
            .expect("HTTP announce task")
            .expect_err("invalidated HTTP announce must fail");
        assert!(error.to_string().contains("invalidated"));

        server.abort();
        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn udp_tracker_exchange_sends_nothing_after_generation_invalidation() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind paused UDP tracker");
        let tracker_addr = socket.local_addr().expect("UDP tracker address");
        let (connect_seen_tx, connect_seen_rx) = oneshot::channel();
        let (release_response_tx, release_response_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (len, peer) = socket.recv_from(&mut buf).await.expect("receive connect");
            assert_eq!(len, 16);
            let transaction_id = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let _ = connect_seen_tx.send(());
            let _ = release_response_rx.await;

            let mut response = [0u8; 16];
            response[..4].copy_from_slice(&0u32.to_be_bytes());
            response[4..8].copy_from_slice(&transaction_id.to_be_bytes());
            response[8..16].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
            let _ = socket.send_to(&response, peer).await;

            timeout(Duration::from_millis(300), socket.recv_from(&mut buf))
                .await
                .is_ok()
        });

        let (network_handle, supervisor_task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let network_lease = network_handle.try_lease().unwrap();
        let announce_lease = network_lease.clone();
        let announce = tokio::spawn(async move {
            announce_started(
                &announce_lease,
                format!("udp://{tracker_addr}/announce"),
                &[0x33; 20],
                "-SS0001-123456789012".to_string(),
                51413,
                4096,
            )
            .await
        });

        connect_seen_rx.await.expect("UDP tracker saw connect");
        network_handle.block("test UDP cancellation").await.unwrap();
        let _ = release_response_tx.send(());
        let error = timeout(Duration::from_millis(500), announce)
            .await
            .expect("UDP announce should cancel promptly")
            .expect("UDP announce task")
            .expect_err("invalidated UDP announce must fail");
        assert!(error.to_string().contains("invalidated"));
        assert!(!server.await.expect("paused UDP tracker task"));

        network_handle.shutdown().await.unwrap();
        supervisor_task.await.unwrap();
    }

    #[tokio::test]
    async fn announce_completed_sends_udp_completed_event_and_zero_numwant() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind fake tracker");
        let tracker_addr = socket.local_addr().expect("fake tracker addr");

        let server = tokio::spawn(async move {
            let mut buf = [0u8; 2048];

            let (_, peer) = socket.recv_from(&mut buf).await.expect("recv connect");
            let connect_transaction_id = u32::from_be_bytes(buf[12..16].try_into().unwrap());

            let mut connect_response = [0u8; 16];
            connect_response[..4].copy_from_slice(&0u32.to_be_bytes());
            connect_response[4..8].copy_from_slice(&connect_transaction_id.to_be_bytes());
            connect_response[8..16].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
            socket
                .send_to(&connect_response, peer)
                .await
                .expect("send connect response");

            let (_, peer) = socket.recv_from(&mut buf).await.expect("recv announce");
            let event_code = u32::from_be_bytes(buf[80..84].try_into().unwrap());
            let numwant = i32::from_be_bytes(buf[92..96].try_into().unwrap());
            assert_eq!(event_code, 1);
            assert_eq!(numwant, 0);

            let mut announce_response = Vec::with_capacity(20);
            announce_response.extend_from_slice(&1u32.to_be_bytes());
            announce_response.extend_from_slice(
                &u32::from_be_bytes(buf[12..16].try_into().unwrap()).to_be_bytes(),
            );
            announce_response.extend_from_slice(&30u32.to_be_bytes());
            announce_response.extend_from_slice(&0u32.to_be_bytes());
            announce_response.extend_from_slice(&1u32.to_be_bytes());
            socket
                .send_to(&announce_response, peer)
                .await
                .expect("send announce response");
        });

        let (_network_handle, network_lease) = unrestricted_network_lease();
        let response = announce_completed(
            &network_lease,
            format!("udp://{}/announce", tracker_addr),
            &[0x11; 20],
            "-SS0001-123456789012".to_string(),
            51413,
            2048,
            4096,
        )
        .await
        .expect("udp completed announce should succeed");

        server.await.expect("fake tracker task");

        assert_eq!(response.complete, 1);
        assert!(response.peers.is_empty());
    }

    #[tokio::test]
    async fn announce_started_retries_udp_with_fresh_socket_after_timeout() {
        let socket = Arc::new(
            UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind fake tracker"),
        );
        let tracker_addr = socket.local_addr().expect("fake tracker addr");

        let server_socket = Arc::clone(&socket);
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let mut delayed_peer = None;
            let mut delayed_connect_task = None;

            loop {
                let (len, peer) = server_socket
                    .recv_from(&mut buf)
                    .await
                    .expect("recv packet");

                if len == 16 {
                    let connect_transaction_id =
                        u32::from_be_bytes(buf[12..16].try_into().unwrap());
                    let mut connect_response = [0u8; 16];
                    connect_response[..4].copy_from_slice(&0u32.to_be_bytes());
                    connect_response[4..8].copy_from_slice(&connect_transaction_id.to_be_bytes());
                    connect_response[8..16]
                        .copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());

                    if delayed_peer.is_none() {
                        delayed_peer = Some(peer);
                        let delayed_socket = Arc::clone(&server_socket);
                        delayed_connect_task = Some(tokio::spawn(async move {
                            sleep(Duration::from_secs(6)).await;
                            delayed_socket
                                .send_to(&connect_response, peer)
                                .await
                                .expect("send delayed connect response");
                        }));
                    } else {
                        server_socket
                            .send_to(&connect_response, peer)
                            .await
                            .expect("send connect response");
                    }
                    continue;
                }

                assert_eq!(len, 98, "expected UDP announce packet");
                let announce_transaction_id = u32::from_be_bytes(buf[12..16].try_into().unwrap());
                let mut announce_response = Vec::with_capacity(26);
                announce_response.extend_from_slice(&1u32.to_be_bytes());
                announce_response.extend_from_slice(&announce_transaction_id.to_be_bytes());
                announce_response.extend_from_slice(&30u32.to_be_bytes());
                announce_response.extend_from_slice(&4u32.to_be_bytes());
                announce_response.extend_from_slice(&9u32.to_be_bytes());
                announce_response.extend_from_slice(&[127, 0, 0, 1]);
                announce_response.extend_from_slice(&6881u16.to_be_bytes());
                server_socket
                    .send_to(&announce_response, peer)
                    .await
                    .expect("send announce response");
                break;
            }

            if let Some(task) = delayed_connect_task {
                task.await.expect("delayed connect task");
            }
        });

        let (_network_handle, network_lease) = unrestricted_network_lease();
        let response = announce_started(
            &network_lease,
            format!("udp://{}/announce", tracker_addr),
            &[0x11; 20],
            "-SS0001-123456789012".to_string(),
            51413,
            4096,
        )
        .await
        .expect("udp announce should recover after a timeout");

        server.await.expect("fake tracker task");

        assert_eq!(response.interval, 30);
        assert_eq!(
            response.peers,
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6881)]
        );
    }
}
