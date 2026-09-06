// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application visualization model definitions and transitions.

use super::*;

#[derive(Debug, Clone, Default)]
pub struct DhtWaveUiState {
    pub phase: f64,
    pub amplitude: f64,
    pub harmonic_amplitude: f64,
    pub frequency: f64,
    pub phase_speed: f64,
    pub crest_bias: f64,
    pub bootstrap_ratio: f64,
    pub discovery_boost: f64,
    pub query_load: f64,
    pub query_surge: f64,
    pub initialized: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum VisualizationFocusPanel {
    TorrentList,
    TorrentDetails,
    PeerFiles,
    #[default]
    Chart,
    PeerStream,
    BlockStream,
    DhtWave,
    DiskHealth,
    Statistics,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerStreamVisualization {
    #[default]
    #[serde(alias = "prism_split")]
    Classic,
    HelixExchange,
}

impl PeerStreamVisualization {
    pub const ALL: [Self; 2] = [Self::Classic, Self::HelixExchange];

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskHealthVisualization {
    #[default]
    #[serde(
        alias = "io_braid",
        alias = "pressure_fan",
        alias = "load_balance",
        alias = "track_drum",
        alias = "disk_platter",
        alias = "spool_drive",
        alias = "head_sweep",
        alias = "read_head",
        alias = "sector_compass",
        alias = "sector_rack",
        alias = "sector_fan",
        alias = "io_crankshaft",
        alias = "io_equalizer",
        alias = "io_spindle",
        alias = "queue_escapement",
        alias = "queue_conveyor",
        alias = "queue_stack",
        alias = "cache_membrane",
        alias = "cache_reservoir",
        alias = "throughput_rails",
        alias = "pressure_clamp",
        alias = "pressure_bellows",
        alias = "pressure_gauge",
        alias = "block_press",
        alias = "block_well",
        alias = "block_cascade",
        alias = "transfer_pulley",
        alias = "transfer_seismograph",
        alias = "seek_radar",
        alias = "write_stamp",
        alias = "write_anvil",
        alias = "write_fountain",
        alias = "read_fork",
        alias = "read_calipers",
        alias = "read_ribbon",
        alias = "buffer_carousel",
        alias = "buffer_capsules",
        alias = "latency_canyon",
        alias = "flush_sluice",
        alias = "flush_chute",
        alias = "buffer_tower",
        alias = "latency_metronome",
        alias = "latency_spring",
        alias = "transfer_bridge",
        alias = "transfer_cam",
        alias = "transfer_ratchet",
        alias = "load_prism",
        alias = "wear_micrometer",
        alias = "wear_strata",
        alias = "flush_vortex",
        alias = "load_governor",
        alias = "load_piston",
        alias = "sector_bloom",
        alias = "head_shuttle",
        alias = "head_comb",
        alias = "head_ladder"
    )]
    Classic,
    #[serde(alias = "cache_lattice")]
    SeekPendulum,
    #[serde(alias = "circuit_board")]
    StorageDial,
}

impl DiskHealthVisualization {
    pub const ALL: [Self; 3] = [Self::Classic, Self::SeekPendulum, Self::StorageDial];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::SeekPendulum => "Seek Pendulum",
            Self::StorageDial => "Storage Dial",
        }
    }

    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::SeekPendulum => "Pendulum",
            Self::StorageDial => "Dial",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DhtVisualization {
    #[default]
    #[serde(
        alias = "lookup_core",
        alias = "signal_spire",
        alias = "routing_loom",
        alias = "search_beacon",
        alias = "peer_constellation",
        alias = "hash_cascade",
        alias = "query_canyon",
        alias = "packet_lantern",
        alias = "yield_fountain",
        alias = "bootstrap_bridge",
        alias = "demand_prism",
        alias = "signal_ladder",
        alias = "routing_crown",
        alias = "query_hourglass",
        alias = "yield_delta",
        alias = "hash_circuit",
        alias = "query_tide",
        alias = "node_web",
        alias = "query_pulse",
        alias = "query_wings"
    )]
    Classic,
    RelayRibbon,
    PulseGrid,
    LookupVortex,
    PeerBloom,
}

impl DhtVisualization {
    pub const ALL: [Self; 5] = [
        Self::Classic,
        Self::RelayRibbon,
        Self::PulseGrid,
        Self::LookupVortex,
        Self::PeerBloom,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::RelayRibbon => "Relay Ribbon",
            Self::PulseGrid => "Pulse Grid",
            Self::LookupVortex => "Lookup Vortex",
            Self::PeerBloom => "Peer Bloom",
        }
    }

    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::RelayRibbon => "Ribbon",
            Self::PulseGrid => "Grid",
            Self::LookupVortex => "Vortex",
            Self::PeerBloom => "Bloom",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|view| *view == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisualizationFocusState {
    pub active: bool,
    pub selected: VisualizationFocusPanel,
    pub peer_stream: PeerStreamVisualization,
    pub disk_health: DiskHealthVisualization,
    pub dht: DhtVisualization,
}

impl VisualizationFocusState {
    pub(super) fn from_settings(settings: &Settings) -> Self {
        Self {
            peer_stream: settings.peer_stream_visualization,
            disk_health: settings.disk_health_visualization,
            dht: settings.dht_visualization,
            ..Self::default()
        }
    }

    pub(super) fn apply_settings(&mut self, settings: &Settings) {
        self.peer_stream = settings.peer_stream_visualization;
        self.disk_health = settings.disk_health_visualization;
        self.dht = settings.dht_visualization;
    }
}
