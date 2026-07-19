// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::command::TorrentCommand;
use reqwest::header::RANGE;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tracing::{event, Level};

#[allow(clippy::too_many_arguments)]
pub async fn web_seed_worker(
    client: reqwest::Client,
    url: String,
    peer_id: String,
    piece_length: u64,
    total_size: u64,
    mut peer_rx: Receiver<TorrentCommand>,
    manager_tx: Sender<TorrentCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
    mut network_invalidation_rx: watch::Receiver<bool>,
) {
    // 1. Handshake sequence
    if manager_tx
        .send(TorrentCommand::SuccessfullyConnected(peer_id.clone()))
        .await
        .is_err()
    {
        return;
    }

    let num_pieces = total_size.div_ceil(piece_length);
    let bitfield_len = num_pieces.div_ceil(8);
    let full_bitfield = vec![255u8; bitfield_len as usize];

    if manager_tx
        .send(TorrentCommand::PeerBitfield(peer_id.clone(), full_bitfield))
        .await
        .is_err()
    {
        return;
    }

    if manager_tx
        .send(TorrentCommand::Unchoke(peer_id.clone()))
        .await
        .is_err()
    {
        return;
    }

    // 2. Main Command Loop
    'outer: loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                break 'outer;
            }
            changed = network_invalidation_rx.changed() => {
                if changed.is_err() || *network_invalidation_rx.borrow() {
                    break 'outer;
                }
            }
            cmd = peer_rx.recv() => {
                match cmd {
                    // FIX: Handle BulkRequest (Batch) instead of SendRequest
                    Some(TorrentCommand::BulkRequest(requests)) => {
                        for (index, begin, length) in requests {
                            // Calculate absolute byte range for the HTTP request
                            let start = (index as u64 * piece_length) + begin as u64;
                            let end = start + length as u64 - 1;
                            let range_header = format!("bytes={}-{}", start, end);

                            // event!(Level::DEBUG, "WebSeed Request: {} range={}", url, range_header);

                            let request = client.get(&url).header(RANGE, range_header).send();

                            // Await the Response Header (cancellable)
                            let mut response = match tokio::select! {
                                res = request => res,
                                _ = shutdown_rx.recv() => break 'outer,
                                changed = network_invalidation_rx.changed() => {
                                    if changed.is_err() || *network_invalidation_rx.borrow() {
                                        break 'outer;
                                    }
                                    continue;
                                },
                            } {
                                Ok(resp) if resp.status().is_success() => resp,
                                Ok(resp) => {
                                    event!(Level::WARN, "WebSeed Error {}: {}", resp.status(), url);
                                    let _ = manager_tx.send(TorrentCommand::Disconnect(peer_id)).await;
                                    break 'outer;
                                }
                                Err(e) => {
                                    event!(Level::WARN, "WebSeed Connection Failed: {}", e);
                                    let _ = manager_tx.send(TorrentCommand::Disconnect(peer_id)).await;
                                    break 'outer;
                                }
                            };

                            // 3. Stream the body
                            let mut buffer = Vec::with_capacity(length as usize);

                            loop {
                                let chunk_option = tokio::select! {
                                    res = response.chunk() => res,
                                    _ = shutdown_rx.recv() => break 'outer,
                                    changed = network_invalidation_rx.changed() => {
                                        if changed.is_err() || *network_invalidation_rx.borrow() {
                                            break 'outer;
                                        }
                                        continue;
                                    },
                                };

                                match chunk_option {
                                    Ok(Some(bytes)) => {
                                        buffer.extend_from_slice(&bytes);
                                    }
                                    Ok(None) => {
                                        // End of stream. Send the accumulated block.
                                                                                if !buffer.is_empty()
                                                                                    && manager_tx.send(TorrentCommand::Block(
                                                                                        peer_id.clone(),
                                                                                        index,
                                                                                        begin,
                                                                                        buffer,
                                                                                    ))
                                                                                    .await
                                                                                    .is_err()
                                                                                {
                                                                                    break 'outer;
                                                                                }
                                        break; // Finished this request, move to next in batch
                                    }
                                    Err(e) => {
                                        event!(Level::WARN, "WebSeed Stream Error: {}", e);
                                        let _ = manager_tx.send(TorrentCommand::Disconnect(peer_id)).await;
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
}
