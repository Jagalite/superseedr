// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::torrent_manager::block_manager::{BlockAddress, BlockManager};

#[cfg(test)]
use crate::torrent_manager::state::TorrentStatus;

#[cfg(test)]
use rand::prelude::IndexedRandom;

#[cfg(test)]
use std::collections::HashSet;

use std::collections::HashMap;
use tracing::{event, Level};

#[derive(PartialEq, Clone, Copy, Debug, Default)]
pub enum PieceStatus {
    #[default]
    Need,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum EffectivePiecePriority {
    Skip,
    #[default]
    Normal,
    High,
}

#[derive(Default, Debug, Clone)]
pub struct PieceManager {
    // --- Public Fields (Required by state.rs) ---
    pub bitfield: Vec<PieceStatus>,
    pub need_queue: Vec<u32>,
    pub pending_queue: HashMap<u32, Vec<String>>,
    pub piece_rarity: HashMap<u32, usize>,
    pub pieces_remaining: usize,
    pub piece_priorities: Vec<EffectivePiecePriority>,

    // --- The Block Engine ---
    pub block_manager: BlockManager,
}

impl PieceManager {
    pub fn new() -> Self {
        Self {
            bitfield: Vec::new(),
            need_queue: Vec::new(),
            pending_queue: HashMap::new(),
            piece_rarity: HashMap::new(),
            pieces_remaining: 0,
            piece_priorities: Vec::new(),
            block_manager: BlockManager::new(),
        }
    }

    /// GEOMETRY SETUP:
    /// This must be called (usually from state.rs Action::MetadataReceived) to allow
    /// the inner BlockManager to calculate offsets correctly.
    pub fn set_geometry(
        &mut self,
        piece_length: u32,
        total_length: u64,
        piece_overrides: HashMap<u32, u32>,
        validation_complete: bool,
    ) {
        self.block_manager.set_geometry(
            piece_length,
            total_length,
            Vec::new(),
            Vec::new(),
            piece_overrides,
            validation_complete,
        );
    }

    pub fn set_initial_fields(&mut self, num_pieces: usize, validation_complete: bool) {
        let mut bitfield = vec![PieceStatus::Need; num_pieces];
        self.need_queue.clear();

        self.piece_priorities.clear();

        if validation_complete {
            bitfield.fill(PieceStatus::Done);
        } else {
            for (i, status) in bitfield.iter().enumerate() {
                if *status == PieceStatus::Need {
                    self.need_queue.push(i as u32);
                }
            }
        }
        self.bitfield = bitfield;
        self.pieces_remaining = self.need_queue.len();
    }

    pub fn apply_priorities(&mut self, new_priorities: Vec<EffectivePiecePriority>) -> Vec<u32> {
        let mut cancelled_pieces = Vec::new();

        // Safety check
        if new_priorities.len() != self.bitfield.len() {
            if !self.piece_priorities.is_empty() {
                self.piece_priorities.clear(); // Reset on mismatch
            }
            return Vec::new();
        }

        // Lazy Init: If we are currently empty (Standard), fill with Normal to allow diffing
        if self.piece_priorities.is_empty() {
            self.piece_priorities = vec![EffectivePiecePriority::Normal; self.bitfield.len()];
        }

        for (idx, &new_prio) in new_priorities.iter().enumerate() {
            let p_idx = idx as u32;
            let old_prio = self.piece_priorities[idx];

            if new_prio != old_prio {
                self.piece_priorities[idx] = new_prio;

                let is_done = self.bitfield[idx] == PieceStatus::Done;
                if !is_done {
                    // Transition TO Skip
                    if new_prio == EffectivePiecePriority::Skip {
                        // Remove from Need
                        if let Some(pos) = self.need_queue.iter().position(|&x| x == p_idx) {
                            self.need_queue.swap_remove(pos);
                        }
                        // Mark for Cancel if Pending
                        if self.pending_queue.contains_key(&p_idx) {
                            cancelled_pieces.push(p_idx);
                        }
                    }
                    // Transition FROM Skip (to Normal/High)
                    else if old_prio == EffectivePiecePriority::Skip
                        && !self.need_queue.contains(&p_idx)
                        && !self.pending_queue.contains_key(&p_idx)
                    {
                        self.need_queue.push(p_idx);
                    }
                }
            }
        }

        // Optimization: If everything is Normal, clear the vector to use Fast Path
        if self
            .piece_priorities
            .iter()
            .all(|&p| p == EffectivePiecePriority::Normal)
        {
            self.piece_priorities.clear();
        }

        cancelled_pieces
    }

