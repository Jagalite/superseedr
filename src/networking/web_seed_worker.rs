// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::networking::runtime::NetworkHttpClient;
use crate::torrent_manager::command::TorrentCommand;
use reqwest::header::RANGE;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tracing::{event, Level};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagerSendOutcome {
    Sent,
    Invalidated,
    Stopped,
}

async fn send_manager_command(
    manager_tx: &Sender<TorrentCommand>,
    command: TorrentCommand,
    shutdown_rx: &mut broadcast::Receiver<()>,
    network_invalidation_rx: &mut watch::Receiver<bool>,
) -> ManagerSendOutcome {
    tokio::select! {
        biased;
        changed = network_invalidation_rx.changed() => {
            if changed.is_err() || *network_invalidation_rx.borrow() {
                ManagerSendOutcome::Invalidated
            } else {
                ManagerSendOutcome::Stopped
            }
        }
        _ = shutdown_rx.recv() => ManagerSendOutcome::Stopped,
        result = manager_tx.send(command) => {
            if result.is_ok() {
                ManagerSendOutcome::Sent
            } else {
                ManagerSendOutcome::Stopped
            }
        }
    }
}

fn queue_web_seed_disconnect(
    manager_tx: Sender<TorrentCommand>,
    peer_id: String,
    scope_id: crate::networking::NetworkScopeId,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let command = TorrentCommand::WebSeedDisconnected { peer_id, scope_id };
    match manager_tx.try_send(command) {
        Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(command)) => {
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown_rx.recv() => {}
                    _ = manager_tx.send(command) => {}
                }
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn web_seed_worker(
    client: NetworkHttpClient,
    url: String,
    peer_id: String,
    piece_length: u64,
    total_size: u64,
    mut peer_rx: Receiver<TorrentCommand>,
    manager_tx: Sender<TorrentCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
    mut network_invalidation_rx: watch::Receiver<bool>,
    network_scope_id: crate::networking::NetworkScopeId,
) {
    if *network_invalidation_rx.borrow() {
        drop(client);
        queue_web_seed_disconnect(manager_tx, peer_id, network_scope_id, shutdown_rx);
        return;
    }

    // 1. Handshake sequence
    let num_pieces = total_size.div_ceil(piece_length);
    let bitfield_len = num_pieces.div_ceil(8);
    let full_bitfield = vec![255u8; bitfield_len as usize];
    for command in [
        TorrentCommand::SuccessfullyConnected(peer_id.clone()),
        TorrentCommand::PeerBitfield(peer_id.clone(), full_bitfield),
        TorrentCommand::Unchoke(peer_id.clone()),
    ] {
        match send_manager_command(
            &manager_tx,
            command,
            &mut shutdown_rx,
            &mut network_invalidation_rx,
        )
        .await
        {
            ManagerSendOutcome::Sent => {}
            ManagerSendOutcome::Invalidated => {
                drop(client);
                queue_web_seed_disconnect(manager_tx, peer_id, network_scope_id, shutdown_rx);
                return;
            }
            ManagerSendOutcome::Stopped => return,
        }
    }

    // 2. Main Command Loop
    let mut disconnect_registered_peer = false;
    'outer: loop {
        tokio::select! {
            biased;
            changed = network_invalidation_rx.changed() => {
                if changed.is_err() || *network_invalidation_rx.borrow() {
                    disconnect_registered_peer = true;
                    break 'outer;
                }
            }
            _ = shutdown_rx.recv() => {
                break 'outer;
            }
            cmd = peer_rx.recv() => {
                match cmd {
                    // FIX: Handle BulkRequest (Batch) instead of SendRequest
                    Some(TorrentCommand::BulkRequest(requests)) => {
                        for (index, begin, length) in requests {
                            if *network_invalidation_rx.borrow() {
                                disconnect_registered_peer = true;
                                break 'outer;
                            }
                            // Calculate absolute byte range for the HTTP request
                            let start = (index as u64 * piece_length) + begin as u64;
                            let end = start + length as u64 - 1;
                            let range_header = format!("bytes={}-{}", start, end);

                            // event!(Level::DEBUG, "WebSeed Request: {} range={}", url, range_header);

                            let request = match client.get(&url) {
                                Ok(request) => request.header(RANGE, range_header).send(),
                                Err(error) => {
                                    event!(Level::WARN, "WebSeed Request Blocked: {}", error);
                                    disconnect_registered_peer = true;
                                    break 'outer;
                                }
                            };

                            // Await the Response Header (cancellable)
                            let mut response = match tokio::select! {
                                biased;
                                changed = network_invalidation_rx.changed() => {
                                    if changed.is_err() || *network_invalidation_rx.borrow() {
                                        disconnect_registered_peer = true;
                                        break 'outer;
                                    }
                                    continue;
                                },
                                _ = shutdown_rx.recv() => break 'outer,
                                res = request => res,
                            } {
                                Ok(resp) if resp.status().is_success() => resp,
                                Ok(resp) => {
                                    event!(Level::WARN, "WebSeed Error {}: {}", resp.status(), url);
                                    disconnect_registered_peer = true;
                                    break 'outer;
                                }
                                Err(e) => {
                                    event!(Level::WARN, "WebSeed Connection Failed: {}", e);
                                    disconnect_registered_peer = true;
                                    break 'outer;
                                }
                            };

                            // 3. Stream the body
                            let mut buffer = Vec::with_capacity(length as usize);

                            loop {
                                let chunk_option = tokio::select! {
                                    biased;
                                    changed = network_invalidation_rx.changed() => {
                                        if changed.is_err() || *network_invalidation_rx.borrow() {
                                            disconnect_registered_peer = true;
                                            break 'outer;
                                        }
                                        continue;
                                    },
                                    _ = shutdown_rx.recv() => break 'outer,
                                    res = response.chunk() => res,
                                };

                                match chunk_option {
                                    Ok(Some(bytes)) => {
                                        buffer.extend_from_slice(&bytes);
                                    }
                                    Ok(None) => {
                                        // End of stream. Send the accumulated block.
                                        if !buffer.is_empty() {
                                            match send_manager_command(
                                                &manager_tx,
                                                TorrentCommand::Block(
                                                    peer_id.clone(),
                                                    index,
                                                    begin,
                                                    buffer,
                                                ),
                                                &mut shutdown_rx,
                                                &mut network_invalidation_rx,
                                            )
                                            .await
                                            {
                                                ManagerSendOutcome::Sent => {}
                                                ManagerSendOutcome::Invalidated => {
                                                    disconnect_registered_peer = true;
                                                    break 'outer;
                                                }
                                                ManagerSendOutcome::Stopped => break 'outer,
                                            }
                                        }
                                        break; // Finished this request, move to next in batch
                                    }
                                    Err(e) => {
                                        event!(Level::WARN, "WebSeed Stream Error: {}", e);
                                        disconnect_registered_peer = true;
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }

                    // FIX: Handle BulkCancel (No-op for HTTP usually, or close connection)
                    Some(TorrentCommand::BulkCancel(_)) => {
                        // HTTP requests are synchronous in this loop; we can't easily cancel
                        // one in the middle of a batch without dropping the connection.
                        // For now, we ignore it. The Manager will discard the data if we send it.
                    }

                    Some(TorrentCommand::Disconnect(_)) => break 'outer,
                    Some(_) => {}
                    None => break 'outer,
                }
            }
        }
    }

    drop(client);
    if disconnect_registered_peer {
        // A failed or invalidated worker must remove its registered pseudo-peer
        // so recovery can start a replacement instead of retaining a closed
        // peer channel in state.
        queue_web_seed_disconnect(manager_tx, peer_id, network_scope_id, shutdown_rx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio::time::timeout;

    fn test_http_client() -> NetworkHttpClient {
        let (_handle, lease) = crate::networking::runtime::test_network_lease();
        lease
            .web_seed_http_client()
            .expect("obtain test HTTP client")
    }

    #[tokio::test]
    async fn initially_invalid_generation_disconnects_without_starting_the_web_seed() {
        let (_peer_tx, peer_rx) = mpsc::channel(1);
        let (manager_tx, mut manager_rx) = mpsc::channel(2);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (_invalidation_tx, invalidation_rx) = watch::channel(true);
        let peer_id = "http://127.0.0.1/initially-invalid-seed".to_string();

        web_seed_worker(
            test_http_client(),
            peer_id.clone(),
            peer_id.clone(),
            1024,
            2048,
            peer_rx,
            manager_tx,
            shutdown_tx.subscribe(),
            invalidation_rx,
            crate::networking::NetworkScopeId::for_test(17),
        )
        .await;

        assert!(matches!(
            manager_rx.recv().await,
            Some(TorrentCommand::WebSeedDisconnected { peer_id: id, scope_id })
                if id == peer_id && scope_id == crate::networking::NetworkScopeId::for_test(17)
        ));
        assert!(manager_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn generation_invalidation_disconnects_the_registered_web_seed() {
        let (peer_tx, peer_rx) = mpsc::channel(1);
        let (manager_tx, mut manager_rx) = mpsc::channel(8);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (invalidation_tx, invalidation_rx) = watch::channel(false);
        let peer_id = "http://127.0.0.1/seed-data".to_string();

        let worker = tokio::spawn(web_seed_worker(
            test_http_client(),
            peer_id.clone(),
            peer_id.clone(),
            1024,
            2048,
            peer_rx,
            manager_tx,
            shutdown_tx.subscribe(),
            invalidation_rx,
            crate::networking::NetworkScopeId::for_test(23),
        ));

        assert!(matches!(
            manager_rx.recv().await,
            Some(TorrentCommand::SuccessfullyConnected(id)) if id == peer_id
        ));
        assert!(matches!(
            manager_rx.recv().await,
            Some(TorrentCommand::PeerBitfield(id, _)) if id == peer_id
        ));
        assert!(matches!(
            manager_rx.recv().await,
            Some(TorrentCommand::Unchoke(id)) if id == peer_id
        ));

        invalidation_tx
            .send(true)
            .expect("invalidate web-seed generation");
        assert!(matches!(
            timeout(Duration::from_millis(500), manager_rx.recv())
                .await
                .expect("worker should notify manager promptly"),
            Some(TorrentCommand::WebSeedDisconnected { peer_id: id, scope_id })
                if id == peer_id && scope_id == crate::networking::NetworkScopeId::for_test(23)
        ));
        timeout(Duration::from_millis(500), worker)
            .await
            .expect("worker should stop promptly")
            .expect("worker task");

        drop(peer_tx);
        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn pending_invalidation_wins_over_a_queued_web_seed_request() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local web-seed probe");
        let url = format!(
            "http://{}/seed-data",
            listener.local_addr().expect("read probe address")
        );
        let (peer_tx, peer_rx) = mpsc::channel(1);
        let (manager_tx, mut manager_rx) = mpsc::channel(8);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (invalidation_tx, invalidation_rx) = watch::channel(false);

        let worker = tokio::spawn(web_seed_worker(
            test_http_client(),
            url.clone(),
            url.clone(),
            1024,
            2048,
            peer_rx,
            manager_tx,
            shutdown_tx.subscribe(),
            invalidation_rx,
            crate::networking::NetworkScopeId::for_test(29),
        ));

        for _ in 0..3 {
            manager_rx.recv().await.expect("receive web-seed handshake");
        }
        peer_tx
            .send(TorrentCommand::BulkRequest(vec![(0, 0, 1)]))
            .await
            .expect("queue web-seed request");
        invalidation_tx.send_replace(true);

        assert!(matches!(
            timeout(Duration::from_millis(500), manager_rx.recv())
                .await
                .expect("worker should notify manager promptly"),
            Some(TorrentCommand::WebSeedDisconnected { peer_id, scope_id })
                if peer_id == url && scope_id == crate::networking::NetworkScopeId::for_test(29)
        ));
        timeout(Duration::from_millis(500), worker)
            .await
            .expect("worker should stop promptly")
            .expect("worker task");
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "invalidated worker must not start the queued HTTP request"
        );

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn completed_block_delivery_is_canceled_when_generation_expires() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::oneshot;

        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local web-seed probe");
        let url = format!(
            "http://{}/seed-block",
            listener.local_addr().expect("read probe address")
        );
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            let _ = request_seen_tx.send(());
            let _ = release_rx.await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")
                .await
                .expect("write response");
        });
        let (peer_tx, peer_rx) = mpsc::channel(1);
        let (manager_tx, mut manager_rx) = mpsc::channel(3);
        let manager_fill_tx = manager_tx.clone();
        let (shutdown_tx, _) = broadcast::channel(1);
        let (invalidation_tx, invalidation_rx) = watch::channel(false);
        let worker = tokio::spawn(web_seed_worker(
            test_http_client(),
            url.clone(),
            url.clone(),
            1024,
            2048,
            peer_rx,
            manager_tx,
            shutdown_tx.subscribe(),
            invalidation_rx,
            crate::networking::NetworkScopeId::for_test(31),
        ));

        for _ in 0..3 {
            manager_rx.recv().await.expect("receive web-seed handshake");
        }
        peer_tx
            .send(TorrentCommand::BulkRequest(vec![(0, 0, 1)]))
            .await
            .expect("queue web-seed request");
        request_seen_rx.await.expect("server saw web-seed request");
        for _ in 0..3 {
            manager_fill_tx
                .send(TorrentCommand::NotInterested)
                .await
                .expect("fill manager queue");
        }
        release_tx.send(()).expect("release web-seed response");
        tokio::time::sleep(Duration::from_millis(25)).await;
        invalidation_tx.send_replace(true);

        timeout(Duration::from_millis(500), worker)
            .await
            .expect("invalidated worker should not wait for manager capacity")
            .expect("worker task");
        for _ in 0..3 {
            assert!(matches!(
                manager_rx.recv().await,
                Some(TorrentCommand::NotInterested)
            ));
        }
        assert!(matches!(
            timeout(Duration::from_millis(500), manager_rx.recv())
                .await
                .expect("disconnect should be delivered after capacity returns"),
            Some(TorrentCommand::WebSeedDisconnected { peer_id, scope_id })
                if peer_id == url && scope_id == crate::networking::NetworkScopeId::for_test(31)
        ));
        assert!(manager_rx.try_recv().is_err());

        server.await.expect("web-seed probe task");
        let _ = shutdown_tx.send(());
    }
}
