// SPDX-License-Identifier: GPL-3.0-or-later
//! Opt-in external image round trip with independent browser protocol and production OPFS.
use super::*;
use crate::resource::ResourceType;
use sha2::Sha256;
use std::{path::PathBuf, process::Stdio};
use tokio::io::AsyncReadExt;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

async fn checksum(path: &std::path::Path) -> String {
    let mut file = tokio::fs::File::open(path).await.unwrap();
    let mut hash = Sha256::new();
    let mut bytes = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut bytes).await.unwrap();
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    hex::encode(hash.finalize())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires IMAGE_ACCEPTANCE_CONFIG, an external verified image, Chromium and browser client bundle"]
#[allow(clippy::assertions_on_constants)] // Runtime feature guard for this opt-in test.
async fn external_image_roundtrip_over_public_tracker() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(tracing::Level::WARN)
        .with_target(
            "superseedr::torrent_manager::manager::rtc",
            tracing::Level::DEBUG,
        )
        .with_target("superseedr::networking::webtorrent", tracing::Level::DEBUG);
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_test_writer()
                .with_filter(filter),
        )
        .try_init();
    // Keep this an ignored-test runtime guard so every feature matrix can compile it.
    assert!(
        !cfg!(feature = "pex") && !cfg!(feature = "dht"),
        "use --no-default-features --features webtorrent"
    );
    let config_path =
        std::env::var("IMAGE_ACCEPTANCE_CONFIG").expect("external input configuration");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
    let phase_seconds = config["phase_timeout_seconds"].as_u64().unwrap_or(900);
    tokio::time::timeout(Duration::from_secs(phase_seconds.saturating_mul(2).saturating_add(240)), async {
        let directory = PathBuf::from(config["output"].as_str().unwrap());
        tokio::fs::create_dir(&directory).await.expect("destination must be new and empty");
        let source = PathBuf::from(config["source"].as_str().unwrap());
        let expected = config["sha256"].as_str().unwrap().to_string();
        assert_eq!(checksum(&source).await, expected, "publisher checksum of source");
        let mut torrent = crate::torrent_file::parser::from_bytes(&std::fs::read(config["torrent"].as_str().unwrap()).unwrap()).unwrap();
        assert!(torrent.info.files.is_empty(), "single-file external image");
        let length = torrent.info.length as u64;
        assert_eq!(tokio::fs::metadata(&source).await.unwrap().len(), length);
        let hash = sha1::Sha1::digest(&torrent.info_dict_bencode).to_vec();
        let tracker = config["tracker"].as_str().unwrap().to_string();
        assert!(tracker.starts_with("wss://"), "public secure tracker required");
        torrent.announce = Some(tracker.clone());
        torrent.announce_list = None;
        torrent.url_list = None;
        let magnet = format!("magnet:?xt=urn:btih:{}&tr={}", hex::encode(&hash), urlencoding::encode(&tracker));
        config["info"] = hex::encode(&torrent.info_dict_bencode).into();
        config["magnet"] = magnet.clone().into();
        config["length"] = length.into();
        config["hash"] = hex::encode(&hash).into();
        let native_file = directory.join(&torrent.info.name);
        let seeding_only = config["native_seed"].as_str().map(str::to_owned);
        if let Some(path) = &seeding_only {
            assert_eq!(checksum(std::path::Path::new(path)).await, expected);
            tokio::fs::copy(path, &native_file).await.unwrap();
        }
        let mut outcomes = Vec::new();
        for download in [true, false].into_iter().filter(|download| !download || seeding_only.is_none()) {
            let phase = if download { "download" } else { "seed" };
            config["mode"] = if download { "seed" } else { "sink" }.into();
            config["export"] = directory.join("browser-export.iso").to_str().unwrap().into();
            let phase_config = directory.join(format!("{phase}.json"));
            tokio::fs::write(&phase_config, serde_json::to_vec(&config).unwrap()).await.unwrap();
            let mut browser = Command::new("node").arg("tests/rtc-image.mjs").arg(phase_config)
                .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"))
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true).spawn().unwrap();
            let mut lines = BufReader::new(browser.stdout.take().unwrap()).lines();
            let (ready_tx, ready) = tokio::sync::oneshot::channel();
            let (verified_tx, mut verified) = tokio::sync::oneshot::channel();
            let output_task = tokio::spawn(async move {
                let mut ready = Some(ready_tx);
                let mut verified = Some(verified_tx);
                while let Some(line) = lines.next_line().await.unwrap() {
                    eprintln!("browser: {line}");
                    if line == "READY" { if let Some(tx) = ready.take() { let _ = tx.send(()); } }
                    if line.starts_with("VERIFIED ") { if let Some(tx) = verified.take() { let _ = tx.send(line); } }
                }
            });
            tokio::time::timeout(Duration::from_secs(120), ready).await.expect("browser ready deadline").unwrap();
            let (resource_stop, _) = broadcast::channel(1);
            let limits = [ResourceType::PeerConnection, ResourceType::DiskRead, ResourceType::DiskWrite]
                .into_iter().map(|kind| (kind, (32, 32))).chain([(ResourceType::Reserve, (0, 0))]).collect();
            let (resources, client) = crate::resource::ResourceManager::new(limits, resource_stop.clone());
            let resource_task = tokio::spawn(resources.run());
            let (_incoming, incoming_peer_rx) = mpsc::channel(8);
            let (commands, manager_command_rx) = mpsc::channel(8);
            let (events, mut event_rx) = mpsc::channel(128);
            let event_task = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
            let (metrics_tx, mut metrics) = watch::channel(TorrentMetrics::default());
            let settings = Settings { client_id: "Q".repeat(20), ..Default::default() };
            let params = TorrentParameters {
                network_activation: test_network_activation(0),
                dht_handle: crate::dht::service::DhtHandle::disabled(), incoming_peer_rx, metrics_tx,
                peer_policy_rx: crate::peer_manager::default_policy_receiver(),
                torrent_validation_status: false, torrent_data_path: Some(directory.clone()), container_name: None,
                manager_command_rx, manager_event_tx: events, settings: Arc::new(settings), resource_manager: client,
                global_dl_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)),
                global_ul_bucket: Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY)), file_priorities: HashMap::new(),
            };
            // No incoming native listener, DHT, PEX peers, HTTP/UDP trackers or web seeds.
            let manager = if download {
                assert!(!native_file.exists());
                TorrentManager::from_magnet(params, magnet_url::Magnet::new(&magnet).unwrap(), &magnet).unwrap()
            } else {
                // A new manager must recheck the actual downloaded file before seeding it.
                TorrentManager::from_torrent(params, torrent.clone()).unwrap()
            };
            let started = std::time::Instant::now();
            let manager_task = tokio::spawn(manager.run(false));
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            let deadline = tokio::time::sleep(Duration::from_secs(config["phase_timeout_seconds"].as_u64().unwrap_or(900)));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => panic!("{phase} phase deadline"),
                    changed = metrics.changed() => { changed.unwrap(); }
                    line = &mut verified, if !download => {
                        assert!(line.unwrap().contains(&expected), "browser durable export checksum");
                        break;
                    }
                    _ = tick.tick() => {
                        let m = metrics.borrow();
                        eprintln!("{phase}: pieces {}/{}, downloaded {}, uploaded {}, peers {}, elapsed {:.1}s", m.number_of_pieces_completed, m.number_of_pieces_total, m.session_total_downloaded, m.session_total_uploaded, m.peers.len(), started.elapsed().as_secs_f64());
                    }
                }
                let snapshot = metrics.borrow_and_update().clone();
                assert_eq!(snapshot.tcp_peer_count + snapshot.utp_peer_count, 0);
                assert!(snapshot.peers.iter().chain(&snapshot.departed_peers).all(|p| p.address.starts_with("webrtc://")), "all payload peers must be RTC");
                if download && snapshot.is_complete { break; }
            }
            let snapshot = metrics.borrow().clone();
            assert!(snapshot.is_complete);
            assert_eq!(snapshot.number_of_pieces_completed as usize, torrent.info.pieces.len() / 20);
            if download { assert!(snapshot.session_total_downloaded >= length); }
            assert_eq!(checksum(&native_file).await, expected);
            if !download { assert_eq!(checksum(&directory.join("browser-export.iso")).await, expected); }
            let outcome = serde_json::json!({"phase":phase,"seconds":started.elapsed().as_secs_f64(),
                "size":length,"pieces":snapshot.number_of_pieces_completed,"downloaded":snapshot.session_total_downloaded,
                "uploaded":snapshot.session_total_uploaded,"sha256":expected,"tcp_peers":snapshot.tcp_peer_count,"utp_peers":snapshot.utp_peer_count});
            eprintln!("RESULT {outcome}"); outcomes.push(outcome);
            tokio::fs::write(directory.join("results.json"), serde_json::to_vec_pretty(&outcomes).unwrap()).await.unwrap();
            commands.send(ManagerCommand::Shutdown).await.unwrap();
            tokio::time::timeout(Duration::from_secs(15), manager_task).await.unwrap().unwrap().unwrap();
            browser.stdin.as_mut().unwrap().write_all(b"stop\n").await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(15), browser.wait()).await.unwrap().unwrap().success());
            output_task.await.unwrap();
            let _ = resource_stop.send(()); resource_task.await.unwrap(); event_task.await.unwrap();
        }
        tokio::fs::write(directory.join("results.json"), serde_json::to_vec_pretty(&outcomes).unwrap()).await.unwrap();
    }).await.expect("external image round trip deadline");
}
