// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashSet, VecDeque};
use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sha1::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::timeout;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message, WebSocketStream};

use super::rtc::WebRtcSessionConfig;
use super::tracker_worker::{
    webtorrent_tracker_worker, WebTorrentAnnounceStats, WebTorrentTrackerConfig,
    WebTorrentTrackerEvent,
};
use crate::networking::session::{ConnectionType, PeerSession, PeerSessionParameters};
use crate::token_bucket::TokenBucket;
use crate::torrent_manager::command::TorrentCommand;

const LOCAL_RELAY_SIGNALING_TIMEOUT: Duration = Duration::from_secs(30);

type LocalTrackerSocket = WebSocketStream<TcpStream>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelayOfferer {
    First,
    Second,
}

async fn receive_tracker_text(
    socket: &mut LocalTrackerSocket,
) -> io::Result<(String, serde_json::Value)> {
    loop {
        let message = socket
            .next()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "tracker socket closed"))?
            .map_err(io::Error::other)?;
        match message {
            Message::Text(text) => {
                let text = text.to_string();
                let value = serde_json::from_str(&text).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?;
                return Ok((text, value));
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .map_err(io::Error::other)?,
            Message::Pong(_) => {}
            Message::Close(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "tracker socket closed",
                ));
            }
            Message::Binary(_) | Message::Frame(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker sent a non-text signaling message",
                ));
            }
        }
    }
}

fn announce_has_offer(announce: &serde_json::Value) -> bool {
    announce["offers"]
        .as_array()
        .is_some_and(|offers| !offers.is_empty())
}