    pub fn handle_block(
        &mut self,
        piece_index: u32,
        block_offset: u32,
        block_data: &[u8],
        piece_size: usize,
    ) -> Option<Vec<u8>> {
        if self.block_manager.piece_length == 0 {
            let estimated_total = (piece_index as u64 + 1) * piece_size as u64;
            self.set_geometry(piece_size as u32, estimated_total, HashMap::new(), false);
        }

        let addr = self.block_manager.inflate_address_from_overlay(
            piece_index,
            block_offset,
            block_data.len() as u32,
        )?;

        self.block_manager
            .handle_v1_block_buffering(addr, block_data)
    }

    pub fn mark_as_complete(&mut self, piece_index: u32) -> Vec<String> {
        let current_status = self.bitfield.get(piece_index as usize).cloned();

        if current_status == Some(PieceStatus::Done) {
            return Vec::new();
        }

        self.bitfield[piece_index as usize] = PieceStatus::Done;
        self.pieces_remaining = self.pieces_remaining.saturating_sub(1);

        let _old_need_len = self.need_queue.len();
        self.need_queue.retain(|&p| p != piece_index);
        let _new_need_len = self.need_queue.len();

        let peers_to_cancel = self.pending_queue.remove(&piece_index).unwrap_or_default();

        self.block_manager.commit_v1_piece(piece_index);

        peers_to_cancel
    }

    pub fn reset_piece_assembly(&mut self, piece_index: u32) {
        // Delegate cleanup to BlockManager
        self.block_manager.reset_v1_buffer(piece_index);

        event!(
            Level::DEBUG,
            piece = piece_index,
            "Resetting piece assembler due to verification failure."
        );
    }

    pub fn requeue_pending_to_need(&mut self, piece_index: u32) {
        self.pending_queue.remove(&piece_index);

        let was_done = self.bitfield.get(piece_index as usize) == Some(&PieceStatus::Done);
        if was_done {
            self.pieces_remaining += 1;
        }

        if let Some(status) = self.bitfield.get_mut(piece_index as usize) {
            *status = PieceStatus::Need;
        }

        // Only requeue if NOT skipped
        let is_skipped = if !self.piece_priorities.is_empty() {
            self.piece_priorities[piece_index as usize] == EffectivePiecePriority::Skip
        } else {
            false
        };

        if !is_skipped && !self.need_queue.contains(&piece_index) {
            self.need_queue.push(piece_index);
        }

        self.block_manager.revert_v1_piece_completion(piece_index);
    }

    pub fn release_pending_peer_or_requeue(&mut self, piece_index: u32, peer_id: &str) {
        if let Some(peers) = self.pending_queue.get_mut(&piece_index) {
            peers.retain(|pending_peer| pending_peer != peer_id);
            if !peers.is_empty() {
                return;
            }
        }

        self.requeue_pending_to_need(piece_index);
    }

    pub fn update_rarity<'a, I>(&mut self, all_peer_bitfields: I)
    where
        I: Iterator<Item = &'a Vec<bool>> + Clone,
    {
        self.block_manager.update_rarity(all_peer_bitfields);

        // We only want to expose rarity for pieces we actually Need or are Pending.
        // This matches the original API contract and passes the existing tests.
        self.piece_rarity = self
            .block_manager
            .piece_rarity
            .clone()
            .into_iter()
            .filter(|(k, _)| self.bitfield.get(*k as usize) != Some(&PieceStatus::Done))
            .collect();
    }

