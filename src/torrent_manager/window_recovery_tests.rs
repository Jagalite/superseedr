// SPDX-License-Identifier: GPL-3.0-or-later
//! Real TCP/manager/storage characterization of recovery after competing downloads.
//! The constrained case injects the initial unequal bandwidth allocation; it does
//! not claim to reproduce the cause of that allocation in a live swarm.
use super::*;
use crate::networking::protocol::{generate_message, parse_message_from_bytes, Message};
use crate::resource::{ResourceManager, ResourceType};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinSet;

const BLOCK_BYTES: usize = 16_384;
const PIECE_COUNTS: [usize; 3] = [512, 512, 4096];

#[derive(Default)]
struct WireStats {
    requests: AtomicU64,
    responses: AtomicU64,
    queued: AtomicU64,
}

async fn send(writer: &mut tokio::net::tcp::OwnedWriteHalf, message: Message) {
    writer
        .write_all(&generate_message(message).unwrap())
        .await
        .unwrap();
}

async fn peer(
    listener: tokio::net::TcpListener,
    index: usize,
    constrained: bool,
    released: Arc<AtomicBool>,
    stats: Arc<WireStats>,
) {
    let (mut socket, _) = listener.accept().await.unwrap();
    socket.set_nodelay(true).unwrap();
    let mut handshake = [0; 68];
    socket.read_exact(&mut handshake).await.unwrap();
    handshake[20..28].fill(0);
    handshake[48..68].fill(index as u8 + 1);
    socket.write_all(&handshake).await.unwrap();
    let (mut reader, mut writer) = socket.into_split();
    send(
        &mut writer,
        Message::Bitfield(vec![255; PIECE_COUNTS[index] / 8]),
    )
    .await;
    send(&mut writer, Message::Unchoke).await;

    let (tx, mut rx) = mpsc::channel(1024);
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        loop {
            let mut prefix = [0; 4];
            if reader.read_exact(&mut prefix).await.is_err() {
                break;
            }
            let length = u32::from_be_bytes(prefix) as usize;
            assert!(length < 1024 * 1024);
            let mut frame = vec![0; 4 + length];
            frame[..4].copy_from_slice(&prefix);
            if reader.read_exact(&mut frame[4..]).await.is_err() {
                break;
            }
            let message = parse_message_from_bytes(&mut std::io::Cursor::new(&frame)).unwrap();
            if tx.send(message).await.is_err() {
                break;
            }
        }
    });

    let mut pending = VecDeque::new();
    let mut next_send = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(4));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            message = rx.recv() => match message {
                Some(Message::Request(piece, offset, length)) => {
                    assert!((piece as usize) < PIECE_COUNTS[index]);
                    assert_eq!((offset, length), (0, BLOCK_BYTES as u32));
                    stats.requests.fetch_add(1, Ordering::Relaxed);
                    // Delay each response independently, allowing pipelining.
                    pending.push_back((tokio::time::Instant::now() + Duration::from_millis(250), piece));
                }
                Some(Message::Cancel(piece, _, _)) => pending.retain(|(_, p)| *p != piece),
                Some(_) => {},
                None => return,
            },
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                if now >= next_send && pending.front().is_some_and(|(due, _)| *due <= now) {
                    let (_, piece) = pending.pop_front().unwrap();
                    send(&mut writer, Message::Piece(piece, 0, vec![0xA7; BLOCK_BYTES])).await;
                    stats.responses.fetch_add(1, Ordering::Relaxed);
                    let spacing = if index == 2 && constrained && !released.load(Ordering::Relaxed) {
                        250 // 64 KiB/s while the two smaller torrents download.
                    } else {
                        4 // Up to 4 MiB/s, subject to the client's request window.
                    };
                    next_send = now + Duration::from_millis(spacing);
                }
            },
        }
        stats.queued.store(pending.len() as u64, Ordering::Relaxed);
    }
}