async fn receive_offer_announce(
    first: &mut LocalTrackerSocket,
    second: &mut LocalTrackerSocket,
) -> io::Result<(RelayOfferer, serde_json::Value)> {
    timeout(LOCAL_RELAY_SIGNALING_TIMEOUT, async {
        // Consume one announce from each side before waiting for a refill. Otherwise the later
        // Native offer can win the select while the browser's initial offerless announce remains
        // queued and is then mistaken for the browser's answer.
        let ((_, first_announce), (_, second_announce)) =
            tokio::try_join!(receive_tracker_text(first), receive_tracker_text(second),)?;
        if announce_has_offer(&first_announce) {
            return Ok((RelayOfferer::First, first_announce));
        }
        if announce_has_offer(&second_announce) {
            return Ok((RelayOfferer::Second, second_announce));
        }

        loop {
            tokio::select! {
                message = receive_tracker_text(first) => {
                    let (_, announce) = message?;
                    if announce_has_offer(&announce) {
                        return Ok((RelayOfferer::First, announce));
                    }
                }
                message = receive_tracker_text(second) => {
                    let (_, announce) = message?;
                    if announce_has_offer(&announce) {
                        return Ok((RelayOfferer::Second, announce));
                    }
                }
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "offer announce timed out"))?
}

pub(crate) async fn relay_browser_negotiation(
    first: &mut LocalTrackerSocket,
    second: &mut LocalTrackerSocket,
) -> io::Result<()> {
    let (offerer, offerer_announce) = receive_offer_announce(first, second).await?;
    let (offerer, browser) = match offerer {
        RelayOfferer::First => (first, second),
        RelayOfferer::Second => (second, first),
    };
    let offer = offerer_announce["offers"]
        .as_array()
        .and_then(|offers| offers.first())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "announce has no offer"))?;
    browser
        .send(Message::text(
            serde_json::json!({
                "info_hash": offerer_announce["info_hash"],
                "peer_id": offerer_announce["peer_id"],
                "offer_id": offer["offer_id"],
                "offer": offer["offer"],
            })
            .to_string(),
        ))
        .await
        .map_err(io::Error::other)?;
    let answer = timeout(LOCAL_RELAY_SIGNALING_TIMEOUT, async {
        loop {
            let (text, message) = receive_tracker_text(browser).await?;
            if message.get("answer").is_some() {
                return Ok::<_, io::Error>(text);
            }
            // A peer may prepare its own offers while answering this one.
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "browser answer timed out"))??;
    offerer
        .send(Message::text(answer))
        .await
        .map_err(io::Error::other)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_relay_waits_for_later_offer_and_retains_both_sockets() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay = tokio::spawn(async move {
        let (first, _) = listener.accept().await?;
        let mut first = accept_async(first).await.map_err(io::Error::other)?;
        let (second, _) = listener.accept().await?;
        let mut second = accept_async(second).await.map_err(io::Error::other)?;
        relay_browser_negotiation(&mut first, &mut second).await
    });

    timeout(Duration::from_secs(3), async {
        let (mut native_peer, _) = connect_async(format!("ws://{address}/announce"))
            .await
            .unwrap();
        let (mut browser_peer, _) = connect_async(format!("ws://{address}/announce"))
            .await
            .unwrap();
        native_peer
            .send(Message::text(
                serde_json::json!({"peer_id": "native-peer", "offers": []}).to_string(),
            ))
            .await
            .unwrap();
        browser_peer
            .send(Message::text(
                serde_json::json!({"peer_id": "browser-peer", "offers": []}).to_string(),
            ))
            .await
            .unwrap();
        native_peer
            .send(Message::text(
                serde_json::json!({
                    "info_hash": "synthetic-info-hash",
                    "peer_id": "native-peer",
                    "offers": [{
                        "offer_id": "synthetic-offer-id",
                        "offer": {"type": "offer", "sdp": "v=0\r\n"},
                    }],
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let relayed_offer = browser_peer.next().await.unwrap().unwrap();
        let Message::Text(relayed_offer) = relayed_offer else {
            panic!("expected relayed text offer");
        };
        let relayed_offer: serde_json::Value =
            serde_json::from_str(relayed_offer.as_str()).unwrap();
        assert_eq!(relayed_offer["offer_id"], "synthetic-offer-id");
        browser_peer
            .send(Message::text(
                serde_json::json!({
                    "peer_id": "browser-peer",
                    "offer_id": "synthetic-offer-id",
                    "answer": {"type": "answer", "sdp": "v=0\r\n"},
                })
                .to_string(),
            ))
            .await
            .unwrap();

        let relayed_answer = native_peer.next().await.unwrap().unwrap();
        let Message::Text(relayed_answer) = relayed_answer else {
            panic!("expected relayed text answer");
        };
        let relayed_answer: serde_json::Value =
            serde_json::from_str(relayed_answer.as_str()).unwrap();
        assert_eq!(relayed_answer["offer_id"], "synthetic-offer-id");
        relay.await.unwrap().unwrap();
    })
    .await
    .expect("local relay test timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires SUPERSEEDR_WEBTORRENT_BROWSER_BIN pointing to a headless-capable browser"]
async fn browser_peer_exchanges_bidirectional_blocks_and_metadata_over_webtorrent() {
    let browser_bin = std::env::var("SUPERSEEDR_WEBTORRENT_BROWSER_BIN")
        .expect("set SUPERSEEDR_WEBTORRENT_BROWSER_BIN to a browser executable");
    let (tracker_url, tracker_listener) = match std::env::var("SUPERSEEDR_WEBTORRENT_TRACKER_URL") {
        Ok(url) => {
            assert!(url.starts_with("ws://") || url.starts_with("wss://"));
            (url, None)
        }
        Err(_) => {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}/announce", listener.local_addr().unwrap());
            (url, Some(listener))
        }
    };
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();
    let payload = match std::env::var("SUPERSEEDR_WEBTORRENT_EXTERNAL_PAYLOAD") {
        Ok(path) => tokio::fs::read(path)
            .await
            .expect("read external WebTorrent payload"),
        Err(_) => (0..16_384).map(|index| (index % 251) as u8).collect(),
    };
    let piece_length = 256 * 1024;
    let piece_hashes = payload
        .chunks(piece_length)
        .flat_map(|piece| sha1::Sha1::digest(piece).to_vec())
        .collect();
    let info = crate::torrent_file::Info {
        name: "external_payload.bin".to_string(),
        piece_length: piece_length as i64,
        pieces: piece_hashes,
        length: payload.len() as i64,
        files: vec![],
        private: None,
        md5sum: None,
        meta_version: None,
        file_tree: None,
    };
    let metadata = serde_bencode::to_bytes(&info).unwrap();
    let piece_count = payload.len().div_ceil(piece_length);
    let info_hash: [u8; 20] = sha1::Sha1::digest(&metadata).into();
    let info_hash_javascript = info_hash
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let metadata_javascript = metadata
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let html = include_str!("browser_probe.html")
        .replace("__TRACKER_URL__", &tracker_url)
        .replace("__INFO_HASH_BYTES__", &info_hash_javascript)
        .replace("__METADATA_BYTES__", &metadata_javascript)
        .replace("__PIECE_LENGTH__", &piece_length.to_string())
        .replace("__PIECE_COUNT__", &piece_count.to_string());

    let (result_tx, mut result_rx) = oneshot::channel::<String>();
    let payload = Arc::new(payload);
    let served_payload = payload.clone();
    let http_task = tokio::spawn(async move {
        let mut result_tx = Some(result_tx);
        loop {
            let (mut stream, _) = http_listener.accept().await.unwrap();
            let mut request = vec![0_u8; 8192];
            let count = stream.read(&mut request).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..count]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            if let Some(status) = path.strip_prefix("/result?status=") {
                let response = b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response).await;
                if let Some(tx) = result_tx.take() {
                    let _ = tx.send(status.to_string());
                }
                return;
            }
            if path == "/" {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            } else if path == "/payload" {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    served_payload.len()
                );
                stream.write_all(header.as_bytes()).await.unwrap();
                stream.write_all(served_payload.as_slice()).await.unwrap();
            } else {
                let _ = stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await;
            }
        }
    });

    let using_external_tracker = tracker_listener.is_none();
    let relay_task = tracker_listener.map(|tracker_listener| {
        tokio::spawn(async move {
            let (first, _) = tracker_listener.accept().await.unwrap();
            let mut first = accept_async(first).await.unwrap();
            let (second, _) = tracker_listener.accept().await.unwrap();
            let mut second = accept_async(second).await.unwrap();
            relay_browser_negotiation(&mut first, &mut second)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(180)).await;
        })
    });

    let browser_profile = tempfile::tempdir().unwrap();
    let mut browser_command = tokio::process::Command::new(browser_bin);
    browser_command
        .kill_on_drop(true)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--disable-background-networking")
        .arg("--no-first-run")
        .arg(format!(
            "--user-data-dir={}",
            browser_profile.path().display()
        ))
        .arg(format!("http://{http_addr}/"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut browser = browser_command.spawn().unwrap();

    if using_external_tracker {
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let peer_id = [b'R'; 20];
    let (_stats_tx, stats_rx) = watch::channel(WebTorrentAnnounceStats::default());
    let (worker_cancel_tx, worker_cancel_rx) = watch::channel(false);
    let (worker_event_tx, mut worker_event_rx) = mpsc::channel(16);
    let worker_task = tokio::spawn(webtorrent_tracker_worker(
        WebTorrentTrackerConfig {
            url: tracker_url,
            info_hash,
            peer_id,
            key: 7,
            num_offers: 1,
            max_incoming_negotiations: 2,
            rtc: WebRtcSessionConfig::loopback(),
        },
        stats_rx,
        worker_cancel_rx,
        worker_event_tx,
    ));

    let stream = timeout(Duration::from_secs(60), async {
        loop {
            tokio::select! {
                result = &mut result_rx => {
                    panic!("browser failed before WebRTC negotiation completed: {}", result.unwrap());
                }
                event = worker_event_rx.recv() => match event {
                    Some(WebTorrentTrackerEvent::PeerReady { stream, .. }) => return stream,
                    Some(WebTorrentTrackerEvent::Failed(error)) => {
                        panic!("WebTorrent tracker worker failed: {error}")
                    }
                    Some(_) => {}
                    None => panic!("WebTorrent tracker worker channel closed"),
                }
            }
        }
    })
    .await
    .expect("browser WebRTC negotiation timed out");

    let (torrent_manager_tx, mut torrent_manager_rx) = mpsc::channel(512);
    let (peer_tx, peer_rx) = mpsc::channel(512);
    let (session_cancel_tx, session_cancel_rx) = watch::channel(false);
    let (session_shutdown_tx, _) = broadcast::channel(1);
    let session = PeerSession::new(PeerSessionParameters {
        info_hash: info_hash.to_vec(),
        torrent_metadata_length: None,
        connection_type: ConnectionType::Outgoing,
        torrent_manager_rx: peer_rx,
        torrent_manager_tx,
        peer_ip_port: "webrtc://browser-probe".to_string(),
        client_id: peer_id.to_vec(),
        global_dl_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
        global_ul_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
        shutdown_tx: session_shutdown_tx.clone(),
        network_scope_id: None,
        session_cancel: session_cancel_rx,
    })
    .with_expected_peer_id([b'J'; 20]);
    let session_task = tokio::spawn(session.run(stream, Vec::new(), None));
    timeout(Duration::from_secs(180), async {
        let mut uploaded_block = false;
        let mut downloaded_bytes = 0;
        let mut received_metadata = false;
        let mut in_flight = 0_usize;
        let mut requested = VecDeque::new();
        for piece in 0..piece_count {
            let piece_size = (payload.len() - piece * piece_length).min(piece_length);
            for begin in (0..piece_size).step_by(16_384) {
                requested.push_back((
                    piece as u32,
                    begin as u32,
                    (piece_size - begin).min(16_384) as u32,
                ));
            }
        }
        let mut received = HashSet::new();

        async fn fill_pipeline(
            peer_tx: &mpsc::Sender<TorrentCommand>,
            requested: &mut VecDeque<(u32, u32, u32)>,
            in_flight: &mut usize,
        ) {
            let mut batch = Vec::new();
            while *in_flight < 16 {
                let Some(request) = requested.pop_front() else {
                    break;
                };
                batch.push(request);
                *in_flight += 1;
            }
            if !batch.is_empty() {
                peer_tx
                    .send(TorrentCommand::BulkRequest(batch))
                    .await
                    .unwrap();
            }
        }

        loop {
            match torrent_manager_rx.recv().await {
                Some(TorrentCommand::SuccessfullyConnected(_)) => {
                    peer_tx.send(TorrentCommand::PeerUnchoke).await.unwrap();
                }
                Some(TorrentCommand::PeerBitfield(_, _)) => {
                    fill_pipeline(&peer_tx, &mut requested, &mut in_flight).await;
                }
                Some(TorrentCommand::RequestUpload(_, piece, begin, length)) => {
                    let offset = piece as usize * piece_length + begin as usize;
                    let end = offset + length as usize;
                    assert!(end <= payload.len());
                    peer_tx
                        .send(TorrentCommand::Upload(
                            piece,
                            begin,
                            payload[offset..end].to_vec(),
                        ))
                        .await
                        .unwrap();
                    uploaded_block = true;
                }
                Some(TorrentCommand::Block(_, piece, begin, block)) => {
                    let offset = piece as usize * piece_length + begin as usize;
                    let end = offset + block.len();
                    assert_eq!(block, payload[offset..end]);
                    if received.insert((piece, begin)) {
                        downloaded_bytes += block.len();
                        in_flight -= 1;
                        fill_pipeline(&peer_tx, &mut requested, &mut in_flight).await;
                    }
                }
                Some(TorrentCommand::MetadataTorrent(torrent, length)) => {
                    assert_eq!(length as usize, metadata.len());
                    assert_eq!(torrent.info_dict_bencode, metadata);
                    assert_eq!(torrent.info.length as usize, payload.len());
                    received_metadata = true;
                }
                Some(_) => {}
                None => panic!("peer session command channel closed"),
            }
            if uploaded_block && downloaded_bytes == payload.len() && received_metadata {
                return;
            }
        }
    })
    .await
    .expect("browser did not complete bidirectional blocks and metadata transfer");

    let result = timeout(Duration::from_secs(10), &mut result_rx)
        .await
        .expect("browser did not report a result")
        .unwrap();
    assert_eq!(result, "ok");

    session_cancel_tx.send_replace(true);
    let _ = session_shutdown_tx.send(());
    worker_cancel_tx.send_replace(true);
    let _ = browser.kill().await;
    let _ = timeout(Duration::from_secs(2), session_task).await;
    let _ = timeout(Duration::from_secs(2), worker_task).await;
    if let Some(relay_task) = relay_task {
        relay_task.abort();
    }
    http_task.await.unwrap();
}