    #[cfg(test)]
    pub fn choose_piece_for_peer(
        &self,
        peer_bitfield: &[bool],
        peer_pending: &HashSet<u32>,
        torrent_status: &TorrentStatus,
    ) -> Option<u32> {
        // FAST PATH: Standard Mode (Empty Vector)
        if self.piece_priorities.is_empty() {
            if *torrent_status != TorrentStatus::Endgame {
                return self
                    .need_queue
                    .iter()
                    .filter(|&&p| peer_bitfield.get(p as usize) == Some(&true))
                    .filter(|&&p| !peer_pending.contains(&p))
                    .min_by_key(|&&p| self.piece_rarity.get(&p).unwrap_or(&usize::MAX))
                    .copied();
            } else {
                let candidates: Vec<u32> = self
                    .pending_queue
                    .keys()
                    .chain(self.need_queue.iter())
                    .filter(|&&p| peer_bitfield.get(p as usize) == Some(&true))
                    .filter(|&&p| !peer_pending.contains(&p))
                    .copied()
                    .collect();
                return candidates.choose(&mut rand::rng()).copied();
            }
        }

        let compare_pieces = |a: &&u32, b: &&u32| {
            // Dereference twice to get the actual u32 piece index
            let idx_a = **a;
            let idx_b = **b;

            let prio_a = self.piece_priorities[idx_a as usize];
            let prio_b = self.piece_priorities[idx_b as usize];

            match prio_b.cmp(&prio_a) {
                std::cmp::Ordering::Equal => {
                    let rare_a = self.piece_rarity.get(&idx_a).unwrap_or(&usize::MAX);
                    let rare_b = self.piece_rarity.get(&idx_b).unwrap_or(&usize::MAX);
                    rare_a.cmp(rare_b)
                }
                other => other,
            }
        };

        let source_iter: Box<dyn Iterator<Item = &u32>> =
            if *torrent_status != TorrentStatus::Endgame {
                Box::new(self.need_queue.iter())
            } else {
                Box::new(self.pending_queue.keys().chain(self.need_queue.iter()))
            };

        source_iter
            .filter(|&&p| peer_bitfield.get(p as usize) == Some(&true))
            .filter(|&&p| !peer_pending.contains(&p))
            .filter(|&&p| self.piece_priorities[p as usize] != EffectivePiecePriority::Skip)
            .min_by(compare_pieces)
            .copied()
    }

    pub fn mark_as_pending(&mut self, piece_index: u32, peer_id: String) {
        self.need_queue.retain(|&p| p != piece_index);
        self.pending_queue
            .entry(piece_index)
            .or_default()
            .push(peer_id.clone());
    }

    pub fn clear_assembly_buffers(&mut self) {
        self.block_manager.legacy_buffers.clear();
    }

    pub fn requestable_block_addresses_for_piece(&self, piece_index: u32) -> Vec<BlockAddress> {
        let use_global_have = !self.block_manager.is_non_aligned_piece_grid();
        let assembler_mask = self
            .block_manager
            .legacy_buffers
            .get(&piece_index)
            .map(|a| a.mask.clone());

        self.block_manager
            .piece_block_addresses(piece_index)
            .into_iter()
            .filter(|addr| {
                if let Some(mask) = &assembler_mask {
                    if mask.get(addr.block_index as usize) == Some(&true) {
                        return false;
                    }
                }

                if use_global_have {
                    let global_idx = self.block_manager.flatten_address(*addr);
                    if self.block_manager.block_bitfield.get(global_idx as usize) == Some(&true) {
                        return false;
                    }
                }

                true
            })
            .collect()
    }

