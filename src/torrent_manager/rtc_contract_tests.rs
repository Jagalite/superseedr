// SPDX-License-Identifier: GPL-3.0-or-later
//! Actual manager + independently implemented Chromium peer over a local tracker.
use super::*;
use crate::resource::ResourceType;
use futures_util::{SinkExt, StreamExt};
use std::{path::PathBuf, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::Command,
    sync::Mutex,
};
use tokio_tungstenite::tungstenite::Message;

async fn relay(listener: TcpListener) {
    let registry = Arc::new(Mutex::new(HashMap::<String, Sender<Message>>::new()));
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.unwrap(); let registry = registry.clone();
                tasks.spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else { return; };
                    let (tx, mut rx) = mpsc::channel(32);
                    let mut identity = None;
                    loop {
                        tokio::select! {
                            message = rx.recv() => { let Some(message) = message else { break; }; if socket.send(message).await.is_err() { break; } }
                            message = socket.next() => {
                                let Some(Ok(Message::Text(text))) = message else { break; };
                                let message: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                                let id = message["peer_id"].as_str().unwrap().to_string();
                                registry.lock().await.insert(id.clone(), tx.clone()); identity = Some(id.clone());
                                if let Some(target) = message["to_peer_id"].as_str() {
                                    if let Some(target) = registry.lock().await.get(target) { let _ = target.try_send(Message::Text(message.to_string().into())); }
                                    continue;
                                }
                                let response = serde_json::json!({"action":"announce","info_hash":message["info_hash"],"interval":1800});
                                if socket.send(Message::Text(response.to_string().into())).await.is_err() { break; }
                                if let Some(offers) = message["offers"].as_array() {
                                    for offer in offers {
                                        let proposal = serde_json::json!({"action":"announce","info_hash":message["info_hash"],"peer_id":id,"offer_id":offer["offer_id"],"offer":offer["offer"]});
                                        for (other, target) in registry.lock().await.iter() {
                                            if other != &id { let _ = target.try_send(Message::Text(proposal.to_string().into())); }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(identity) = identity { registry.lock().await.remove(&identity); }
                });
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

async fn transfer(browser_seeds: bool) {
    let directory = tempfile::tempdir().unwrap();
    let length = 32768 * 3 + 173;
    let payload: Vec<u8> = (0..length).map(|i| (i * 13 + (i >> 7)) as u8).collect();
    let info = crate::torrent_file::Info {
        name: "orbital-payload.bin".into(),
        piece_length: 32768,
        pieces: payload
            .chunks(32768)
            .flat_map(|piece| sha1::Sha1::digest(piece).to_vec())
            .collect(),
        length: length as i64,
        files: vec![],
        private: None,
        md5sum: None,
        meta_version: None,
        file_tree: None,
    };
    let metadata = serde_bencode::to_bytes(&info).unwrap();
    let hash = sha1::Sha1::digest(&metadata).to_vec();
    let file = directory.path().join(&info.name);
    if !browser_seeds {
        tokio::fs::write(&file, &payload).await.unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tracker = format!("ws://{}/announce", listener.local_addr().unwrap());
    let relay = tokio::spawn(relay(listener));
    let configuration = serde_json::json!({"tracker":tracker,"hash":hex::encode(&hash),"peer":hex::encode([91;20]),
        "metadata":hex::encode(&metadata),"length":length,"pieceLength":32768,"mode":if browser_seeds {"seed"} else {"sink"}});
    let mut browser = Command::new("node")
        .arg("tests/rtc-peer.mjs")
        .arg(configuration.to_string())
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = BufReader::new(browser.stdout.take().unwrap()).lines();
    let mut ready = false;
    while let Some(line) = output.next_line().await.unwrap() {
        eprintln!("browser: {line}");
        if line == "READY" {
            ready = true;
            break;
        }
    }
    assert!(ready, "Chromium fixture failed before tracker registration");

    let (resource_stop, _) = broadcast::channel(1);
    let limits = [
        ResourceType::PeerConnection,
        ResourceType::DiskRead,
        ResourceType::DiskWrite,
    ]
    .into_iter()
    .map(|kind| (kind, (32, 32)))
    .chain([(ResourceType::Reserve, (0, 0))])
    .collect();
    let (resources, client) = crate::resource::ResourceManager::new(limits, resource_stop.clone());
    let resource_task = tokio::spawn(resources.run());
    let (_incoming, incoming_peer_rx) = mpsc::channel(8);
    let (commands, manager_command_rx) = mpsc::channel(8);
    let (events, mut event_rx) = mpsc::channel(128);
    let event_task = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            tracing::debug!(?event, "contract manager event");
        }
    });
    let (metrics_tx, mut metrics) = watch::channel(TorrentMetrics::default());
    let settings = Settings {
        client_id: "Q".repeat(20),
        ..Default::default()
    };
    let params = TorrentParameters {
        network_activation: test_network_activation(0),
        dht_handle: crate::dht::service::DhtHandle::disabled(),
        incoming_peer_rx,
        metrics_tx,
        peer_policy_rx: crate::peer_manager::default_policy_receiver(),
        torrent_validation_status: false,
        torrent_data_path: Some(directory.path().into()),
        container_name: None,
        manager_command_rx,
        manager_event_tx: events,
        settings: Arc::new(settings),
        resource_manager: client,
        global_dl_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
        global_ul_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
        file_priorities: HashMap::new(),
    };
    let manager = if browser_seeds {
        let magnet = format!(
            "magnet:?xt=urn:btih:{}&tr={}",
            hex::encode(&hash),
            urlencoding::encode(&tracker)
        );
        TorrentManager::from_magnet(params, magnet_url::Magnet::new(&magnet).unwrap(), &magnet)
            .unwrap()
    } else {
        TorrentManager::from_torrent(
            params,
            Torrent {
                info,
                info_dict_bencode: metadata,
                announce: Some(tracker),
                ..Torrent::default()
            },
        )
        .unwrap()
    };
    let manager_task = tokio::spawn(manager.run(false));
    if browser_seeds {
        loop {
            metrics.changed().await.unwrap();
            let snapshot = metrics.borrow_and_update().clone();
            eprintln!(
                "manager: pieces {}/{}; received {}",
                snapshot.number_of_pieces_completed,
                snapshot.number_of_pieces_total,
                snapshot.session_total_downloaded
            );
            if snapshot.is_complete {
                assert_eq!(snapshot.number_of_pieces_completed, 4);
                assert!(snapshot.session_total_downloaded >= length as u64);
                assert!(snapshot
                    .peers
                    .iter()
                    .chain(snapshot.departed_peers.iter())
                    .any(|peer| peer.address.starts_with("webrtc://")
                        && peer.total_downloaded >= length as u64));
                assert_eq!(tokio::fs::read(&file).await.unwrap(), payload);
                break;
            }
        }
    } else {
        let mut verified = false;
        while let Some(line) = output.next_line().await.unwrap() {
            eprintln!("browser: {line}");
            if line == "VERIFIED" {
                verified = true;
                break;
            }
        }
        assert!(
            verified,
            "independent browser must verify the uploaded bytes"
        );
    }
    if browser_seeds {
        let mut returned = false;
        while let Some(line) = output.next_line().await.unwrap() {
            eprintln!("browser: {line}");
            if line == "METADATA_RETURNED" {
                returned = true;
                break;
            }
        }
        assert!(
            returned,
            "an existing magnet session must serve newly verified metadata"
        );
    }
    commands.send(ManagerCommand::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), manager_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    browser
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"stop\n")
        .await
        .unwrap();
    assert!(browser.wait().await.unwrap().success());
    let _ = resource_stop.send(());
    resource_task.await.unwrap();
    event_task.await.unwrap();
    relay.abort();
    let _ = relay.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires npm ci in web and a Playwright Chromium installation"]
async fn chromium_magnet_download_uses_real_manager() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
    tokio::time::timeout(Duration::from_secs(90), transfer(true))
        .await
        .expect("browser magnet download deadline");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires npm ci in web and a Playwright Chromium installation"]
async fn chromium_downloads_verified_payload_from_real_manager() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
    tokio::time::timeout(Duration::from_secs(90), transfer(false))
        .await
        .expect("browser upload deadline");
}
