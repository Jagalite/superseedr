// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::types::{InfoHash, NodeId};

pub fn seeded_node_id(seed: u8) -> NodeId {
    let mut bytes = [0u8; 20];
    for (idx, byte) in bytes.iter_mut().enumerate() {
        *byte = seed.wrapping_add(idx as u8);
    }
    NodeId::from(bytes)
}

pub fn seeded_info_hash(seed: u8) -> InfoHash {
    let mut bytes = [0u8; 20];
    for (idx, byte) in bytes.iter_mut().enumerate() {
        *byte = seed.wrapping_add((idx as u8).wrapping_mul(3));
    }
    InfoHash::from(bytes)
}
