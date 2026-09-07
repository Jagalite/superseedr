// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application resource model definitions and transitions.

use super::*;

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct CalculatedLimits {
    pub reserve_permits: usize,
    pub max_connected_peers: usize,
    pub disk_read_permits: usize,
    pub disk_write_permits: usize,
}
impl CalculatedLimits {
    pub fn into_map_with_peer_queue(
        self,
        peer_connection_queue_size: usize,
    ) -> HashMap<ResourceType, (usize, usize)> {
        let mut map = HashMap::new();
        map.insert(ResourceType::Reserve, (self.reserve_permits, 0));
        map.insert(
            ResourceType::PeerConnection,
            (self.max_connected_peers, peer_connection_queue_size),
        );
        map.insert(
            ResourceType::DiskRead,
            (
                self.disk_read_permits,
                self.disk_read_permits.saturating_mul(2),
            ),
        );
        map.insert(
            ResourceType::DiskWrite,
            (
                self.disk_write_permits,
                self.disk_write_permits.saturating_mul(2),
            ),
        );
        map
    }
}