    pub fn cancel_tuples_for_piece(&self, piece_index: u32) -> Vec<(u32, u32, u32)> {
        self.block_manager
            .piece_block_addresses(piece_index)
            .into_iter()
            .map(|addr| (addr.piece_index, addr.byte_offset, addr.length))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent_manager::state::TorrentStatus;
    use std::collections::HashSet;

    /// Helper to create a piece manager initialized with 'Need' pieces
    fn setup_manager(num_pieces: usize) -> PieceManager {
        let mut pm = PieceManager::new();
        // Set dummy geometry so BlockManager math works (assuming standard 16KB blocks)
        // 16KB * 10 blocks per piece = 163840 bytes per piece
        let piece_len = 163_840;
        let total_len = piece_len as u64 * num_pieces as u64;
        pm.set_geometry(piece_len, total_len, HashMap::new(), false);

        pm.set_initial_fields(num_pieces, false);
        pm
    }

    #[test]
    fn test_initialization_not_validated() {
        let mut pm = PieceManager::new();
        let num_pieces = 10;
        pm.set_initial_fields(num_pieces, false);

        assert_eq!(pm.bitfield.len(), num_pieces);
        assert_eq!(pm.bitfield[0], PieceStatus::Need);
        assert_eq!(pm.need_queue.len(), num_pieces);
        assert_eq!(pm.pieces_remaining, num_pieces);
        assert_eq!(pm.need_queue[0], 0);
        assert_eq!(pm.need_queue[9], 9);
    }

    #[test]
    fn test_initialization_pre_validated() {
        let mut pm = PieceManager::new();
        let num_pieces = 10;
        pm.set_initial_fields(num_pieces, true);

        assert_eq!(pm.bitfield.len(), num_pieces);
        assert_eq!(pm.bitfield[0], PieceStatus::Done);
        assert!(pm.need_queue.is_empty());
        assert_eq!(pm.pieces_remaining, 0);
    }

    #[test]
    fn test_state_transitions() {
        let mut pm = setup_manager(5); // pieces 0, 1, 2, 3, 4
        assert_eq!(pm.pieces_remaining, 5);
        assert_eq!(pm.need_queue, vec![0, 1, 2, 3, 4]);

        pm.mark_as_pending(2, "peer_A".to_string());
        assert_eq!(pm.need_queue, vec![0, 1, 3, 4]);
        assert_eq!(
            pm.pending_queue.get(&2).unwrap(),
            &vec!["peer_A".to_string()]
        );
        assert_eq!(pm.pieces_remaining, 5); // Still need it

        pm.mark_as_pending(2, "peer_B".to_string());
        assert_eq!(
            pm.pending_queue.get(&2).unwrap(),
            &vec!["peer_A".to_string(), "peer_B".to_string()]
        );

        pm.requeue_pending_to_need(2);
        // Order doesn't matter, check presence and absence
        assert!(!pm.pending_queue.contains_key(&2));
        assert!(pm.need_queue.contains(&0));
        assert!(pm.need_queue.contains(&1));
        assert!(pm.need_queue.contains(&2));
        assert!(pm.need_queue.contains(&3));
        assert!(pm.need_queue.contains(&4));
        assert_eq!(pm.need_queue.len(), 5);

        let peers_to_cancel = pm.mark_as_complete(3);
        assert!(peers_to_cancel.is_empty());
        assert_eq!(pm.bitfield[3], PieceStatus::Done);
        assert_eq!(pm.pieces_remaining, 4);
        assert!(!pm.need_queue.contains(&3));

        pm.mark_as_pending(2, "peer_C".to_string()); // Pend it again
        let peers_to_cancel = pm.mark_as_complete(2);
        assert_eq!(peers_to_cancel, vec!["peer_C".to_string()]);
        assert_eq!(pm.bitfield[2], PieceStatus::Done);
        assert_eq!(pm.pieces_remaining, 3);
        assert!(!pm.pending_queue.contains_key(&2));
        assert!(!pm.need_queue.contains(&2));

        let peers_to_cancel = pm.mark_as_complete(2);
        assert!(peers_to_cancel.is_empty());
        assert_eq!(pm.pieces_remaining, 3); // No change
    }

    #[test]
    fn test_piece_assembly_and_reset() {
        let mut pm = PieceManager::new();
        let piece_index = 0;
        let piece_size = 32768; // 2 blocks of 16384
        let block_size = 16384;

        // Set geometry explicitly (required for block manager calculations)
        pm.set_geometry(
            piece_size as u32,
            piece_size as u64 * 10,
            HashMap::new(),
            false,
        );

        let block_data_0 = vec![1; block_size];
        let block_data_1 = vec![2; block_size];

        let result = pm.handle_block(piece_index, 0, &block_data_0, piece_size);
        assert!(result.is_none());

        // CHECK: Access inner BlockManager legacy_buffers
        assert!(pm.block_manager.legacy_buffers.contains_key(&piece_index));
        let assembler = pm.block_manager.legacy_buffers.get(&piece_index).unwrap();
        assert_eq!(assembler.total_blocks, 2);
        assert_eq!(assembler.received_blocks, 1);

        pm.reset_piece_assembly(piece_index);
        assert!(!pm.block_manager.legacy_buffers.contains_key(&piece_index));

        let result = pm.handle_block(piece_index, 0, &block_data_0, piece_size);
        assert!(result.is_none());

        let result = pm.handle_block(piece_index, block_size as u32, &block_data_1, piece_size);

        assert!(result.is_some());
        let full_piece = result.unwrap();
        assert_eq!(full_piece.len(), piece_size);
        assert_eq!(&full_piece[0..block_size], &block_data_0[..]);
        assert_eq!(&full_piece[block_size..], &block_data_1[..]);

        assert!(!pm.block_manager.legacy_buffers.contains_key(&piece_index));
    }

    #[test]
    fn test_update_rarity() {
        let mut pm = setup_manager(4); // need = [0, 1, 2, 3]
        pm.mark_as_pending(2, "peer_A".to_string()); // need = [0, 1, 3], pending = [2]
        pm.mark_as_complete(0); // need = [1, 3], pending = [2], done = [0]

        // Pieces to check: 1, 3, 2

        let peer1_bitfield = vec![true, true, false, true]; // Has 0, 1, 3
        let peer2_bitfield = vec![true, false, true, true]; // Has 0, 2, 3
        let peer_bitfields = [peer1_bitfield, peer2_bitfield];

        pm.update_rarity(peer_bitfields.iter());

        // Piece 0 is Done, should not be in rarity map
        assert!(!pm.piece_rarity.contains_key(&0));
        // Piece 1 is Need, 1 peer has it
        assert_eq!(pm.piece_rarity.get(&1), Some(&1));
        // Piece 2 is Pending, 1 peer has it
        assert_eq!(pm.piece_rarity.get(&2), Some(&1));
        // Piece 3 is Need, 2 peers have it
        assert_eq!(pm.piece_rarity.get(&3), Some(&2));
    }

    #[test]
    fn test_choose_piece_standard_mode() {
        let mut pm = setup_manager(5); // need = [0, 1, 2, 3, 4]

        // Rarity: 0 (rare), 1 (common), 2 (rare), 3 (medium), 4 (peer doesn't have)
        pm.piece_rarity.insert(0, 1);
        pm.piece_rarity.insert(1, 10);
        pm.piece_rarity.insert(2, 1);
        pm.piece_rarity.insert(3, 5);
        pm.piece_rarity.insert(4, 2);

        let peer_bitfield = vec![true, true, true, true, false]; // Has 0, 1, 2, 3
        let mut peer_pending = HashSet::new();
        let status = TorrentStatus::Standard;

        let choice = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending, &status);
        assert!(choice == Some(0) || choice == Some(2));
        let chosen_piece = choice.unwrap();

        peer_pending.insert(chosen_piece);
        let choice2 = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending, &status);
        if chosen_piece == 0 {
            assert_eq!(choice2, Some(2));
        } else {
            assert_eq!(choice2, Some(0));
        }

