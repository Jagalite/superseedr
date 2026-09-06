// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercise the real reducer and emitted peer requests across scheduling policies.
use super::tests::{create_dummy_torrent, create_empty_state};
use super::*;
use crate::config::DownloadMode;
use proptest::prelude::*;

fn state_with_pieces(count: usize, piece_length: u32) -> TorrentState {
    let mut state = create_empty_state();
    let mut torrent = create_dummy_torrent(count);
    torrent.info.piece_length = piece_length as i64;
    torrent.info.length = count as i64 * piece_length as i64;
    state.torrent = Some(torrent);
    state.piece_manager.set_initial_fields(count, false);
    state.piece_manager.set_geometry(
        piece_length,
        count as u64 * piece_length as u64,
        HashMap::new(),
        false,
    );
    state.torrent_status = TorrentStatus::Standard;
    state.update(Action::SetDownloadMode(DownloadMode::Sequential));
    state
}
fn peer(state: &mut TorrentState, id: &str, available: Vec<bool>) {
    let (tx, _) = tokio::sync::mpsc::channel(100);
    let mut peer = PeerState::new(id.into(), tx, state.now);
    peer.peer_id = id.as_bytes().to_vec();
    peer.bitfield = available;
    peer.peer_choking = ChokeStatus::Unchoke;
    state.peers.insert(id.into(), peer);
}
fn requests(effects: Vec<Effect>) -> Vec<(u32, u32, u32)> {
    effects
        .into_iter()
        .flat_map(|effect| match effect {
            Effect::SendToPeer { cmd, .. } => match *cmd {
                TorrentCommand::BulkRequest(requests) => requests,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        })
        .collect()
}
fn assign(state: &mut TorrentState, id: &str) -> Vec<(u32, u32, u32)> {
    requests(state.update(Action::AssignWork { peer_id: id.into() }))
}

#[test]
fn sequential_orders_available_pieces_instead_of_rarity() {
    let mut state = state_with_pieces(6, 16_384);
    state.piece_manager.need_queue = vec![5, 3, 1, 4, 0, 2];
    for piece in 0..6 {
        state
            .piece_manager
            .piece_rarity
            .insert(piece, 6 - piece as usize);
    }
    peer(
        &mut state,
        "ordered-peer",
        vec![true, false, true, true, true, true],
    );
    let batch = assign(&mut state, "ordered-peer");
    assert_eq!(
        batch.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![0, 2, 3, 4, 5]
    );
    assert!(assign(&mut state, "ordered-peer").is_empty());
}

#[test]
fn sequential_stalled_frontier_does_not_slide_on_later_completions() {
    let mut state = state_with_pieces(9, 4 * 1024 * 1024);
    for piece in 1..4 {
        state.piece_manager.mark_as_complete(piece);
    }
    peer(
        &mut state,
        "late-peer",
        (0..9).map(|piece| piece >= 4).collect(),
    );
    assert!(assign(&mut state, "late-peer").is_empty());
    assert!(
        state.peers["late-peer"].am_interested,
        "later-only peers remain interested for future progress"
    );
    state.verifying_pieces.insert(0);
    state.update(Action::PieceVerified {
        peer_id: "source".into(),
        piece_index: 0,
        valid: true,
        data: Vec::new(),
    });
    assert!(
        assign(&mut state, "late-peer").is_empty(),
        "hash success alone cannot advance readable progress"
    );
    let batch = requests(state.update(Action::PieceWrittenToDisk {
        peer_id: "source".into(),
        piece_index: 0,
    }));
    assert_eq!(batch.first().map(|r| r.0), Some(4));
}

#[test]
fn sequential_priorities_and_skips_define_the_selected_order() {
    let mut state = state_with_pieces(8, 4 * 1024 * 1024);
    state.piece_manager.apply_priorities(vec![
        EffectivePiecePriority::Normal,
        EffectivePiecePriority::Skip,
        EffectivePiecePriority::Normal,
        EffectivePiecePriority::Normal,
        EffectivePiecePriority::High,
        EffectivePiecePriority::High,
        EffectivePiecePriority::High,
        EffectivePiecePriority::High,
    ]);
    peer(
        &mut state,
        "normal-peer",
        vec![true, true, true, true, false, false, false, false],
    );
    assert!(assign(&mut state, "normal-peer").is_empty());
    peer(&mut state, "high-peer", vec![true; 8]);
    let batch = assign(&mut state, "high-peer");
    assert_eq!(batch.first().map(|r| r.0), Some(4));
    assert!(batch.iter().all(|r| (4..8).contains(&r.0)));
    // A skipped gap consumes no window position, and unskipping it moves the
    // derived frontier back without losing completed data.
    for piece in 4..8 {
        state.piece_manager.mark_as_complete(piece);
    }
    let batch = assign(&mut state, "normal-peer");
    assert_eq!(batch.first().map(|r| r.0), Some(0));
    assert!(batch.iter().all(|r| r.0 != 1));
}

#[test]
fn sequential_switch_preserves_inflight_and_reorders_pending_gaps() {
    let mut state = state_with_pieces(6, 32_768);
    state.download_mode = DownloadMode::RarestFirst;
    peer(&mut state, "switch-peer", vec![true; 6]);
    state.piece_manager.mark_as_pending(4, "switch-peer".into());
    let peer = state.peers.get_mut("switch-peer").unwrap();
    peer.pending_requests.insert(4);
    peer.active_blocks.insert((4, 0, 16_384));
    peer.inflight_requests = 1;
    let batch = requests(state.update(Action::SetDownloadMode(DownloadMode::Sequential)));
    assert_eq!(batch.first().map(|r| r.0), Some(0));
    assert!(!batch.contains(&(4, 0, 16_384)));
    assert!(batch.contains(&(4, 16_384, 16_384)));
    assert_eq!(state.piece_manager.pending_queue[&4], vec!["switch-peer"]);
    assert_eq!(
        batch.len() + 1,
        state.peers["switch-peer"].inflight_requests
    );
}

#[test]
fn sequential_mode_switch_does_not_extend_old_work_outside_window() {
    let mut state = state_with_pieces(10, 4 * 1024 * 1024);
    state.download_mode = DownloadMode::RarestFirst;
    peer(&mut state, "switch-peer", vec![true; 10]);
    state.piece_manager.mark_as_pending(9, "switch-peer".into());
    let peer = state.peers.get_mut("switch-peer").unwrap();
    peer.pending_requests.insert(9);
    peer.active_blocks.insert((9, 0, 16_384));
    peer.inflight_requests = 1;
    let batch = requests(state.update(Action::SetDownloadMode(DownloadMode::Sequential)));
    assert!(batch.iter().all(|r| r.0 < 4));
    assert!(state.peers["switch-peer"]
        .active_blocks
        .contains(&(9, 0, 16_384)));
    assert_eq!(
        state.peers["switch-peer"].inflight_requests,
        MAX_PIPELINE_DEPTH
    );
}

#[test]
fn sequential_retry_and_endgame_retain_existing_ownership_rules() {
    let mut state = state_with_pieces(3, 16_384);
    peer(&mut state, "slow-peer", vec![true; 3]);
    peer(&mut state, "fast-peer", vec![true; 3]);
    assert_eq!(assign(&mut state, "slow-peer").len(), 3);
    assert_eq!(state.torrent_status, TorrentStatus::Endgame);
    let raced = assign(&mut state, "fast-peer");
    assert_eq!(raced.iter().map(|r| r.0).collect::<Vec<_>>(), vec![0, 1, 2]);
    state.update(Action::PeerChoked {
        peer_id: "slow-peer".into(),
    });
    assert_eq!(state.piece_manager.pending_queue[&0], vec!["fast-peer"]);
    assert!(state.piece_manager.need_queue.is_empty());
    let effects = state.update(Action::PieceVerified {
        peer_id: "fast-peer".into(),
        piece_index: 0,
        valid: false,
        data: Vec::new(),
    });
    // Hash failure asks the existing peer lifecycle to disconnect the source;
    // pending ownership is released only when that observation returns.
    assert!(effects.iter().any(
        |effect| matches!(effect, Effect::DisconnectPeer { peer_id } if peer_id == "fast-peer")
    ));
    state.update(Action::PeerDisconnected {
        peer_id: "fast-peer".into(),
        force: true,
    });
    assert!(state.piece_manager.need_queue.contains(&0));
    assert!(state.piece_manager.sequential_window(16_384).contains(&0));
}

#[test]
fn sequential_pause_validation_and_mode_toggle_do_not_bypass_gates() {
    let mut state = state_with_pieces(5, 4 * 1024 * 1024);
    peer(
        &mut state,
        "tail-peer",
        vec![false, false, false, false, true],
    );
    assert!(assign(&mut state, "tail-peer").is_empty());
    state.is_paused = true;
    assert!(requests(state.update(Action::SetDownloadMode(DownloadMode::RarestFirst))).is_empty());
    state.is_paused = false;
    state.torrent_status = TorrentStatus::Validating;
    assert!(assign(&mut state, "tail-peer").is_empty());
    state.torrent_status = TorrentStatus::Standard;
    assert_eq!(
        assign(&mut state, "tail-peer").first().map(|r| r.0),
        Some(4)
    );
}

#[test]
fn sequential_keeps_short_and_non_aligned_requests_inside_piece_geometry() {
    let mut state = state_with_pieces(3, 20_000);
    state
        .piece_manager
        .set_geometry(20_000, 43_000, HashMap::new(), false);
    state.torrent.as_mut().unwrap().info.length = 43_000;
    peer(&mut state, "geometry-peer", vec![true; 3]);
    assert_eq!(
        assign(&mut state, "geometry-peer"),
        vec![
            (0, 0, 16_384),
            (0, 16_384, 3_616),
            (1, 0, 16_384),
            (1, 16_384, 3_616),
            (2, 0, 3_000),
        ]
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn sequential_requests_stay_bounded_and_available(
        done in prop::collection::vec(any::<bool>(), 12),
        available in prop::collection::vec(any::<bool>(), 12),
    ) {
        let mut state = state_with_pieces(12, 4 * 1024 * 1024);
        for (index, complete) in done.iter().enumerate() { if *complete { state.piece_manager.mark_as_complete(index as u32); } }
        peer(&mut state, "property-peer", available.clone());
        let frontier = done.iter().position(|complete| !complete).unwrap_or(done.len());
        let batch = assign(&mut state, "property-peer");
        prop_assert!(batch.len() <= MAX_PIPELINE_DEPTH);
        let mut unique = HashSet::new();
        for request in batch {
            let piece = request.0 as usize;
            prop_assert!(piece >= frontier && piece < frontier + 4);
            prop_assert!(available[piece] && !done[piece]);
            prop_assert!(unique.insert(request));
        }
    }
}

#[test]
fn sequential_window_bounds_tiny_and_oversized_pieces() {
    let mut pieces = PieceManager::new();
    pieces.set_initial_fields(4096, false);
    assert_eq!(pieces.sequential_window(1).len(), 1024);
    assert_eq!(pieces.sequential_window(u64::MAX), HashSet::from([0]));
}

#[test]
fn sequential_v2_requests_respect_the_file_tail_limit() {
    let mut state = state_with_pieces(2, 32_768);
    state.piece_to_roots.insert(
        1,
        vec![V2RootInfo {
            file_offset: 32_768,
            length: 1234,
            root_hash: vec![0; 32],
            file_index: 1,
        }],
    );
    peer(&mut state, "tail-peer", vec![true; 2]);
    let batch = assign(&mut state, "tail-peer");
    assert_eq!(
        batch,
        vec![(0, 0, 16_384), (0, 16_384, 16_384), (1, 0, 1234)]
    );
}
