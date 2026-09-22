// SPDX-License-Identifier: GPL-3.0-or-later
//! Diagnostic characterization, not a claim about the original live incident.
//! Uses the real native manager, TCP session, hashing, and temporary payload files.
use super::*;
use crate::networking::protocol::{generate_message, parse_message_from_bytes, Message};
use crate::resource::{ResourceManager, ResourceType};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

const PIECES: usize = 200;
const PIECE_BYTES: usize = 16_384;
const STALL_SECONDS: u64 = 97 * 60;
type ManagerRun<'a> = Pin<Box<dyn Future<Output = Result<(), Box<dyn Error + Send + Sync>>> + 'a>>;

enum PeerControl {
    Prepare(oneshot::Sender<()>),
    Unchoke(oneshot::Sender<()>),
    KeepAlive(oneshot::Sender<()>),
    Close(oneshot::Sender<()>),
}

async fn wire_send(writer: &mut tokio::net::tcp::OwnedWriteHalf, message: Message) {
    writer
        .write_all(&generate_message(message).unwrap())
        .await
        .unwrap();
}

async fn synthetic_peer(
    listener: tokio::net::TcpListener,
    mut controls: mpsc::Receiver<PeerControl>,
    requests: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    identity: u8,
) {
    let mut reconnect = false;
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        connections.fetch_add(1, Ordering::SeqCst);
        socket.set_nodelay(true).unwrap();
        let mut handshake = [0; 68];
        socket.read_exact(&mut handshake).await.unwrap();
        handshake[20..28].fill(0);
        handshake[48..68].fill(identity);
        socket.write_all(&handshake).await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let mut bits = vec![0; PIECES.div_ceil(8)];
        if reconnect {
            bits.fill(255);
        } else {
            bits[0] = 128;
        }
        wire_send(&mut writer, Message::Bitfield(bits)).await;
        let (messages, mut received) = mpsc::channel(256);
        let reading = tokio::spawn(async move {
            loop {
                let mut prefix = [0; 4];
                if reader.read_exact(&mut prefix).await.is_err() {
                    break;
                }
                let size = u32::from_be_bytes(prefix) as usize;
                assert!(size < 1024 * 1024);
                let mut frame = vec![0; 4 + size];
                frame[..4].copy_from_slice(&prefix);
                if reader.read_exact(&mut frame[4..]).await.is_err() {
                    break;
                }
                let message = parse_message_from_bytes(&mut std::io::Cursor::new(&frame)).unwrap();
                if messages.send(message).await.is_err() {
                    break;
                }
            }
        });
        let mut prepared = false;
        let mut choking = true;
        loop {
            tokio::select! {
                message = received.recv() => match message {
                    Some(Message::Interested) if !prepared => {
                        choking = false;
                        wire_send(&mut writer, Message::Unchoke).await;
                    }
                    Some(Message::Request(index, offset, length)) => {
                        assert!(!choking, "client requested while remote was choking");
                        assert!(index < PIECES as u32);
                        assert_eq!((offset, length), (0, PIECE_BYTES as u32));
                        requests.fetch_add(1, Ordering::SeqCst);
                        wire_send(&mut writer, Message::Piece(index, offset, vec![0xA7; length as usize])).await;
                    }
                    Some(_) => {}
                    None => break,
                },
                control = controls.recv() => match control {
                    Some(PeerControl::Prepare(ack)) => {
                        prepared = true;
                        reconnect = true;
                        choking = true;
                        wire_send(&mut writer, Message::Choke).await;
                        for index in 1..PIECES {
                            wire_send(&mut writer, Message::Have(index as u32)).await;
                        }
                        let _ = ack.send(());
                    }
                    Some(PeerControl::Unchoke(ack)) => {
                        choking = false;
                        wire_send(&mut writer, Message::Unchoke).await;
                        let _ = ack.send(());
                    }
                    Some(PeerControl::KeepAlive(ack)) => {
                        wire_send(&mut writer, Message::KeepAlive).await;
                        // A harmless availability repeat lets the observer prove
                        // that the session consumed traffic before advancing time.
                        wire_send(&mut writer, Message::Have(0)).await;
                        let _ = ack.send(());
                    }
                    Some(PeerControl::Close(ack)) => {
                        let _ = ack.send(());
                        break;
                    }
                    None => { reading.abort(); return; }
                }
            }
        }
        reading.abort();
    }
}

