// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub bootstrap_nodes: Vec<SocketAddr>,
    pub refresh_interval: Duration,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            bootstrap_nodes: Vec::new(),
            refresh_interval: Duration::from_secs(900),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BootstrapCoordinator {
    config: BootstrapConfig,
}

impl BootstrapCoordinator {
    pub fn new(config: BootstrapConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &BootstrapConfig {
        &self.config
    }
}
