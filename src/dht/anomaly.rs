// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#[derive(Debug, Clone)]
pub struct AnomalyConfig {
    pub max_nodes_per_prefix: usize,
    pub max_dead_referral_rate_percent: u8,
}

impl Default for AnomalyConfig {
    fn default() -> Self {
        Self {
            max_nodes_per_prefix: 8,
            max_dead_referral_rate_percent: 50,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReferralQuality {
    pub reported: u32,
    pub reachable: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnomalyScore {
    pub node_id_churn: u32,
    pub dead_referrals: u32,
    pub malformed_replies: u32,
}