async fn drive_until(
    run: &mut ManagerRun<'_>,
    metrics: &mut watch::Receiver<TorrentMetrics>,
    label: &str,
    ready: impl Fn(&TorrentMetrics) -> bool,
) {
    timeout(Duration::from_secs(15), async {
        loop {
            if ready(&metrics.borrow()) {
                return;
            }
            tokio::select! {
                result = &mut *run => panic!("manager exited at {label}: {result:?}"),
                result = metrics.changed() => result.unwrap(),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("deadline at {label}: {:?}", metrics.borrow()));
}

async fn drive_for(run: &mut ManagerRun<'_>, duration: Duration) {
    tokio::select! {
        result = &mut *run => panic!("manager exited: {result:?}"),
        _ = tokio::time::sleep(duration) => {},
    }
}

async fn pump(run: &mut ManagerRun<'_>) {
    for _ in 0..32 {
        std::future::poll_fn(|cx| {
            if let std::task::Poll::Ready(result) = run.as_mut().poll(cx) {
                panic!("manager exited while observing stall: {result:?}");
            }
            std::task::Poll::Ready(())
        })
        .await;
        tokio::task::yield_now().await;
    }
}

fn trace(phase: &str, metrics: &TorrentMetrics, requests: usize) {
    println!(
        "[STALL_REPRO] {}",
        serde_json::json!({
            "phase": phase, "pieces": metrics.number_of_pieces_completed,
            "total_pieces": metrics.number_of_pieces_total,
            "connected": metrics.number_of_successfully_connected_peers,
            "choked": metrics.peers.iter().filter(|peer| peer.peer_choking).count(),
            "wire_requests": requests, "control": format!("{:?}", metrics.torrent_control_state),
        })
    );
}

async fn run_case(
    pressure_peers: usize,
    repeat_unchoke: bool,
    peer_count: usize,
    churn_rounds: usize,
) {
    assert!(pressure_peers <= peer_count);
    let saturate = pressure_peers > 0;
    // Opt in only when replaying this diagnostic against the pre-fix source.
    let expect_drops = std::env::var_os("SUPERSEEDR_REPRO_EXPECT_DROPS").is_some();
    let expect_stall = expect_drops && pressure_peers == peer_count;
    let directory = tempfile::tempdir().unwrap();
    let mut payload_directory = directory.path().to_path_buf();
    let requests = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(AtomicUsize::new(0));
    let mut peers = Vec::new();
    let mut controls = Vec::new();
    let mut addresses = Vec::new();
    for index in 0..peer_count {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        addresses.push(listener.local_addr().unwrap());
        let (control, receive) = mpsc::channel(16);
        controls.push(control);
        peers.push(tokio::spawn(synthetic_peer(
            listener,
            receive,
            requests.clone(),
            connections.clone(),
            index as u8 + 7,
        )));
    }
    let mut params = resource_tests::build_test_params();
    params.torrent_data_path = Some(payload_directory.clone());
    let mut settings = (*params.settings).clone();
    settings.client_id = "-SS0001-123456789012".into();
    params.settings = Arc::new(settings);
    let (mut commands, receive) = mpsc::channel(100);
    params.manager_command_rx = receive;
    let (events, mut event_rx) = mpsc::channel(1000);
    params.manager_event_tx = events.clone();
    let event_task = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    let (metrics_tx, mut metrics) = watch::channel(TorrentMetrics::default());
    params.metrics_tx = metrics_tx;
    let (shutdown, _) = broadcast::channel(1);
    let (resource, client) = ResourceManager::new(
        HashMap::from([
            (ResourceType::Reserve, (0, 0)),
            (ResourceType::PeerConnection, (8, 32)),
            (ResourceType::DiskRead, (4, 32)),
            (ResourceType::DiskWrite, (4, 256)),
        ]),
        shutdown.clone(),
    );
    params.resource_manager = client.clone();
    let resource_task = tokio::spawn(resource.run());
    let mut torrent = resource_tests::create_dummy_torrent(PIECES);
    torrent.announce = None;
    torrent.info.name = "orbital-stall-fixture.bin".into();
    torrent.info.pieces = sha1::Sha1::digest(vec![0xA7; PIECE_BYTES]).repeat(PIECES);
    torrent.info_dict_bencode = serde_bencode::to_bytes(&torrent.info).unwrap();
    let mut manager = TorrentManager::from_torrent(params, torrent.clone()).unwrap();
    manager.data_rate_ms = 20;
    let mut inbox = manager.torrent_manager_tx.clone();
    for address in &addresses {
        manager.connect_to_peer(*address);
    }
    let mut run: ManagerRun<'_> = Box::pin(manager.run(false));
    drive_until(&mut run, &mut metrics, "initial 0.5%", |m| {
        m.number_of_pieces_completed == 1
    })
    .await;
    drive_until(&mut run, &mut metrics, "all peer handshakes", |m| {
        m.peers.len() == peer_count && m.peers.iter().all(|p| p.bitfield.len() == PIECES)
    })
    .await;
    trace(
        "initial_progress",
        &metrics.borrow(),
        requests.load(Ordering::SeqCst),
    );

    for round in 0..churn_rounds {
        for control in &controls {
            let (ack, closed) = oneshot::channel();
            control.send(PeerControl::Close(ack)).await.unwrap();
            closed.await.unwrap();
        }
        // Let the OS deliver EOF before accelerating the cleanup interval.
        tokio::time::sleep(Duration::from_millis(20)).await;
        tokio::time::pause();
        let clock_guard = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        pump(&mut run).await;
        tokio::time::advance(Duration::from_secs(4)).await;
        pump(&mut run).await;
        tokio::time::advance(Duration::from_millis(20)).await;
        pump(&mut run).await;
        clock_guard.abort();
        tokio::time::resume();
        drive_until(&mut run, &mut metrics, "remote disconnect cleanup", |m| {
            m.peers.is_empty()
        })
        .await;
        let resources = client.snapshot().await.unwrap();
        for resource in resources.resources.values() {
            assert_eq!(
                (resource.in_use, resource.queued),
                (0, 0),
                "resource leak after churn round {round}"
            );
        }
        // Spread the real connection lifecycle events over 30 hours of Tokio
        // time while disconnected. This does not age std::time state or the UI.
        tokio::time::pause();
        let clock_guard = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        tokio::time::advance(Duration::from_secs(30 * 3600 / churn_rounds as u64 - 4)).await;
        pump(&mut run).await;
        clock_guard.abort();
        tokio::time::resume();
        for address in &addresses {
            commands
                .send(ManagerCommand::ConnectToPeer(*address))
                .await
                .unwrap();
        }
        drive_until(&mut run, &mut metrics, "churn reconnection", |m| {
            m.peers.len() == peer_count && m.peers.iter().all(|p| p.bitfield.len() == PIECES)
        })
        .await;
        assert_eq!(metrics.borrow().number_of_pieces_completed, 1);
    }
    if churn_rounds > 0 {
        let warmup_connections = peer_count * (churn_rounds + 1);
        assert_eq!(connections.load(Ordering::SeqCst), warmup_connections);
        commands.send(ManagerCommand::Shutdown).await.unwrap();
        timeout(Duration::from_secs(10), &mut run)
            .await
            .unwrap()
            .unwrap();
        // Manager exit can precede asynchronous peer-session cancellation and
        // permit Drop delivery. Observe bounded cleanup, not one scheduling turn.
        let cleanup_started = std::time::Instant::now();
        loop {
            let resources = client.snapshot().await.unwrap();
            if resources
                .resources
                .values()
                .all(|resource| resource.in_use == 0 && resource.queued == 0)
            {
                break;
            }
            assert!(
                cleanup_started.elapsed() < Duration::from_secs(2),
                "resources retained after warmup shutdown: {resources:?}"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        println!(
            "[STALL_REPRO] warmup_shutdown_release_wait_us={}",
            cleanup_started.elapsed().as_micros()
        );
        println!("[STALL_REPRO] churn_rounds={churn_rounds} remote_disconnects={} warmup_tcp_connections={warmup_connections} accelerated_idle_hours=30 resource_leaks=0", churn_rounds * peer_count);

        // Keep the shared resource service alive, but start a fresh torrent and
        // payload after the warmup. Client history and torrent age are distinct.
        // The app's UI, peer-policy service, and wall-clock ages are not covered.
        requests.store(0, Ordering::SeqCst);
        connections.store(0, Ordering::SeqCst);
        payload_directory = directory.path().join("fresh-after-churn");
        tokio::fs::create_dir(&payload_directory).await.unwrap();
        let mut params = resource_tests::build_test_params();
        params.torrent_data_path = Some(payload_directory.clone());
        let mut settings = (*params.settings).clone();
        settings.client_id = "-SS0001-123456789012".into();
        params.settings = Arc::new(settings);
        let (fresh_commands, receive) = mpsc::channel(100);
        commands = fresh_commands;
        params.manager_command_rx = receive;
        params.manager_event_tx = events;
        let (metrics_tx, fresh_metrics) = watch::channel(TorrentMetrics::default());
        metrics = fresh_metrics;
        params.metrics_tx = metrics_tx;
        params.resource_manager = client.clone();
        let mut manager = TorrentManager::from_torrent(params, torrent).unwrap();
        manager.data_rate_ms = 20;
        inbox = manager.torrent_manager_tx.clone();
        for address in &addresses {
            manager.connect_to_peer(*address);
        }
        run = Box::pin(manager.run(false));
        drive_until(&mut run, &mut metrics, "fresh torrent after churn", |m| {
            m.number_of_pieces_completed == 1
                && m.peers.len() == peer_count
                && m.peers.iter().all(|p| p.bitfield.len() == PIECES)
        })
        .await;
        trace(
            "fresh_torrent_after_resource_service_churn",
            &metrics.borrow(),
            requests.load(Ordering::SeqCst),
        );
    }
    for control in &controls {
        let (ack, prepared) = oneshot::channel();
        control.send(PeerControl::Prepare(ack)).await.unwrap();
        prepared.await.unwrap();
        drive_for(&mut run, Duration::from_millis(20)).await;
    }
    drive_until(
        &mut run,
        &mut metrics,
        "remote choking with all pieces available",
        |m| {
            m.peers.len() == peer_count
                && m.peers
                    .iter()
                    .all(|p| p.peer_choking && p.bitfield.iter().filter(|v| **v).count() == PIECES)
        },
    )
    .await;
    let mut filled = 0;
    if saturate {
        while inbox.try_send(TorrentCommand::NotInterested).is_ok() {
            filled += 1;
        }
        assert_eq!(inbox.capacity(), 0);
    }
    for control in controls.iter().take(pressure_peers) {
        let (ack, sent) = oneshot::channel();
        control.send(PeerControl::Unchoke(ack)).await.unwrap();
        sent.await.unwrap();
        // Stagger notifications while this one manager inbox remains full.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A bounded scheduling pause: only the manager actor is not polled, while
    // the real socket reader/session remain runnable. Capacity then fully returns.
    tokio::time::sleep(Duration::from_millis(50)).await;
    drive_for(&mut run, Duration::from_millis(200)).await;
    println!("[STALL_REPRO] saturated={saturate} injected_mailbox_commands={filled} pressure_peers={pressure_peers} peer_count={peer_count}");
    trace(
        "after_pressure_interval",
        &metrics.borrow(),
        requests.load(Ordering::SeqCst),
    );
    if saturate && expect_drops {
        assert_eq!(metrics.borrow().number_of_pieces_completed, 1);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(metrics.borrow().peers.iter().all(|p| p.peer_choking));
        assert_eq!(inbox.capacity(), inbox.max_capacity());
    }
    if saturate && !expect_drops {
        // Completion alone can hide lost notifications: one useful peer is enough
        // to finish. Check every identity before allowing late peers to unchoke.
        drive_until(
            &mut run,
            &mut metrics,
            "all pressured peers unchoked before late notifications",
            |m| {
                m.peers.len() == peer_count
                    && (0..peer_count).all(|index| {
                        m.peers
                            .iter()
                            .find(|peer| peer.peer_id == [index as u8 + 7; 20])
                            .is_some_and(|peer| peer.peer_choking == (index >= pressure_peers))
                    })
            },
        )
        .await;
        trace(
            "all_pressured_peers_unchoked_before_late_notifications",
            &metrics.borrow(),
            requests.load(Ordering::SeqCst),
        );
    }

    // Peers outside the blocked interval announce only after the inbox drains.
    for control in controls.iter().skip(pressure_peers) {
        let (ack, sent) = oneshot::channel();
        control.send(PeerControl::Unchoke(ack)).await.unwrap();
        sent.await.unwrap();
        drive_for(&mut run, Duration::from_millis(20)).await;
    }
    if expect_stall {
        assert_eq!(metrics.borrow().number_of_pieces_completed, 1);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(metrics.borrow().peers.iter().all(|p| p.peer_choking));
        assert_eq!(inbox.capacity(), inbox.max_capacity());
        // Advance session timers only after disk work has settled. A yielding
        // task prevents Tokio's automatic clock jumps while real TCP is pending.
        tokio::time::pause();
        let clock_guard = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        for _ in 0..STALL_SECONDS / 10 {
            for control in &controls {
                let (ack, sent) = oneshot::channel();
                control.send(PeerControl::KeepAlive(ack)).await.unwrap();
                sent.await.unwrap();
            }
            let delivery = std::time::Instant::now();
            while inbox.capacity() > inbox.max_capacity() - peer_count {
                assert!(
                    delivery.elapsed() < Duration::from_secs(2),
                    "peer liveness delivery deadline"
                );
                tokio::task::yield_now().await;
            }
            pump(&mut run).await;
            tokio::time::advance(Duration::from_secs(10)).await;
            pump(&mut run).await;
            assert_eq!(metrics.borrow().number_of_pieces_completed, 1);
            assert_eq!(
                metrics.borrow().number_of_successfully_connected_peers,
                peer_count
            );
            assert_eq!(requests.load(Ordering::SeqCst), 1);
        }
        clock_guard.abort();
        tokio::time::resume();
        trace(
            "97_minutes_virtual_stalled",
            &metrics.borrow(),
            requests.load(Ordering::SeqCst),
        );
        if repeat_unchoke {
            let (ack, sent) = oneshot::channel();
            controls[0].send(PeerControl::Unchoke(ack)).await.unwrap();
            sent.await.unwrap();
        } else {
            commands.send(ManagerCommand::Pause).await.unwrap();
            drive_until(&mut run, &mut metrics, "paused", |m| {
                m.torrent_control_state == crate::app::TorrentControlState::Paused
            })
            .await;
            commands.send(ManagerCommand::Resume).await.unwrap();
            // Rediscovery is explicit: this fixture has no tracker or DHT service.
            commands
                .send(ManagerCommand::ConnectToPeer(addresses[0]))
                .await
                .unwrap();
        }
    }
    drive_until(&mut run, &mut metrics, "completed", |m| {
        m.number_of_pieces_completed == PIECES as u32
    })
    .await;
    trace(
        if saturate && !expect_drops {
            "completed_after_inbox_drain_without_recovery_action"
        } else if repeat_unchoke {
            "completed_after_repeated_unchoke_without_reconnect"
        } else if expect_stall {
            "completed_after_pause_resume"
        } else if saturate {
            "completed_via_late_unchoke_without_pause"
        } else {
            "healthy_control_completed"
        },
        &metrics.borrow(),
        requests.load(Ordering::SeqCst),
    );
    let expected_connections = peer_count + usize::from(expect_stall && !repeat_unchoke);
    assert_eq!(connections.load(Ordering::SeqCst), expected_connections);
    println!("[STALL_REPRO] verified_tcp_connections={expected_connections}");
    let bytes = tokio::fs::read(payload_directory.join("orbital-stall-fixture.bin"))
        .await
        .unwrap();
    assert_eq!(bytes, vec![0xA7; PIECES * PIECE_BYTES]);
    commands.send(ManagerCommand::Shutdown).await.unwrap();
    timeout(Duration::from_secs(10), &mut run)
        .await
        .unwrap()
        .unwrap();
    for peer in peers {
        peer.abort();
    }
    event_task.abort();
    let _ = shutdown.send(());
    resource_task.await.unwrap();
}

#[tokio::test]
#[ignore = "manual localhost diagnostic; verifies recovery after inbox backpressure"]
async fn stall_reproduction_healthy_control() {
    run_case(0, false, 1, 0).await;
}

#[tokio::test]
#[ignore = "manual localhost diagnostic; verifies recovery after inbox backpressure"]
async fn stall_reproduction_lost_unchoke_recovers_after_pause_resume() {
    run_case(1, false, 1, 0).await;
}

#[tokio::test]
#[ignore = "manual localhost diagnostic; verifies recovery after inbox backpressure"]
async fn stall_reproduction_repeated_unchoke_recovers_without_reconnect() {
    run_case(1, true, 1, 0).await;
}

#[tokio::test]
#[ignore = "manual localhost diagnostic; verifies recovery after inbox backpressure"]
async fn stall_reproduction_four_peers_unchoke_during_same_full_inbox() {
    run_case(4, true, 4, 0).await;
}

#[tokio::test]
#[ignore = "manual localhost diagnostic; verifies recovery after inbox backpressure"]
async fn stall_reproduction_four_peers_late_unchoke_keeps_download_moving() {
    run_case(3, false, 4, 0).await;
}

#[tokio::test]
#[ignore = "manual lifecycle diagnostic; accelerated idle time is not a client uptime soak"]
async fn stall_reproduction_fresh_torrent_after_resource_service_churn() {
    run_case(3, false, 4, 32).await;
}
