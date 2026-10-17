// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use proptest::prelude::*;

const PIECES: usize = 33_080;
const PIECE_BYTES: u32 = 1024 * 1024;
const BLOCK_BYTES: u32 = 16_384;
const BLOCKS_PER_PIECE: usize = (PIECE_BYTES / BLOCK_BYTES) as usize;
const PEER: &str = "synthetic-refill-peer";

fn pending_fixture(pending_count: usize, endgame: bool, priorities: bool) -> TorrentState {
    let mut state = assignment_fixture(PIECES, PIECE_BYTES, pending_count, endgame);
    state.piece_manager.piece_rarity = (0..PIECES as u32).map(|p| (p, 10)).collect();
    state.piece_manager.piece_rarity.insert(10, 1);
    if priorities {
        let mut priority = vec![EffectivePiecePriority::Normal; PIECES];
        priority[20] = EffectivePiecePriority::High;
        state.piece_manager.apply_priorities(priority);
    }
    state
}

fn assignment_fixture(
    pieces: usize,
    piece_bytes: u32,
    pending_count: usize,
    endgame: bool,
) -> TorrentState {
    let mut state = super::tests::create_empty_state();
    let mut torrent = super::tests::create_dummy_torrent(pieces);
    torrent.info.name = "orbital-scheduling-payload".into();
    torrent.info.piece_length = piece_bytes as i64;
    torrent.info.length = pieces as i64 * piece_bytes as i64;
    state.torrent = Some(torrent);
    state.piece_manager.set_initial_fields(pieces, false);
    state.piece_manager.set_geometry(
        piece_bytes,
        pieces as u64 * piece_bytes as u64,
        HashMap::new(),
        false,
    );
    state.torrent_status = if endgame {
        TorrentStatus::Endgame
    } else {
        TorrentStatus::Standard
    };
    let (tx, _) = tokio::sync::mpsc::channel(1);
    let mut peer = PeerState::new(PEER.into(), tx, state.now);
    peer.bitfield = vec![true; pieces];
    peer.peer_choking = ChokeStatus::Unchoke;
    for piece in 0..pending_count as u32 {
        state.piece_manager.mark_as_pending(piece, PEER.into());
        peer.pending_requests.insert(piece);
    }
    state.peers.insert(PEER.into(), peer);
    state
}

