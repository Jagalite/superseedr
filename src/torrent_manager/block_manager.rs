#[allow(dead_code)]
// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::{HashMap, HashSet};

pub const BLOCK_SIZE: u32 = 16_384;

#[allow(dead_code)]
pub const V2_HASH_LEN: usize = 32;

#[derive(Debug, Clone)]
pub struct LegacyAssembler {
    pub buffer: Vec<u8>,
    pub received_blocks: usize,
    pub total_blocks: usize,
    pub mask: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockAddress {
    pub piece_index: u32,
    pub block_index: u32,
    pub byte_offset: u32,
    pub global_offset: u64,
    pub length: u32,
}

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum BlockResult {
    Accepted,
    Duplicate,
    V1BlockBuffered,
    V1PieceVerified { piece_index: u32, data: Vec<u8> },
}

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum BlockDecision {
    VerifyV2 {
        file_index: usize,
        root_hash: [u8; 32],
        block_index_in_file: u32,
    },
    BufferV1,
    Duplicate,
    Error,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileInfo {
    pub start_offset: u64,
    pub end_offset: u64,
    pub root_hash: [u8; 32],
}

#[derive(Default, Debug, Clone)]
pub struct BlockManager {
    // --- STATE ---
    pub block_bitfield: Vec<bool>,
    pub pending_blocks: HashSet<u32>,
    pub piece_rarity: HashMap<u32, usize>,

    // --- METADATA ---
    pub piece_hashes_v1: Vec<[u8; 20]>,

    // V2: Files are mapped by index to their geometry and root hash
    pub files: Vec<FileInfo>,

    // This allows pieces to be shorter than standard length even if they aren't the global last piece.
    pub piece_lengths: HashMap<u32, u32>,

    pub legacy_buffers: HashMap<u32, LegacyAssembler>,

    // --- GEOMETRY ---
    pub piece_length: u32,
    pub total_length: u64,
    pub total_blocks: u32,
}

#[allow(dead_code)]
impl BlockManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_geometry(
        &mut self,
        piece_length: u32,
        total_length: u64,
        v1_hashes: Vec<[u8; 20]>,
        // Map of file_index -> (size, root_hash)
        v2_file_info: Vec<(u64, [u8; 32])>,

        piece_overrides: HashMap<u32, u32>,
        validation_complete: bool,
    ) {
        self.piece_length = piece_length;
        self.total_length = total_length;
        self.piece_hashes_v1 = v1_hashes;
        self.piece_lengths = piece_overrides;

        // Construct File Layout
        let mut current_offset = 0;
        self.files = v2_file_info
            .into_iter()
            .map(|(size, root)| {
                let info = FileInfo {
                    start_offset: current_offset,
                    end_offset: current_offset + size,
                    root_hash: root,
                };
                current_offset += size;
                info
            })
            .collect();

        self.total_blocks = (total_length as f64 / BLOCK_SIZE as f64).ceil() as u32;
        self.block_bitfield = vec![validation_complete; self.total_blocks as usize];
    }

    /// Determines what to do with an incoming block:
    /// 1. If it maps to a V2 file, return VerifyV2 (Caller must handle async hashing).
    /// 2. If it's V1, return BufferV1 (Manager handles buffering).
    pub fn handle_incoming_block_decision(&self, addr: BlockAddress) -> BlockDecision {
        let global_idx = self.flatten_address(addr);

        if global_idx as usize >= self.block_bitfield.len() {
            return BlockDecision::Error;
        }
        if self.block_bitfield[global_idx as usize] {
            return BlockDecision::Duplicate;
        }

        // V2 Check: Do we have a V2 Root for this file location?
        if let Some((file_idx, file)) = self.get_file_for_offset(addr.global_offset) {
            // Calculate which block index *within this specific file* we are verifying
            let offset_in_file = addr.global_offset - file.start_offset;
            let block_index_in_file = (offset_in_file / BLOCK_SIZE as u64) as u32;

            return BlockDecision::VerifyV2 {
                file_index: file_idx,
                root_hash: file.root_hash,
                block_index_in_file,
            };
        }

        BlockDecision::BufferV1
    }

    // --- HELPER: Find which file owns this offset ---
    fn get_file_for_offset(&self, global_offset: u64) -> Option<(usize, &FileInfo)> {
        // Simple linear scan for now; Binary search recommended for production with many files
        self.files
            .iter()
            .enumerate()
            .find(|(_, f)| global_offset >= f.start_offset && global_offset < f.end_offset)
    }

    // --- STATE COMMITMENT ---

    pub fn commit_verified_block(&mut self, addr: BlockAddress) -> BlockResult {
        let global_idx = self.flatten_address(addr);

        if global_idx as usize >= self.block_bitfield.len() {
            return BlockResult::Duplicate;
        }

        if self.block_bitfield[global_idx as usize] {
            return BlockResult::Duplicate;
        }

        self.block_bitfield[global_idx as usize] = true;
        self.pending_blocks.remove(&global_idx);

        BlockResult::Accepted
    }

    // --- WORK SELECTION ---

    pub fn pick_blocks_for_peer(
        &self,
        peer_bitfield: &[bool],
        count: usize,
        rarest_pieces: &[u32],
        endgame_mode: bool,
    ) -> Vec<BlockAddress> {
        let mut picked = Vec::with_capacity(count);

        for &piece_idx in rarest_pieces {
            if picked.len() >= count {
                break;
            }

            // Skip if peer doesn't have it
            if !peer_bitfield.get(piece_idx as usize).unwrap_or(&false) {
                continue;
            }

            let (start_blk, end_blk) = self.get_block_range(piece_idx);

            for global_idx in start_blk..end_blk {
                if picked.len() >= count {
                    break;
                }

                let already_have = self
                    .block_bitfield
                    .get(global_idx as usize)
                    .copied()
                    .unwrap_or(true);
                let is_pending = self.pending_blocks.contains(&global_idx);

                if !already_have && (!is_pending || endgame_mode) {
                    picked.push(self.inflate_address(global_idx));
                }
            }
        }
        picked
    }

    pub fn mark_pending(&mut self, global_idx: u32) {
        self.pending_blocks.insert(global_idx);
    }

    pub fn unmark_pending(&mut self, global_idx: u32) {
        self.pending_blocks.remove(&global_idx);
    }

    // --- GEOMETRY HELPERS ---

    fn blocks_in_piece(&self, piece_len: u32) -> u32 {
        piece_len.div_ceil(BLOCK_SIZE)
    }

    pub fn get_block_range(&self, piece_idx: u32) -> (u32, u32) {
        let piece_len = self.calculate_piece_size(piece_idx);
        let blocks_in_piece = self.blocks_in_piece(piece_len);

        let piece_start_offset = piece_idx as u64 * self.piece_length as u64;
        let start_blk = (piece_start_offset / BLOCK_SIZE as u64) as u32;
        let actual_start_blk = std::cmp::min(start_blk, self.total_blocks);
        let end_blk = std::cmp::min(actual_start_blk + blocks_in_piece, self.total_blocks);
        (actual_start_blk, end_blk)
    }

    pub fn is_non_aligned_piece_grid(&self) -> bool {
        self.piece_length != 0 && !self.piece_length.is_multiple_of(BLOCK_SIZE)
    }

    pub fn piece_block_addresses(&self, piece_index: u32) -> Vec<BlockAddress> {
        let piece_len = self.calculate_piece_size(piece_index);
        if piece_len == 0 {
            return Vec::new();
        }

        let block_count = self.blocks_in_piece(piece_len);
        let mut out = Vec::with_capacity(block_count as usize);
        for block_index in 0..block_count {
            let byte_offset = block_index * BLOCK_SIZE;
            let length = std::cmp::min(BLOCK_SIZE, piece_len.saturating_sub(byte_offset));
            if length == 0 {
                continue;
            }
            if let Some(addr) = self.inflate_address_from_overlay(piece_index, byte_offset, length)
            {
                out.push(addr);
            }
        }

        out
    }

    fn calculate_piece_size(&self, piece_idx: u32) -> u32 {
        if let Some(&len) = self.piece_lengths.get(&piece_idx) {
            return len;
        }

        let offset = piece_idx as u64 * self.piece_length as u64;
        let remaining = self.total_length.saturating_sub(offset);
        std::cmp::min(self.piece_length as u64, remaining) as u32
    }

    pub fn inflate_address(&self, global_idx: u32) -> BlockAddress {
        let global_offset = global_idx as u64 * BLOCK_SIZE as u64;
        let piece_index = (global_offset / self.piece_length as u64) as u32;
        let byte_offset_in_piece = (global_offset % self.piece_length as u64) as u32;

        let valid_piece_len = self.calculate_piece_size(piece_index);
        let remaining_in_piece =
            (valid_piece_len as u64).saturating_sub(byte_offset_in_piece as u64);
        let length = std::cmp::min(BLOCK_SIZE as u64, remaining_in_piece) as u32;

        BlockAddress {
            piece_index,
            block_index: (byte_offset_in_piece / BLOCK_SIZE),
            byte_offset: byte_offset_in_piece,
            global_offset,
            length,
        }
    }

    pub fn flatten_address(&self, addr: BlockAddress) -> u32 {
        (addr.global_offset / BLOCK_SIZE as u64) as u32
    }

    pub fn is_piece_complete(&self, piece_index: u32) -> bool {
        // On non-aligned piece grids, global 16KiB blocks can overlap adjacent pieces,
        // so block-bitfield-only completion checks are ambiguous.
        if self.is_non_aligned_piece_grid() {
            return false;
        }

        let (start, end) = self.get_block_range(piece_index);
        for i in start..end {
            if !self
                .block_bitfield
                .get(i as usize)
                .copied()
                .unwrap_or(false)
            {
                return false;
            }
        }
        true
    }

    // --- V1 COMPATIBILITY BUFFERING ---
    pub fn handle_v1_block_buffering(
        &mut self,
        addr: BlockAddress,
        data: &[u8],
    ) -> Option<Vec<u8>> {
        let piece_len = self.calculate_piece_size(addr.piece_index);
        let num_blocks = self.blocks_in_piece(piece_len);

        // Get or create the assembler.
        let assembler = self
            .legacy_buffers
            .entry(addr.piece_index)
            .or_insert_with(|| LegacyAssembler {
                buffer: vec![0u8; piece_len as usize],
                received_blocks: 0,
                total_blocks: num_blocks as usize,
                mask: vec![false; num_blocks as usize],
            });

        // If it was already complete, do nothing. This prevents re-verification.
        if assembler.received_blocks == assembler.total_blocks {
            return None;
        }

        let offset = addr.byte_offset as usize;
        let end = offset + data.len();

        // Check bounds and if we already have this block.
        if end <= assembler.buffer.len() && !assembler.mask[addr.block_index as usize] {
            assembler.buffer[offset..end].copy_from_slice(data);
            assembler.mask[addr.block_index as usize] = true;
            assembler.received_blocks += 1;
        }

        // If it's now complete, remove it and return the data.
        if assembler.received_blocks == assembler.total_blocks {
            return self
                .legacy_buffers
                .remove(&addr.piece_index)
                .map(|a| a.buffer);
        }

        None
    }

    pub fn inflate_address_from_overlay(
        &self,
        piece_index: u32,
        byte_offset: u32,
        length: u32,
    ) -> Option<BlockAddress> {
        let piece_len = self.calculate_piece_size(piece_index);
        if byte_offset.saturating_add(length) > piece_len {
            return None;
        }

        let piece_start = piece_index as u64 * self.piece_length as u64;
        let global_offset = piece_start + byte_offset as u64;

        Some(BlockAddress {
            piece_index,
            block_index: byte_offset / BLOCK_SIZE,
            byte_offset,
            global_offset,
            length,
        })
    }

    pub fn total_pieces(&self) -> usize {
        self.piece_hashes_v1.len()
    }

    pub fn update_rarity<'a, I>(&mut self, peer_bitfields: I)
    where
        I: Iterator<Item = &'a Vec<bool>>,
    {
        // Count by index first, avoiding a hash lookup for every peer-piece pair.
        let mut counts = Vec::<usize>::new();
        for bitfield in peer_bitfields {
            if counts.len() < bitfield.len() {
                counts.resize(bitfield.len(), 0);
            }
            for (count, &has_piece) in counts.iter_mut().zip(bitfield) {
                *count += usize::from(has_piece);
            }
        }
        self.piece_rarity.clear();
        self.piece_rarity.extend(
            counts
                .into_iter()
                .enumerate()
                .filter(|(_, count)| *count > 0)
                .map(|(piece, count)| (piece as u32, count)),
        );
    }

    pub fn release_pending_blocks_for_peer(&mut self, pending: &HashSet<BlockAddress>) {
        for addr in pending {
            let global_idx = self.flatten_address(*addr);
            self.unmark_pending(global_idx);
        }
    }

    pub fn get_rarest_pieces(&self) -> Vec<u32> {
        let mut pieces: Vec<u32> = (0..self.total_pieces() as u32).collect();
        pieces.retain(|&idx| !self.is_piece_complete(idx));
        pieces.sort_by_key(|idx| self.piece_rarity.get(idx).copied().unwrap_or(0));
        pieces
    }

    pub fn commit_v1_piece(&mut self, piece_index: u32) {
        let (start, end) = self.get_block_range(piece_index);
        for global_idx in start..end {
            if (global_idx as usize) < self.block_bitfield.len() {
                self.block_bitfield[global_idx as usize] = true;
            }
            self.pending_blocks.remove(&global_idx);
        }
        self.legacy_buffers.remove(&piece_index);
    }

    pub fn revert_v1_piece_completion(&mut self, piece_index: u32) {
        let (start, end) = self.get_block_range(piece_index);
        for global_idx in start..end {
            if (global_idx as usize) < self.block_bitfield.len() {
                self.block_bitfield[global_idx as usize] = false;
            }
        }
        // Ensure buffer is gone so we can re-download/re-verify if needed
        self.legacy_buffers.remove(&piece_index);
    }

    pub fn reset_v1_buffer(&mut self, piece_index: u32) {
        self.legacy_buffers.remove(&piece_index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const BLK_SIZE: u32 = BLOCK_SIZE; // 16384

    proptest! {
        #[test]
        fn prop_rarity_rebuild_matches_original_across_snapshots(
            snapshots in proptest::collection::vec(
                proptest::collection::vec(
                    proptest::collection::vec(any::<bool>(), 0..256), 0..16,
                ), 1..8,
            ),
        ) {
            let mut manager = BlockManager::new();
            // Catch stale entries, including indices beyond every new bitfield.
            manager.piece_rarity.insert(10_000, 42);
            for fields in snapshots {
                let mut reference = HashMap::<u32, usize>::new();
                for field in &fields {
                    for (piece, &has_piece) in field.iter().enumerate() {
                        if has_piece {
                            *reference.entry(piece as u32).or_default() += 1;
                        }
                    }
                }
                manager.update_rarity(fields.iter());
                prop_assert_eq!(&manager.piece_rarity, &reference);
                // Reordering peers must not affect counts.
                manager.update_rarity(fields.iter().rev());
                prop_assert_eq!(&manager.piece_rarity, &reference);
            }
            manager.update_rarity(std::iter::empty::<&Vec<bool>>());
            prop_assert!(manager.piece_rarity.is_empty());
        }
    }

    #[test]
    fn rarity_rebuild_matches_counts_across_peer_changes() {
        let mut manager = BlockManager::new();
        let mut reference = HashMap::<u32, usize>::new();
        for peer_count in [0usize, 1, 8, 96, 3, 0] {
            let fields: Vec<Vec<bool>> = (0..peer_count)
                .map(|peer| {
                    (0..33_080 - peer * 7)
                        .map(|piece| peer % 3 == 0 || (piece * 31 + peer * 17) % 11 < 4)
                        .collect()
                })
                .collect();

            // Reference the original per-peer counting behavior, including
            // uneven bitfields and removal of entries when peers leave.
            reference.clear();
            for field in &fields {
                for (piece, &has_piece) in field.iter().enumerate() {
                    if has_piece {
                        *reference.entry(piece as u32).or_insert(0) += 1;
                    }
                }
            }
            manager.update_rarity(fields.iter());
            assert_eq!(manager.piece_rarity, reference);
        }

        let fields = [vec![], vec![false, true, false], vec![true]];
        manager.update_rarity(fields.iter());
        assert_eq!(manager.piece_rarity, HashMap::from([(0, 1), (1, 1)]));
    }

    // Helper to create a basic BlockManager
    fn setup_manager(piece_len: u32, total_len: u64) -> BlockManager {
        let piece_count = (total_len as f64 / piece_len as f64).ceil() as usize;
        let v1_hashes = vec![[0; 20]; piece_count];
        let mut manager = BlockManager::new();
        manager.set_geometry(
            piece_len,
            total_len,
            v1_hashes,
            vec![],
            HashMap::new(),
            false,
        );
        manager
    }

    #[test]
    fn test_geometry_and_total_blocks() {
        // Case 1: Perfect alignment
        let piece_len = 2 * BLK_SIZE; // 32768
        let total_len = piece_len as u64 * 3; // 3 pieces total
        let manager = setup_manager(piece_len, total_len);

        // Piece 0: 2 blocks (0-1), Piece 1: 2 blocks (2-3), Piece 2: 2 blocks (4-5)
        // Total blocks: 6
        assert_eq!(manager.piece_length, piece_len);
        assert_eq!(manager.total_length, total_len);
        assert_eq!(manager.total_pieces(), 3);
        assert_eq!(manager.total_blocks, 6); // 3 * (32768 / 16384)

        // Case 2: Uneven total length
        let total_len = 100_000u64; // Requires 7 blocks (6 * 16384 + 1)
        let manager = setup_manager(piece_len, total_len);
        assert_eq!(manager.total_blocks, 7);
    }

    #[test]
    fn test_calculate_piece_size_full_and_last() {
        let piece_len = 4 * BLK_SIZE; // 65536
        let total_len = (piece_len as u64 * 2) + (BLK_SIZE as u64 / 2); // Two full pieces + small remainder
        let manager = setup_manager(piece_len, total_len);

        // Piece 0 (full)
        assert_eq!(manager.calculate_piece_size(0), piece_len);

        // Piece 1 (full)
        assert_eq!(manager.calculate_piece_size(1), piece_len);

        // Piece 2 (partial) - Expected size BLK_SIZE/2 (8192)
        assert_eq!(manager.calculate_piece_size(2), BLK_SIZE / 2);

        // Piece 3 (non-existent)
        assert_eq!(manager.calculate_piece_size(3), 0);
    }

    #[test]
    fn test_block_range_calculation() {
        let piece_len = 3 * BLK_SIZE; // 49152 (3 blocks)
        let total_len = piece_len as u64 * 2 + (BLK_SIZE as u64 / 2); // 2 full pieces + partial last
        let manager = setup_manager(piece_len, total_len);

        // Piece 0: 3 blocks (0, 1, 2)
        assert_eq!(manager.get_block_range(0), (0, 3));

        // Piece 1: 3 blocks (3, 4, 5)
        assert_eq!(manager.get_block_range(1), (3, 6));

        // Piece 2 (partial): 1 block (6)
        assert_eq!(manager.get_block_range(2), (6, 7));

        // Non-existent piece: 0 blocks
        assert_eq!(manager.get_block_range(3), (7, 7));
    }

    #[test]
    fn test_inflate_and_flatten_address() {
        let piece_len = 4 * BLK_SIZE; // 65536
        let total_len = piece_len as u64 * 2;
        let manager = setup_manager(piece_len, total_len);

        let global_idx_0 = 0;
        let addr_0 = manager.inflate_address(global_idx_0);
        assert_eq!(addr_0.piece_index, 0);
        assert_eq!(addr_0.byte_offset, 0);
        assert_eq!(addr_0.global_offset, 0);
        assert_eq!(addr_0.length, BLK_SIZE);
        assert_eq!(manager.flatten_address(addr_0), global_idx_0);

        let global_idx_3 = 3;
        let addr_3 = manager.inflate_address(global_idx_3);
        assert_eq!(addr_3.piece_index, 0);
        assert_eq!(addr_3.byte_offset, 3 * BLK_SIZE);
        assert_eq!(addr_3.global_offset, 3 * BLK_SIZE as u64);
        assert_eq!(addr_3.length, BLK_SIZE);
        assert_eq!(manager.flatten_address(addr_3), global_idx_3);

        let global_idx_4 = 4;
        let addr_4 = manager.inflate_address(global_idx_4);
        assert_eq!(addr_4.piece_index, 1);
        assert_eq!(addr_4.byte_offset, 0);
        assert_eq!(addr_4.global_offset, 4 * BLK_SIZE as u64);
        assert_eq!(addr_4.length, BLK_SIZE);
        assert_eq!(manager.flatten_address(addr_4), global_idx_4);
    }

    #[test]
    fn test_inflate_address_final_partial_block() {
        let piece_len = 4 * BLK_SIZE; // 65536
                                      // Total length is 1 full piece + 1/2 of a block for piece 1
        let total_len = piece_len as u64 + (BLK_SIZE as u64 / 2);
        let manager = setup_manager(piece_len, total_len);

        // Piece 0 blocks (0, 1, 2, 3)
        // Piece 1 blocks (4) -> only 8192 bytes
        let global_idx_4 = 4;
        let addr_4 = manager.inflate_address(global_idx_4);

        assert_eq!(manager.total_blocks, 5); // 4 full blocks + 1 partial block
        assert_eq!(addr_4.piece_index, 1);
        assert_eq!(addr_4.byte_offset, 0);
        assert_eq!(addr_4.global_offset, 4 * BLK_SIZE as u64);
        assert_eq!(addr_4.length, BLK_SIZE / 2); // Half block (8192)
        assert_eq!(manager.flatten_address(addr_4), global_idx_4);
    }

    #[test]
    fn test_inflate_address_from_overlay_security_guard() {
        let piece_len = 2 * BLK_SIZE; // 32768
        let total_len = piece_len as u64;
        let manager = setup_manager(piece_len, total_len);

        // VALID: Block 0 of Piece 0, full size
        let valid_addr = manager.inflate_address_from_overlay(0, 0, BLK_SIZE);
        assert!(valid_addr.is_some());

        // VALID: Last block of Piece 0, starting at BLK_SIZE, size BLK_SIZE
        let valid_addr_2 = manager.inflate_address_from_overlay(0, BLK_SIZE, BLK_SIZE);
        assert!(valid_addr_2.is_some());

        // INVALID: Starts at the last byte of the piece, but asks for BLK_SIZE
        let invalid_addr_1 = manager.inflate_address_from_overlay(0, piece_len - 1, BLK_SIZE);
        assert!(invalid_addr_1.is_none());

        // INVALID: Starts at BLK_SIZE, asks for BLK_SIZE + 1 (Oversize)
        let invalid_addr_2 = manager.inflate_address_from_overlay(0, BLK_SIZE, BLK_SIZE + 1);
        assert!(invalid_addr_2.is_none());

        // INVALID: Starts one byte past the piece length
        let invalid_addr_3 = manager.inflate_address_from_overlay(0, piece_len, BLK_SIZE);
        assert!(invalid_addr_3.is_none());
    }

    #[test]
    fn test_non_aligned_adjacent_piece_completion_independence() {
        // This captures the boundary-aliasing risk when piece length is not block-aligned.
        let piece_len = 20_000;
        let total_len = 40_000;
        let mut manager = setup_manager(piece_len, total_len);

        // Mark piece 0 complete first (sets global blocks 0 and 1).
        manager.commit_v1_piece(0);
        assert!(
            !manager.is_piece_complete(1),
            "Piece 1 must not be complete after only piece 0 has been committed"
        );

        // Simulate receiving the second global block for piece 1's range.
        let addr = manager.inflate_address(2);
        let _ = manager.commit_verified_block(addr);

        // Expected behavior: still incomplete, because the initial bytes of piece 1 were never received
        // in piece-1-local space.
        assert!(
            !manager.is_piece_complete(1),
            "Piece 1 should not be marked complete via shared global boundary blocks alone"
        );
    }

    #[test]
    fn test_decision_routing_v1_only() {
        let mut bm = BlockManager::new();
        // V1 Setup: No V2 file info provided
        bm.set_geometry(16384, 16384 * 10, vec![], vec![], HashMap::new(), false);

        let addr = bm.inflate_address(0); // Block 0
        let decision = bm.handle_incoming_block_decision(addr);

        // MUST return BufferV1
        assert_eq!(decision, BlockDecision::BufferV1);
    }

    #[test]
    fn test_decision_routing_v2_simple() {
        let mut bm = BlockManager::new();
        let root_a = [0xAA; 32];
        let root_b = [0xBB; 32];

        // V2 Setup: 2 Files.
        // File A: 32KB (2 blocks)
        // File B: 16KB (1 block)
        let v2_info = vec![(32768, root_a), (16384, root_b)];

        // Total len = 48KB
        bm.set_geometry(16384, 49152, vec![], v2_info, HashMap::new(), false);

        let addr_a1 = bm.inflate_address(0); // Block 0
        let dec_a1 = bm.handle_incoming_block_decision(addr_a1);

        match dec_a1 {
            BlockDecision::VerifyV2 {
                file_index,
                root_hash,
                block_index_in_file,
            } => {
                assert_eq!(file_index, 0); // File A
                assert_eq!(root_hash, root_a);
                assert_eq!(block_index_in_file, 0);
            }
            _ => panic!("Expected VerifyV2 for File A"),
        }

        let addr_b = bm.inflate_address(2); // Block 2
        let dec_b = bm.handle_incoming_block_decision(addr_b);

        match dec_b {
            BlockDecision::VerifyV2 {
                file_index,
                root_hash,
                block_index_in_file,
            } => {
                assert_eq!(file_index, 1); // File B
                assert_eq!(root_hash, root_b);
                assert_eq!(block_index_in_file, 0); // First block relative to File B
            }
            _ => panic!("Expected VerifyV2 for File B"),
        }
    }

    #[test]
    fn test_decision_routing_boundary_check() {
        let mut bm = BlockManager::new();
        let root = [0xCC; 32];
        // File starts at 0, ends at 16385 (1 block + 1 byte)
        let v2_info = vec![(16385, root)];

        bm.set_geometry(16384, 16385, vec![], v2_info, HashMap::new(), false);

        let addr_0 = bm.inflate_address(0);
        let dec_0 = bm.handle_incoming_block_decision(addr_0);
        assert!(matches!(
            dec_0,
            BlockDecision::VerifyV2 {
                block_index_in_file: 0,
                ..
            }
        ));

        // Global offset 16384 is inside the file range [0, 16385)
        let addr_1 = bm.inflate_address(1);
        let dec_1 = bm.handle_incoming_block_decision(addr_1);

        match dec_1 {
            BlockDecision::VerifyV2 {
                file_index,
                block_index_in_file,
                ..
            } => {
                assert_eq!(file_index, 0);
                assert_eq!(block_index_in_file, 1);
            }
            _ => panic!("Expected VerifyV2 for partial block at end of file"),
        }
    }

    #[test]
    fn test_endgame_duplicate_completion_suppression() {
        let mut bm = BlockManager::new();
        let piece_len = 32768;
        let total_len = 32768;
        // v1_hashes and v2_file_info can be empty for this logic test
        bm.set_geometry(piece_len, total_len, vec![], vec![], HashMap::new(), false);

        let block_size = 16384;
        let data_block_0 = vec![1u8; block_size];
        let data_block_1 = vec![2u8; block_size];

        // Create addresses for Block 0 and Block 1
        let addr_0 = bm
            .inflate_address_from_overlay(0, 0, block_size as u32)
            .unwrap();
        let addr_1 = bm
            .inflate_address_from_overlay(0, block_size as u32, block_size as u32)
            .unwrap();

        let res1 = bm.handle_v1_block_buffering(addr_0, &data_block_0);
        assert!(res1.is_none(), "First block should not trigger completion");

        let res2 = bm.handle_v1_block_buffering(addr_1, &data_block_1);
        assert!(
            res2.is_some(),
            "Second block SHOULD trigger completion and return data"
        );

        // In the old code, this would return Some(data) again, triggering a verification storm.
        let res3 = bm.handle_v1_block_buffering(addr_1, &data_block_1);

        assert!(
            res3.is_none(),
            "Duplicate block received after completion MUST return None to prevent double-verification"
        );
    }
}

#[cfg(test)]
mod comprehensive_tests {
    use crate::torrent_manager::block_manager::BlockManager;
    use std::collections::HashMap;

    fn create_manager(piece_len: u32, total_len: u64) -> BlockManager {
        let mut bm = BlockManager::new();
        bm.set_geometry(piece_len, total_len, vec![], vec![], HashMap::new(), false);
        bm
    }

    #[test]
    fn test_geometry_exact_alignment() {
        // Case: Total length is exactly 2 pieces, each exactly 2 blocks long.
        let piece_len = 32768; // 2 * 16384
        let total_len = 65536; // 2 * 32768
        let bm = create_manager(piece_len, total_len);

        assert_eq!(bm.total_blocks, 4);
        assert_eq!(bm.block_bitfield.len(), 4);

        // Check ranges
        assert_eq!(bm.get_block_range(0), (0, 2));
        assert_eq!(bm.get_block_range(1), (2, 4));
        // Out of bounds piece should return (total, total)
        assert_eq!(bm.get_block_range(2), (4, 4));
    }

    #[test]
    fn test_geometry_tiny_remainder() {
        // Case: 1 full piece + 1 byte remainder
        let piece_len = 16384;
        let total_len = 16385;
        let bm = create_manager(piece_len, total_len);

        assert_eq!(bm.total_blocks, 2);

        // Piece 0: 1 full block
        let (s0, e0) = bm.get_block_range(0);
        assert_eq!((s0, e0), (0, 1));

        // Piece 1: 1 block (partial)
        let (s1, e1) = bm.get_block_range(1);
        assert_eq!((s1, e1), (1, 2));

        // Check inflation of that last byte
        let addr = bm.inflate_address(1);
        assert_eq!(addr.length, 1);
        assert_eq!(addr.piece_index, 1);
    }

    #[test]
    fn test_geometry_partial_blocks_mid_stream() {
        // Case: Piece length is NOT a multiple of Block Size (rare but legal in V1)
        // Piece Len = 20000 (1 full block 16384 + partial 3616)
        // Total Len = 40000 (2 pieces)
        let piece_len = 20000;
        let total_len = 40000;
        let bm = create_manager(piece_len, total_len);

        // Piece 0: Blocks 0 and 1.
        // Block 0 is full (0-16384). Block 1 is partial (16384-20000).
        // BUT: BlockManager aligns strictly to 16k grid globally.
        // Let's verify how get_block_range handles this.

        // Piece 0 spans bytes 0..20000.
        // Block 0: 0..16384
        // Block 1: 16384..32768 (Piece 0 ends at 20000, so it uses part of Block 1)

        let (s0, e0) = bm.get_block_range(0);
        // Start block 0, End block 2 (covers indices 0, 1)
        assert_eq!((s0, e0), (0, 2));

        // Piece 1 spans bytes 20000..40000.
        // Starts in Block 1 (offset 3616 inside block).
        // Ends in Block 2 (32768..49152).

        let (s1, e1) = bm.get_block_range(1);
        // Should include Block 1 and Block 2.
        assert_eq!((s1, e1), (1, 3));
    }
}

#[cfg(test)]
mod security_tests {
    use crate::torrent_manager::block_manager::BlockManager;
    use std::collections::HashMap;

    fn create_manager(piece_len: u32, total_len: u64) -> BlockManager {
        let mut bm = BlockManager::new();
        bm.set_geometry(piece_len, total_len, vec![], vec![], HashMap::new(), false);
        bm
    }

    #[test]
    fn test_inflate_address_overflow_protection() {
        let piece_len = 32768;
        let total_len = 65536;
        let bm = create_manager(piece_len, total_len);

        // Offset 32760, length 10 (Sums to 32770 > 32768)
        let res = bm.inflate_address_from_overlay(0, 32760, 10);
        assert!(
            res.is_none(),
            "Should reject block extending past piece boundary"
        );

        let res = bm.inflate_address_from_overlay(0, 0, u32::MAX);
        assert!(res.is_none(), "Should reject length > piece size");

        let res = bm.inflate_address_from_overlay(0, 32767, 1);
        assert!(res.is_some(), "Should accept last byte of piece");
    }

    #[test]
    fn test_duplicate_block_handling() {
        let piece_len = 16384;
        let total_len = 16384;
        let mut bm = create_manager(piece_len, total_len);

        let data = vec![1u8; 16384];
        let addr = bm.inflate_address_from_overlay(0, 0, 16384).unwrap();

        let res1 = bm.handle_v1_block_buffering(addr, &data);
        assert!(res1.is_some()); // Completes the piece immediately
        bm.commit_v1_piece(0); // Mark globally done

        // inflate_address might succeed, but processing should handle logic

        // We simulate logic in PieceManager: check bitfield first
        if bm.block_bitfield[0] {
            // Logic handles it
        }

        // Test low-level buffering refusal if mask is set
        // Reset buffer state manually to simulate a race where assembler exists
        // but piece is already done globally.
        let addr_dup = bm.inflate_address_from_overlay(0, 0, 16384).unwrap();

        // If we try to handle it again:
        // handle_v1_block_buffering creates a new assembler if one doesn't exist.
        // It returns data. This is "correct" for V1 (idempotent),
        // but verify it doesn't crash or corrupt state.
        let res2 = bm.handle_v1_block_buffering(addr_dup, &data);
        assert!(res2.is_some());
    }
}

#[cfg(test)]
mod state_tests {
    use crate::torrent_manager::block_manager::BlockManager;
    use std::collections::HashMap;

    #[test]
    fn test_revert_piece_clears_bits() {
        let mut bm = BlockManager::new();
        let piece_len = 32768; // 2 blocks
        let total_len = 32768;
        bm.set_geometry(piece_len, total_len, vec![], vec![], HashMap::new(), false);

        bm.commit_v1_piece(0);
        assert!(bm.block_bitfield[0]);
        assert!(bm.block_bitfield[1]);
        assert!(bm.legacy_buffers.is_empty());

        bm.revert_v1_piece_completion(0);
        assert!(!bm.block_bitfield[0], "Block 0 bit not cleared");
        assert!(!bm.block_bitfield[1], "Block 1 bit not cleared");

        let data = vec![0u8; 16384];
        let addr = bm.inflate_address_from_overlay(0, 0, 16384).unwrap();
        let res = bm.handle_v1_block_buffering(addr, &data);
        assert!(res.is_none()); // Buffered 1/2 blocks

        let assembler = bm.legacy_buffers.get(&0).unwrap();
        assert_eq!(assembler.received_blocks, 1);
    }
}

#[cfg(test)]
mod selection_tests {
    use crate::torrent_manager::block_manager::BlockManager;
    use std::collections::HashMap;

    #[test]
    fn test_pick_blocks_standard_vs_endgame() {
        let mut bm = BlockManager::new();
        // 1 piece, 4 blocks
        let piece_len = 16384 * 4;
        bm.set_geometry(
            piece_len,
            piece_len as u64,
            vec![],
            vec![],
            HashMap::new(),
            false,
        );

        let peer_bitfield = vec![true]; // Peer has Piece 0
        let rarest = vec![0];

        bm.mark_pending(0);

        // Standard Mode: Should skip Block 0, pick Block 1
        let picks_std = bm.pick_blocks_for_peer(&peer_bitfield, 1, &rarest, false);
        assert_eq!(picks_std.len(), 1);
        assert_eq!(picks_std[0].block_index, 1); // Skips 0

        // Endgame Mode: Should duplicate Block 0 if needed, or pick others.
        // Our logic: pick unacquired blocks. If unacquired is pending,
        // skip in standard, take in endgame.

        let picks_endgame = bm.pick_blocks_for_peer(&peer_bitfield, 5, &rarest, true);

        // Should define behavior:
        // Current impl iterates: 0 (Pending), 1 (Pending-ish/Available), 2, 3
        // If logic is correct, it returns all 4 blocks including pending ones.

        let has_block_0 = picks_endgame.iter().any(|b| b.block_index == 0);
        assert!(has_block_0, "Endgame should pick pending blocks");
    }
}
