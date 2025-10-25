// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::tracker::TrackerEvent;

use crate::errors::TrackerError;
use crate::tracker::{Peer, TrackerResponse};

use serde_bencode::from_bytes;
use std::collections::HashSet;
use std::net::Ipv4Addr;

use reqwest::Client;
use reqwest::header;

use crate::tracker::Peers;
use crate::tracker::RawTrackerResponse;

static APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"), 
    "/", 
    env!("CARGO_PKG_VERSION")
);

pub async fn announce_started(
    announce_link: String,
    hashed_info_dict: &[u8],
    client_id: String,
    client_port: u16,
    torrent_size_left: usize,
) -> Result<TrackerResponse, TrackerError> {
    make_announce_request(AnnounceParams {
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

pub async fn announce_periodic(
    announce_link: String,
    hashed_info_dict: &[u8],
    client_id: String,
    client_port: u16,
    uploaded: usize,
    downloaded: usize,
    torrent_size_left: usize,
) -> Result<TrackerResponse, TrackerError> {
    make_announce_request(AnnounceParams {
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
    announce_link: String,
    hashed_info_dict: &[u8],
    client_id: String,
    client_port: u16,
    uploaded: usize,
    downloaded: usize,
) -> Result<TrackerResponse, TrackerError> {
    make_announce_request(AnnounceParams {
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

pub async fn announce_stopped(
    announce_link: String,
    hashed_info_dict: &[u8],
    client_id: String,
    client_port: u16,
    uploaded: usize,
    downloaded: usize,
    torrent_size_left: usize,
) {
    let _ = make_announce_request(AnnounceParams {
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

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(APP_USER_AGENT)
    );

    let client = Client::builder().default_headers(headers).build().unwrap_or_else(|_| reqwest::Client::new());
    let response = client.get(link).send().await?.bytes().await?;
    let raw_response: RawTrackerResponse = from_bytes(&response)?;

    if let Some(reason) = raw_response.failure_reason {
        return Err(TrackerError::Tracker(reason));
    }

    let peers: Vec<_> = match raw_response.peers {
        Peers::Compact(bytes) => bytes
            .chunks_exact(6)
            .map(|chunk| {
                let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                Peer {
                    peer_id: Vec::new(), // Not available in compact format
                    ip: ip.to_string(),
                    port,
                }
            })
            .collect(),
        Peers::Dicts(dicts) => dicts
            .into_iter()
            .map(|d| Peer {
                peer_id: d.peer_id,
                ip: d.ip,
                port: d.port,
            })
            .collect(),
    };

    let tracker_response = TrackerResponse {
        failure_reason: None,
        warning_message: raw_response.warning_message,
        interval: raw_response.interval,
        min_interval: raw_response.min_interval,
        tracker_id: raw_response.tracker_id,
        complete: raw_response.complete,
        incomplete: raw_response.incomplete,
        peers,
    };

    Ok(tracker_response)
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