fn assign(state: &mut TorrentState) -> (Vec<(u32, u32, u32)>, usize) {
    ASSIGN_WORK_CANDIDATE_VISITS.with(|visits| visits.set(0));
    ASSIGN_WORK_RARITY_LOOKUPS.with(|lookups| lookups.set(0));
    let effects = state.update(Action::AssignWork {
        peer_id: PEER.into(),
    });
    let visits = ASSIGN_WORK_CANDIDATE_VISITS.with(|visits| visits.get());
    let requests = effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::SendToPeer { peer_id, cmd } if peer_id == PEER => match *cmd {
                TorrentCommand::BulkRequest(batch) => Some(batch),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .collect();
    (requests, visits)
}

#[test]
fn full_pending_refill_avoids_candidate_work_and_preserves_requests() {
    assert_eq!(MAX_PIPELINE_DEPTH % BLOCKS_PER_PIECE, 0);
    let pending = MAX_PIPELINE_DEPTH / BLOCKS_PER_PIECE;
    for endgame in [false, true] {
        for priorities in [false, true] {
            let mut state = pending_fixture(pending, endgame, priorities);
            let needed = state.piece_manager.need_queue.clone();
            assert!(
                !needed.is_empty(),
                "fixture must have unused candidate pieces"
            );
            let (requests, visits) = assign(&mut state);
            let expected: HashSet<_> = (0..pending as u32)
                .flat_map(|p| {
                    (0..PIECE_BYTES)
                        .step_by(BLOCK_BYTES as usize)
                        .map(move |offset| (p, offset, BLOCK_BYTES))
                })
                .collect();
            assert_eq!(requests.len(), MAX_PIPELINE_DEPTH);
            assert_eq!(requests.iter().copied().collect::<HashSet<_>>(), expected);
            assert_eq!(state.peers[PEER].active_blocks, expected);
            assert_eq!(state.peers[PEER].inflight_requests, MAX_PIPELINE_DEPTH);
            assert_eq!(state.piece_manager.need_queue, needed);
            assert_eq!(visits, 0, "full pending refill must not scan new candidates: endgame={endgame}, priorities={priorities}");
        }
    }
}

#[test]
fn partial_pending_refill_still_selects_new_pieces_by_rarity_and_priority() {
    let pending = MAX_PIPELINE_DEPTH / BLOCKS_PER_PIECE - 1;
    for priorities in [false, true] {
        let mut state = pending_fixture(pending, false, priorities);
        let (requests, visits) = assign(&mut state);
        assert!(visits > 0, "spare capacity must reach candidate selection");
        if !priorities {
            let lookups = ASSIGN_WORK_RARITY_LOOKUPS.with(|lookups| lookups.get());
            assert!(lookups > 0 && lookups <= visits,
                "normal selection must read each candidate rarity at most once: {lookups} lookups for {visits} candidates");
        }
        let selected = if priorities { 20 } else { 10 };
        let expected: HashSet<_> = (0..pending as u32)
            .chain(std::iter::once(selected))
            .flat_map(|p| {
                (0..PIECE_BYTES)
                    .step_by(BLOCK_BYTES as usize)
                    .map(move |offset| (p, offset, BLOCK_BYTES))
            })
            .collect();
        assert_eq!(requests.len(), MAX_PIPELINE_DEPTH);
        assert_eq!(requests.iter().copied().collect::<HashSet<_>>(), expected);
        assert_eq!(state.peers[PEER].active_blocks, expected);
        assert_eq!(state.peers[PEER].inflight_requests, MAX_PIPELINE_DEPTH);
        assert!(state.peers[PEER].pending_requests.contains(&selected));
        assert!(!state.piece_manager.need_queue.contains(&selected));
    }
}

proptest! {
    #[test]
    fn prop_full_refill_preserves_work_without_scanning(
        block_power in 0u32..=6,
        extra_pieces in 1usize..256,
        large in prop_oneof![31 => Just(false), 1 => Just(true)],
        active_mask in proptest::collection::vec(any::<bool>(), MAX_PIPELINE_DEPTH),
        endgame in any::<bool>(),
        priorities in any::<bool>(),
    ) {
        let blocks_per_piece = 1usize << block_power;
        let pending = MAX_PIPELINE_DEPTH / blocks_per_piece;
        let pieces = if large { PIECES } else { pending + extra_pieces };
        let piece_bytes = blocks_per_piece as u32 * BLOCK_BYTES;
        let mut state = assignment_fixture(pieces, piece_bytes, pending, endgame);
        if priorities {
            state.piece_manager.apply_priorities(vec![EffectivePiecePriority::High; pieces]);
        }
        if endgame {
            // Endgame candidates belong to other owners. Populate the fixture
            // directly to avoid quadratic queue removal during large-case setup.
            let (tx, _) = tokio::sync::mpsc::channel(1);
            let other_id = "synthetic-other-peer".to_string();
            let mut other = PeerState::new(other_id.clone(), tx, state.now);
            other.bitfield = vec![true; pieces];
            for piece in state.piece_manager.need_queue.drain(..) {
                state.piece_manager.pending_queue.insert(piece, vec![other_id.clone()]);
                other.pending_requests.insert(piece);
            }
            state.peers.insert(other_id, other);
        }
        let all_blocks: HashSet<_> = (0..pending as u32)
            .flat_map(|piece| (0..piece_bytes).step_by(BLOCK_BYTES as usize)
                .map(move |offset| (piece, offset, BLOCK_BYTES)))
            .collect();
        let mut ordered_blocks: Vec<_> = all_blocks.iter().copied().collect();
        ordered_blocks.sort_unstable();
        // Leave at least one slot so the test reaches refill rather than the
        // earlier already-full-window return.
        let already_active: HashSet<_> = ordered_blocks.iter().enumerate()
            .filter(|(i, _)| *i + 1 < MAX_PIPELINE_DEPTH && active_mask[*i])
            .map(|(_, block)| *block).collect();
        let expected: HashSet<_> = all_blocks.difference(&already_active).copied().collect();
        let peer = state.peers.get_mut(PEER).unwrap();
        peer.active_blocks = already_active;
        peer.inflight_requests = peer.active_blocks.len();
        let need_before = state.piece_manager.need_queue.clone();
        let pending_before = state.piece_manager.pending_queue.clone();
        let (requests, visits) = assign(&mut state);
        prop_assert_eq!(requests.len(), expected.len());
        prop_assert_eq!(requests.into_iter().collect::<HashSet<_>>(), expected);
        prop_assert_eq!(&state.peers[PEER].active_blocks, &all_blocks);
        prop_assert_eq!(state.peers[PEER].inflight_requests, MAX_PIPELINE_DEPTH);
        prop_assert_eq!(&state.piece_manager.need_queue, &need_before);
        prop_assert_eq!(&state.piece_manager.pending_queue, &pending_before);
        prop_assert_eq!(visits, 0);
        prop_assert_eq!(ASSIGN_WORK_RARITY_LOOKUPS.with(|count| count.get()), 0);
    }

    #[test]
    fn prop_assignment_matches_original_stable_sort(
        entries in proptest::collection::vec(
            (proptest::option::of(0usize..8), any::<bool>(), 0u8..3, 0u8..3, any::<u16>()),
            2..128,
        ),
        inflight in 0usize..MAX_PIPELINE_DEPTH,
        large in prop_oneof![31 => Just(false), 1 => Just(true)],
    ) {
        let candidates = if large { PIECES } else { entries.len() };
        // Always exercise the cached-key path and the priority path for each case.
        for use_priorities in [false, true] {
            let mut state = assignment_fixture(inflight + candidates, BLOCK_BYTES, inflight, false);
            let mut priorities = vec![EffectivePiecePriority::Normal; inflight + candidates];
            for i in 0..candidates {
                let piece = (inflight + i) as u32;
                let (rarity, has_piece, busy, priority, _) = entries[i % entries.len()];
                // Guarantee two eligible tied candidates; generated entries add
                // missing rarity keys, unavailable/busy pieces and priority skips.
                let rarity = if i < 2 { entries[0].0 } else { rarity };
                if let Some(rarity) = rarity {
                    state.piece_manager.piece_rarity.insert(piece, rarity);
                }
                if i >= 2 {
                    state.peers.get_mut(PEER).unwrap().bitfield[piece as usize] = has_piece;
                    match busy {
                        1 => { state.verifying_pieces.insert(piece); }
                        2 => { state.writing_pieces.insert(piece); }
                        _ => {}
                    }
                }
                priorities[piece as usize] = if i < 2 { EffectivePiecePriority::High } else {
                    match priority {
                        0 => EffectivePiecePriority::Skip,
                        1 => EffectivePiecePriority::Normal,
                        _ => EffectivePiecePriority::High,
                    }
                };
            }
            if use_priorities {
                state.piece_manager.apply_priorities(priorities);
            }
            state.piece_manager.need_queue.sort_by_key(|piece| {
                entries[(*piece as usize - inflight) % entries.len()].4
            });
            let existing: HashSet<_> = (0..inflight as u32).map(|p| (p, 0, BLOCK_BYTES)).collect();
            state.peers.get_mut(PEER).unwrap().active_blocks = existing.clone();
            state.peers.get_mut(PEER).unwrap().inflight_requests = inflight;
            let need_before = state.piece_manager.need_queue.clone();
            let mut reference: Vec<_> = need_before.iter().copied().filter(|p| {
                state.peers[PEER].bitfield[*p as usize]
                    && !state.verifying_pieces.contains(p) && !state.writing_pieces.contains(p)
            }).collect();
            let candidate_count = reference.len();
            // Reference the original stable sort, including equal-key ordering
            // and the usize::MAX fallback for unknown rarity.
            reference.sort_by_key(|p| (
                std::cmp::Reverse(state.piece_manager.piece_priorities.get(*p as usize)
                    .copied().unwrap_or(EffectivePiecePriority::Normal)),
                state.piece_manager.piece_rarity.get(p).copied().unwrap_or(usize::MAX),
            ));
            reference.truncate(MAX_PIPELINE_DEPTH - inflight);
            let expected: Vec<_> = reference.iter().map(|p| (*p, 0, BLOCK_BYTES)).collect();
            let (requests, visits) = assign(&mut state);
            prop_assert_eq!(&requests, &expected);
            prop_assert_eq!(visits, need_before.len());
            let lookups = ASSIGN_WORK_RARITY_LOOKUPS.with(|count| count.get());
            if !use_priorities {
                prop_assert!(lookups > 0 && lookups <= candidate_count);
            }
            let expected_active: HashSet<_> = existing.into_iter().chain(expected).collect();
            prop_assert_eq!(&state.peers[PEER].active_blocks, &expected_active);
            prop_assert_eq!(state.peers[PEER].inflight_requests, expected_active.len());
            let selected: HashSet<_> = reference.into_iter().collect();
            let expected_need: Vec<_> = need_before.into_iter().filter(|p| !selected.contains(p)).collect();
            prop_assert_eq!(&state.piece_manager.need_queue, &expected_need);
            let expected_pending: HashSet<_> = (0..inflight as u32).chain(selected).collect();
            prop_assert_eq!(&state.peers[PEER].pending_requests, &expected_pending);
            prop_assert_eq!(state.piece_manager.pending_queue.keys().copied().collect::<HashSet<_>>(), expected_pending);
        }
    }
}