async fn run(constrained: bool) {
    let directory = tempfile::tempdir().unwrap();
    let released = Arc::new(AtomicBool::new(false));
    let stats: Vec<_> = (0..3).map(|_| Arc::new(WireStats::default())).collect();
    let (shutdown, _) = broadcast::channel(1);
    let (resources, client) = ResourceManager::new(
        HashMap::from([
            (ResourceType::Reserve, (0, 0)),
            (ResourceType::PeerConnection, (16, 64)),
            (ResourceType::DiskRead, (4, 64)),
            (ResourceType::DiskWrite, (4, 256)),
        ]),
        shutdown.clone(),
    );
    let mut services = JoinSet::new();
    services.spawn(resources.run());
    let (events, mut event_rx) = mpsc::channel(10000);
    services.spawn(async move { while event_rx.recv().await.is_some() {} });
    let bucket = Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY));
    let mut managers = JoinSet::new();
    let mut peers = JoinSet::new();
    let mut commands = Vec::new();
    let mut metrics = Vec::new();
    for index in 0..3 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer_released = released.clone();
        let peer_stats = stats[index].clone();
        peers.spawn(async move {
            peer(listener, index, constrained, peer_released, peer_stats).await;
            index
        });
        let mut params = resource_tests::build_test_params();
        let mut settings = (*params.settings).clone();
        settings.client_id = "-SS0001-123456789012".into();
        params.settings = Arc::new(settings);
        params.torrent_data_path = Some(directory.path().to_path_buf());
        params.resource_manager = client.clone();
        params.global_dl_bucket = bucket.clone();
        params.manager_event_tx = events.clone();
        let (tx, rx) = mpsc::channel(100);
        params.manager_command_rx = rx;
        commands.push(tx);
        let (tx, rx) = watch::channel(TorrentMetrics::default());
        params.metrics_tx = tx;
        metrics.push(rx);
        let mut torrent = resource_tests::create_dummy_torrent(PIECE_COUNTS[index]);
        torrent.announce = None;
        torrent.info.name = format!("orbital-recovery-{index}.bin");
        torrent.info.pieces =
            sha1::Sha1::digest(vec![0xA7; BLOCK_BYTES]).repeat(PIECE_COUNTS[index]);
        torrent.info_dict_bencode = serde_bencode::to_bytes(&torrent.info).unwrap();
        let mut manager = TorrentManager::from_torrent(params, torrent).unwrap();
        manager.data_rate_ms = 100;
        manager.connect_to_peer(address);
        managers.spawn(async move { manager.run(false).await });
    }

    let started = tokio::time::Instant::now();
    let mut release_at = None;
    let mut previous = [0; 3];
    for second in 1..=90 {
        tokio::time::sleep_until(started + Duration::from_secs(second)).await;
        assert!(managers.try_join_next().is_none(), "manager exited early");
        while let Some(result) = peers.try_join_next() {
            let index = result.unwrap();
            assert_eq!(
                metrics[index].borrow().number_of_pieces_completed as usize,
                PIECE_COUNTS[index],
                "synthetic peer exited before its torrent completed"
            );
        }
        let completed: Vec<_> = metrics
            .iter()
            .map(|m| m.borrow().number_of_pieces_completed)
            .collect();
        if release_at.is_none()
            && completed[0] as usize == PIECE_COUNTS[0]
            && completed[1] as usize == PIECE_COUNTS[1]
        {
            release_at = Some(second);
            released.store(true, Ordering::Relaxed);
        }
        let mut rates = Vec::new();
        for (index, stat) in stats.iter().enumerate() {
            let total = stat.responses.load(Ordering::Relaxed);
            rates.push((total - previous[index]) * BLOCK_BYTES as u64);
            previous[index] = total;
        }
        println!(
            "[WINDOW_RECOVERY] {}",
            serde_json::json!({
                "constrained": constrained, "second": second, "released_at": release_at,
                "wire_bytes_per_second": rates, "verified_pieces": completed,
                "wire_requests": stats.iter().map(|s| s.requests.load(Ordering::Relaxed)).collect::<Vec<_>>(),
                "queued_requests": stats.iter().map(|s| s.queued.load(Ordering::Relaxed)).collect::<Vec<_>>(),
                "connected_peers": metrics.iter().map(|m| m.borrow().number_of_successfully_connected_peers).collect::<Vec<_>>(),
            })
        );
        if release_at.is_some_and(|released| second >= released + 35) {
            break;
        }
    }
    assert!(release_at.is_some(), "smaller torrents did not complete");
    assert!(metrics[2].borrow().number_of_pieces_completed > 0);
    for command in commands {
        command.send(ManagerCommand::Shutdown).await.unwrap();
    }
    timeout(Duration::from_secs(10), async {
        while let Some(result) = managers.join_next().await {
            result.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
    for (index, piece_count) in PIECE_COUNTS.iter().enumerate().take(2) {
        let bytes = tokio::fs::read(
            directory
                .path()
                .join(format!("orbital-recovery-{index}.bin")),
        )
        .await
        .unwrap();
        assert_eq!(bytes.len(), piece_count * BLOCK_BYTES);
        assert!(bytes.iter().all(|byte| *byte == 0xA7));
    }
    peers.shutdown().await;
    let _ = shutdown.send(());
    services.shutdown().await;
}

#[tokio::test]
#[ignore = "real-time three-torrent TCP characterization; run explicitly"]
async fn synthetic_window_recovery_equal_capacity() {
    run(false).await;
}

#[tokio::test]
#[ignore = "real-time three-torrent TCP characterization; run explicitly"]
async fn synthetic_window_recovery_after_contention() {
    run(true).await;
}