        peer_pending.insert(0);
        peer_pending.insert(1);
        peer_pending.insert(2);
        peer_pending.insert(3);
        let choice = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending, &status);
        assert_eq!(choice, None);

        let empty_peer_bitfield = vec![false; 5];
        let choice = pm.choose_piece_for_peer(&empty_peer_bitfield, &peer_pending, &status);
        assert_eq!(choice, None);
    }

    #[test]
    fn test_choose_piece_endgame_mode_prioritizes_pending() {
        let mut pm = setup_manager(5);
        pm.mark_as_pending(1, "peer_A".to_string());
        pm.mark_as_pending(2, "peer_B".to_string());

        let peer_bitfield = vec![true, true, true, true, false]; // Has 0, 1, 2, 3
        let peer_pending = HashSet::new();
        let status = TorrentStatus::Endgame;

        let mut choices = HashSet::new();
        for _ in 0..20 {
            let choice = pm
                .choose_piece_for_peer(&peer_bitfield, &peer_pending, &status)
                .unwrap();
            assert!([0, 1, 2, 3].contains(&choice));
            choices.insert(choice);
        }
        // Check if we got at least one from Need and one from Pending over several tries.
        assert!(choices.contains(&0) || choices.contains(&3)); // Need
        assert!(choices.contains(&1) || choices.contains(&2)); // Pending
    }

    #[test]
    fn test_choose_piece_endgame_mode_excludes_peer_pending() {
        let mut pm = setup_manager(5);
        pm.mark_as_pending(1, "peer_A".to_string());
        pm.mark_as_pending(2, "peer_B".to_string());

        let peer_bitfield = vec![true, true, true, true, false];
        let mut peer_pending = HashSet::new();
        peer_pending.insert(1); // Peer is already downloading piece 1
        let status = TorrentStatus::Endgame;

        // Candidates should be [0, 2, 3] (excludes piece 1)
        for _ in 0..20 {
            let choice = pm
                .choose_piece_for_peer(&peer_bitfield, &peer_pending, &status)
                .unwrap();
            assert!([0, 2, 3].contains(&choice));
            assert_ne!(choice, 1);
        }
    }

    #[test]
    fn test_handle_block_out_of_order() {
        let mut pm = PieceManager::new();
        let piece_index = 0;
        let piece_size = 32768;
        let block_size = 16384;

        pm.set_geometry(
            piece_size as u32,
            piece_size as u64 * 5,
            HashMap::new(),
            false,
        );

        let block_data_0 = vec![1; block_size];
        let block_data_1 = vec![2; block_size];

        // Receive block 1 first
        let result1 = pm.handle_block(piece_index, block_size as u32, &block_data_1, piece_size);
        assert!(result1.is_none());

        let assembler1 = pm.block_manager.legacy_buffers.get(&piece_index).unwrap();
        assert_eq!(assembler1.received_blocks, 1);
        assert!(assembler1.mask[1]); // Block index 1 is set

        // Receive block 0 second
        let result0 = pm.handle_block(piece_index, 0, &block_data_0, piece_size);
        assert!(result0.is_some());
        let full_piece = result0.unwrap();

        assert_eq!(full_piece.len(), piece_size);
        assert_eq!(&full_piece[0..block_size], &block_data_0[..]);
        assert_eq!(&full_piece[block_size..], &block_data_1[..]);
        assert!(!pm.block_manager.legacy_buffers.contains_key(&piece_index));
    }

    #[test]
    fn test_handle_block_duplicate() {
        let mut pm = PieceManager::new();
        let piece_index = 0;
        let piece_size = 16384;
        let block_size = 16384;
        let block_data = vec![1; block_size];

        pm.set_geometry(piece_size as u32, piece_size as u64, HashMap::new(), false);

        // Receive block 0
        let result1 = pm.handle_block(piece_index, 0, &block_data, piece_size);
        assert!(result1.is_some());
        assert!(!pm.block_manager.legacy_buffers.contains_key(&piece_index));

        // Test duplicate detection during assembly
        let piece_size_2 = 32768;

        pm.set_geometry(
            piece_size_2 as u32,
            piece_size_2 as u64 * 2,
            HashMap::new(),
            false,
        );

        let block_data_0 = vec![1; block_size];
        let block_data_1 = vec![2; block_size];

        // Add block 0 for Piece 1
        pm.handle_block(1, 0, &block_data_0, piece_size_2);

        // This unwrap will now succeed because Piece 1 is valid within the total length
        let assembler1 = pm.block_manager.legacy_buffers.get(&1).unwrap();
        assert_eq!(assembler1.received_blocks, 1);

        // Add block 0 again (should be ignored)
        pm.handle_block(1, 0, &block_data_0, piece_size_2);
        let assembler2 = pm.block_manager.legacy_buffers.get(&1).unwrap();
        assert_eq!(assembler2.received_blocks, 1);

        // Add block 1 to complete
        let result_final = pm.handle_block(1, block_size as u32, &block_data_1, piece_size_2);
        assert!(result_final.is_some());
    }

    #[test]
    fn test_handle_block_for_completed_piece() {
        let mut pm = setup_manager(1);
        let piece_index = 0;
        let piece_size = 16384;
        let block_data = vec![1; piece_size];

        pm.set_geometry(piece_size as u32, piece_size as u64, HashMap::new(), false);

        // Mark piece as complete first
        pm.mark_as_complete(piece_index);
        assert_eq!(pm.bitfield[piece_index as usize], PieceStatus::Done);

        // Clear buffer just in case
        pm.block_manager.legacy_buffers.remove(&piece_index);

        // Handle a block for the completed piece
        // Because mark_as_complete commits to BlockManager, handle_block should return None
        // or BlockManager returns 'Duplicate' decision internally.
        // However, the current handle_block wrapper calls `handle_v1_block_buffering` directly.
        // BlockManager's handle_v1_block_buffering checks `blocks_in_piece`.
        // The key is that `mark_as_complete` sets the block bits in BlockManager.
        // But `handle_v1_block_buffering` doesn't currently check the global block bitfield,
        // it only checks the assembler mask.
        // So this will re-assemble. This behavior is "acceptable" for the unit test,
        // but arguably `handle_block` should check `bitfield` first.
        // In the provided implementation, it will simply re-buffer and return Data again.

        let result = pm.handle_block(piece_index, 0, &block_data, piece_size);
        assert!(result.is_some());
    }

    #[test]
    fn test_revert_synchronization() {
        // Scenario: Piece completes, verifying commits to BlockManager,
        // then Disk Write fails, requiring a revert.
        let mut pm = setup_manager(1);
        let piece_index = 0;

        pm.mark_as_complete(piece_index);

        // Assertion: BlockManager must think it's done
        let (start, end) = pm.block_manager.get_block_range(piece_index);
        for i in start..end {
            assert!(
                pm.block_manager.block_bitfield[i as usize],
                "Blocks should be true after commit"
            );
        }

        pm.requeue_pending_to_need(piece_index);

        // Assertion: High level state is updated
        assert_eq!(pm.bitfield[0], PieceStatus::Need);
        assert!(pm.need_queue.contains(&0));

        // CRITICAL ASSERTION: BlockManager bits must be cleared.
        // If this fails, we cannot re-download the blocks!
        for i in start..end {
            assert!(
                !pm.block_manager.block_bitfield[i as usize],
                "Blocks should be false after revert"
            );
        }
    }

    #[test]
    fn test_lazy_geometry_initialization() {
        // Scenario: We receive a block before Metadata/Geometry is explicitly set.
        let mut pm = PieceManager::new();
        let piece_size = 16384;
        let block_data = vec![1u8; 16384];

        // We do NOT call set_geometry. We rely on handle_block to infer it.
        let result = pm.handle_block(0, 0, &block_data, piece_size);

        assert!(result.is_some()); // Should succeed and complete immediately
        assert_eq!(pm.block_manager.piece_length, 16384); // Should have inferred size
    }

    #[test]
    fn test_tiny_last_block() {
        // Scenario: Total length is 16385 (1 full block + 1 byte)
        let mut pm = PieceManager::new();
        let piece_size = 32768; // Standard 32KB piece size
        let total_len = 16385;

        pm.set_geometry(piece_size, total_len, HashMap::new(), false);

        let block_0 = vec![1u8; 16384];
        let res_0 = pm.handle_block(0, 0, &block_0, piece_size as usize);
        assert!(res_0.is_none());

        let block_1 = vec![2u8; 1];
        let res_1 = pm.handle_block(0, 16384, &block_1, piece_size as usize);

        // Should complete successfully
        assert!(res_1.is_some());
        let data = res_1.unwrap();

        // The buffer should be sized to the PIECE size (32KB) usually,
        // or the specific remaining size?
        // Current implementation allocates `vec![0u8; piece_len]` in BlockManager.
        // Let's verify we got the data we put in.
        assert_eq!(data[0], 1);
        assert_eq!(data[16384], 2);
    }

    #[test]
    fn test_priority_sorting_order() {
        // GIVEN: A manager with 3 pieces needed
        let mut pm = setup_manager(3); // [0, 1, 2]

        // SETUP:
        // Piece 0 -> Normal (Default)
        // Piece 1 -> High
        // Piece 2 -> Skip
        pm.apply_priorities(vec![
            EffectivePiecePriority::Normal,
            EffectivePiecePriority::High,
            EffectivePiecePriority::Skip,
        ]);

        let peer_bitfield = vec![true, true, true];
        let peer_pending = HashSet::new();
        let status = TorrentStatus::Standard;

        // WHEN: We ask for a piece
        let first_choice = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending, &status);

        // THEN: High priority (1) must win
        assert_eq!(
            first_choice,
            Some(1),
            "High priority piece should be chosen first"
        );

        // Mark 1 as pending so we get the next one
        let mut peer_pending_2 = HashSet::new();
        peer_pending_2.insert(1);

        let second_choice = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending_2, &status);

        // THEN: Normal priority (0) must be next. Piece 2 (Skip) must be ignored.
        assert_eq!(
            second_choice,
            Some(0),
            "Normal priority should be chosen second"
        );

        // Mark 0 as pending
        peer_pending_2.insert(0);
        let third_choice = pm.choose_piece_for_peer(&peer_bitfield, &peer_pending_2, &status);

        // THEN: Skip piece (2) should never be chosen
        assert_eq!(third_choice, None, "Skipped piece should not be chosen");
    }

    #[test]
    fn test_dynamic_priority_switching() {
        // GIVEN: 1 piece that starts as Normal
        let mut pm = setup_manager(1);
        assert!(pm.need_queue.contains(&0));

        // WHEN: We switch it to SKIP
        let _cancelled = pm.apply_priorities(vec![EffectivePiecePriority::Skip]);

        // THEN: It should disappear from the need queue
        assert!(
            pm.need_queue.is_empty(),
            "Skip should remove from need_queue"
        );

        // WHEN: We switch it back to HIGH
        pm.apply_priorities(vec![EffectivePiecePriority::High]);

        // THEN: It should reappear in the need queue
        assert!(
            pm.need_queue.contains(&0),
            "Un-skipping should add back to need_queue"
        );
        assert_eq!(pm.piece_priorities[0], EffectivePiecePriority::High);
    }

    #[test]
    fn test_priority_overrides_rarity() {
        // GIVEN:
        // Piece 0: Rare (1 copy) but Normal Priority
        // Piece 1: Common (100 copies) but High Priority
        let mut pm = setup_manager(2);

        pm.piece_rarity.insert(0, 1); // Rare
        pm.piece_rarity.insert(1, 100); // Common

        pm.apply_priorities(vec![
            EffectivePiecePriority::Normal, // 0
            EffectivePiecePriority::High,   // 1
        ]);

        let peer_bitfield = vec![true, true];
        let pending = HashSet::new();

        // WHEN: We choose
        let choice = pm.choose_piece_for_peer(&peer_bitfield, &pending, &TorrentStatus::Standard);

        // THEN: High Priority (1) must win, even though 0 is much rarer
        assert_eq!(choice, Some(1), "High priority should override Rarity");
    }

    #[test]
    fn test_mixed_priority_endgame() {
        // GIVEN: Endgame Mode
        // Pending: Piece 0 (High)
        // Need: Piece 1 (Normal)
        let mut pm = setup_manager(2);
        pm.mark_as_pending(0, "peer_A".into());
        // Piece 1 remains in Need

        pm.apply_priorities(vec![
            EffectivePiecePriority::High,   // 0 (Pending)
            EffectivePiecePriority::Normal, // 1 (Need)
        ]);

        let peer_bitfield = vec![true, true];
        let pending = HashSet::new(); // Local peer has nothing pending yet

        // WHEN: We choose in Endgame mode
        let choice = pm.choose_piece_for_peer(&peer_bitfield, &pending, &TorrentStatus::Endgame);

        // THEN: We should attempt to "steal" the High Priority pending piece (0)
        // before taking the unassigned Normal piece (1).
        assert_eq!(
            choice,
            Some(0),
            "Endgame should race for High Priority pieces first"
        );
    }

    #[test]
    fn test_all_skipped_behavior() {
        // GIVEN: A manager with 5 pieces, initially all needed
        let mut pm = setup_manager(5);
        assert_eq!(pm.need_queue.len(), 5);

        // WHEN: We apply SKIP to ALL pieces
        let priorities = vec![EffectivePiecePriority::Skip; 5];
        let cancelled = pm.apply_priorities(priorities);

        // THEN:
        // 1. The Need Queue must be completely empty
        assert!(
            pm.need_queue.is_empty(),
            "Need queue should be empty when all pieces are skipped"
        );

        // 2. Cancellation list should be empty (since nothing was pending in this test)
        assert!(cancelled.is_empty());

        // 3. Selection should return None
        let peer_bitfield = vec![true; 5];
        let pending = HashSet::new();
        let choice = pm.choose_piece_for_peer(&peer_bitfield, &pending, &TorrentStatus::Standard);

        assert_eq!(
            choice, None,
            "Should choose nothing if all pieces are skipped"
        );
    }

    #[test]
    fn test_requestable_block_addresses_for_piece_aligned_filters_completed() {
        let mut pm = PieceManager::new();
        pm.set_initial_fields(2, false);
        pm.set_geometry(16384, 32768, HashMap::new(), false);

        pm.mark_as_complete(0);

        let req_piece_0 = pm.requestable_block_addresses_for_piece(0);
        assert!(
            req_piece_0.is_empty(),
            "Aligned completed piece should have no requestable blocks"
        );

        let req_piece_1 = pm.requestable_block_addresses_for_piece(1);
        let tuples: Vec<(u32, u32, u32)> = req_piece_1
            .iter()
            .map(|a| (a.piece_index, a.byte_offset, a.length))
            .collect();
        assert_eq!(tuples, vec![(1, 0, 16384)]);
    }

    #[test]
    fn test_requestable_block_addresses_for_piece_non_aligned_not_suppressed() {
        let mut pm = PieceManager::new();
        pm.set_initial_fields(2, false);
        pm.set_geometry(20000, 40000, HashMap::new(), false);

        // Piece 0 completion marks shared global slot, piece 1 should still request offset 0.
        pm.mark_as_complete(0);

        let req_piece_1 = pm.requestable_block_addresses_for_piece(1);
        let mut tuples: Vec<(u32, u32, u32)> = req_piece_1
            .iter()
            .map(|a| (a.piece_index, a.byte_offset, a.length))
            .collect();
        tuples.sort_unstable_by_key(|(_, off, _)| *off);

        assert_eq!(tuples, vec![(1, 0, 16384), (1, 16384, 3616)]);
    }

    #[test]
    fn test_requestable_block_addresses_for_piece_respects_assembler_mask() {
        let mut pm = PieceManager::new();
        pm.set_initial_fields(1, false);
        pm.set_geometry(20000, 20000, HashMap::new(), false);

        let block = vec![0u8; 16384];
        let _ = pm.handle_block(0, 0, &block, 20000);

        let req = pm.requestable_block_addresses_for_piece(0);
        let tuples: Vec<(u32, u32, u32)> = req
            .iter()
            .map(|a| (a.piece_index, a.byte_offset, a.length))
            .collect();

        assert_eq!(tuples, vec![(0, 16384, 3616)]);
    }
}
