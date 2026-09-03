// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Declarative, browser-owned scenario catalog for the simulated torrent service.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScenarioId {
    Downloading,
    Seeding,
    #[default]
    Mixed,
    Swarm,
    MissingPieces,
    DiskPressure,
    DiskError,
    Recovery,
}

impl ScenarioId {
    #[cfg(test)]
    pub const ALL: [Self; 8] = [
        Self::Downloading,
        Self::Seeding,
        Self::Mixed,
        Self::Swarm,
        Self::MissingPieces,
        Self::DiskPressure,
        Self::DiskError,
        Self::Recovery,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "downloading" => Self::Downloading,
            "seeding" => Self::Seeding,
            "mixed" => Self::Mixed,
            "swarm" => Self::Swarm,
            "missing-pieces" => Self::MissingPieces,
            "disk-pressure" => Self::DiskPressure,
            "disk-error" => Self::DiskError,
            "recovery" => Self::Recovery,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Downloading => "downloading",
            Self::Seeding => "seeding",
            Self::Mixed => "mixed",
            Self::Swarm => "swarm",
            Self::MissingPieces => "missing-pieces",
            Self::DiskPressure => "disk-pressure",
            Self::DiskError => "disk-error",
            Self::Recovery => "recovery",
        }
    }

    pub fn preset(self) -> &'static ScenarioPreset {
        match self {
            Self::Downloading => &DOWNLOADING,
            Self::Seeding => &SEEDING,
            Self::Mixed => &MIXED,
            Self::Swarm => &SWARM,
            Self::MissingPieces => &MISSING_PIECES,
            Self::DiskPressure => &DISK_PRESSURE,
            Self::DiskError => &DISK_ERROR,
            Self::Recovery => &RECOVERY,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialPhase {
    Metadata,
    Peers,
    Downloading,
    Checking,
    Seeding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvailabilityPreset {
    Normal,
    MissingUntil { pieces: u8, peer_arrival_tick: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskPreset {
    Normal,
    PressureUntil {
        recovery_tick: u16,
    },
    ErrorThenRecover {
        error_until_tick: u16,
        healthy_at_tick: u16,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct SessionPreset {
    pub hash_byte: u8,
    pub name: &'static str,
    pub phase: InitialPhase,
    pub progress_percent: u8,
    pub phase_elapsed_ticks: u16,
    pub peer_goal: u8,
    pub rate_percent: u16,
    pub uploaded_percent: u16,
    pub availability: AvailabilityPreset,
    pub disk: DiskPreset,
}

#[derive(Clone, Copy, Debug)]
pub struct JournalPreset {
    pub timestamp: &'static str,
    pub torrent_name: &'static str,
    pub message: &'static str,
    pub kind: JournalKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalKind {
    IngestAdded,
    TorrentCompleted,
    DataUnavailable,
    DataRecovered,
}

#[derive(Clone, Copy, Debug)]
pub struct ScenarioPreset {
    pub sessions: &'static [SessionPreset],
    pub journal: &'static [JournalPreset],
}

const fn session(
    hash_byte: u8,
    name: &'static str,
    phase: InitialPhase,
    progress_percent: u8,
    peer_goal: u8,
) -> SessionPreset {
    SessionPreset {
        hash_byte,
        name,
        phase,
        progress_percent,
        phase_elapsed_ticks: 0,
        peer_goal,
        rate_percent: 100,
        uploaded_percent: 7,
        availability: AvailabilityPreset::Normal,
        disk: DiskPreset::Normal,
    }
}

const DOWNLOADING_SESSIONS: [SessionPreset; 3] = [
    SessionPreset {
        phase_elapsed_ticks: 2,
        rate_percent: 75,
        ..session(0x5a, "Amber Delta", InitialPhase::Downloading, 18, 4)
    },
    SessionPreset {
        phase_elapsed_ticks: 6,
        rate_percent: 100,
        ..session(0x22, "Quiet Meridian", InitialPhase::Downloading, 46, 6)
    },
    SessionPreset {
        phase_elapsed_ticks: 9,
        rate_percent: 125,
        ..session(0x23, "Granite Lantern", InitialPhase::Downloading, 72, 8)
    },
];

const SEEDING_SESSIONS: [SessionPreset; 3] = [
    SessionPreset {
        uploaded_percent: 35,
        ..session(0x5a, "Velvet Current", InitialPhase::Seeding, 100, 5)
    },
    SessionPreset {
        uploaded_percent: 110,
        rate_percent: 125,
        ..session(0x32, "Silver Canopy", InitialPhase::Seeding, 100, 7)
    },
    SessionPreset {
        uploaded_percent: 240,
        rate_percent: 80,
        ..session(0x33, "Cobalt Meadow", InitialPhase::Seeding, 100, 4)
    },
];

const MIXED_SESSIONS: [SessionPreset; 15] = [
    SessionPreset {
        rate_percent: 45,
        ..session(
            0x5a,
            "Nebula Noodle 12.4 x86_64.iso",
            InitialPhase::Metadata,
            0,
            4,
        )
    },
    SessionPreset {
        phase_elapsed_ticks: 4,
        rate_percent: 240,
        ..session(
            0x6b,
            "Kernel Kettle 4.2 x86_64.iso",
            InitialPhase::Downloading,
            36,
            6,
        )
    },
    SessionPreset {
        phase_elapsed_ticks: 15,
        rate_percent: 30,
        ..session(
            0x7c,
            "Sudo Sandwich 9.1 x86_64.iso",
            InitialPhase::Downloading,
            44,
            5,
        )
    },
    SessionPreset {
        rate_percent: 155,
        ..session(
            0xbe,
            "Recursive Raccoon 3.7 x86_64.iso",
            InitialPhase::Downloading,
            58,
            4,
        )
    },
    SessionPreset {
        rate_percent: 70,
        ..session(0x91, "Packet Yak 7.3 x86_64.iso", InitialPhase::Peers, 0, 5)
    },
    SessionPreset {
        rate_percent: 85,
        ..session(
            0x8d,
            "Initramfs After Dark 8.0 x86_64.iso",
            InitialPhase::Checking,
            100,
            6,
        )
    },
    SessionPreset {
        uploaded_percent: 84,
        rate_percent: 65,
        ..session(
            0x9e,
            "Segfault Sorbet 2.6 x86_64.iso",
            InitialPhase::Seeding,
            100,
            7,
        )
    },
    SessionPreset {
        phase_elapsed_ticks: 8,
        rate_percent: 18,
        ..session(
            0xa1,
            "Bashful Badger 6.4 x86_64.iso",
            InitialPhase::Downloading,
            12,
            3,
        )
    },
    SessionPreset {
        phase_elapsed_ticks: 13,
        rate_percent: 290,
        ..session(
            0xa2,
            "Daemon Dumpling 1.8 x86_64.iso",
            InitialPhase::Downloading,
            29,
            9,
        )
    },
    SessionPreset {
        phase_elapsed_ticks: 2,
        rate_percent: 52,
        ..session(
            0xa3,
            "TTY Tiramisu 5.5 x86_64.iso",
            InitialPhase::Downloading,
            63,
            5,
        )
    },
    SessionPreset {
        phase_elapsed_ticks: 11,
        rate_percent: 205,
        ..session(
            0xa4,
            "Fork Bomb Fondue 3.3 x86_64.iso",
            InitialPhase::Downloading,
            78,
            7,
        )
    },
    SessionPreset {
        phase_elapsed_ticks: 5,
        rate_percent: 40,
        ..session(
            0xa5,
            "Rootless Turnip 7.1 x86_64.iso",
            InitialPhase::Checking,
            100,
            4,
        )
    },
    SessionPreset {
        uploaded_percent: 180,
        rate_percent: 230,
        ..session(
            0xa6,
            "Pipe Dream Pudding 4.8 x86_64.iso",
            InitialPhase::Seeding,
            100,
            9,
        )
    },
    SessionPreset {
        uploaded_percent: 42,
        rate_percent: 25,
        ..session(
            0xa7,
            "Mutex Marmalade 2.9 x86_64.iso",
            InitialPhase::Seeding,
            100,
            3,
        )
    },
    SessionPreset {
        phase_elapsed_ticks: 7,
        rate_percent: 90,
        ..session(
            0xa8,
            "Socket Souffle 10.2 x86_64.iso",
            InitialPhase::Downloading,
            52,
            6,
        )
    },
];

const SWARM_SESSIONS: [SessionPreset; 3] = [
    SessionPreset {
        phase_elapsed_ticks: 4,
        rate_percent: 110,
        ..session(0x5a, "Dawn Relay", InitialPhase::Downloading, 22, 16)
    },
    SessionPreset {
        phase_elapsed_ticks: 9,
        rate_percent: 130,
        ..session(0x52, "Mosaic Passage", InitialPhase::Downloading, 51, 20)
    },
    SessionPreset {
        phase_elapsed_ticks: 3,
        ..session(0x53, "Harbor Geometry", InitialPhase::Peers, 0, 18)
    },
];

const MISSING_PIECE_SESSIONS: [SessionPreset; 1] = [SessionPreset {
    phase_elapsed_ticks: 4,
    rate_percent: 120,
    availability: AvailabilityPreset::MissingUntil {
        pieces: 4,
        peer_arrival_tick: 28,
    },
    ..session(
        0x5a,
        "Incomplete Constellation",
        InitialPhase::Downloading,
        82,
        5,
    )
}];

const DISK_PRESSURE_SESSIONS: [SessionPreset; 2] = [
    SessionPreset {
        phase_elapsed_ticks: 4,
        rate_percent: 90,
        disk: DiskPreset::PressureUntil { recovery_tick: 28 },
        ..session(0x5a, "Stonework Queue", InitialPhase::Downloading, 34, 6)
    },
    SessionPreset {
        phase_elapsed_ticks: 7,
        rate_percent: 70,
        disk: DiskPreset::PressureUntil { recovery_tick: 36 },
        ..session(0x72, "Copper Footpath", InitialPhase::Downloading, 61, 5)
    },
];

const DISK_ERROR_SESSIONS: [SessionPreset; 1] = [SessionPreset {
    phase_elapsed_ticks: 5,
    disk: DiskPreset::ErrorThenRecover {
        error_until_tick: 20,
        healthy_at_tick: 34,
    },
    ..session(0x5a, "Interrupted Atlas", InitialPhase::Downloading, 68, 6)
}];

const RECOVERY_SESSIONS: [SessionPreset; 2] = [
    SessionPreset {
        phase_elapsed_ticks: 6,
        rate_percent: 115,
        availability: AvailabilityPreset::MissingUntil {
            pieces: 3,
            peer_arrival_tick: 18,
        },
        ..session(0x5a, "Returning Horizon", InitialPhase::Downloading, 76, 4)
    },
    SessionPreset {
        phase_elapsed_ticks: 3,
        disk: DiskPreset::ErrorThenRecover {
            error_until_tick: 14,
            healthy_at_tick: 26,
        },
        ..session(0x92, "Restored Orchard", InitialPhase::Downloading, 54, 5)
    },
];

const STANDARD_JOURNAL: [JournalPreset; 2] = [
    JournalPreset {
        timestamp: "2026-08-30T12:00:00Z",
        torrent_name: "Signal Garden",
        message: "Simulated metadata resolved",
        kind: JournalKind::IngestAdded,
    },
    JournalPreset {
        timestamp: "2026-08-30T12:03:00Z",
        torrent_name: "Prism Notes",
        message: "Simulated torrent completed",
        kind: JournalKind::TorrentCompleted,
    },
];

const MISSING_JOURNAL: [JournalPreset; 2] = [
    JournalPreset {
        timestamp: "2026-08-30T12:10:00Z",
        torrent_name: "Incomplete Constellation",
        message: "Simulated swarm is missing four pieces",
        kind: JournalKind::DataUnavailable,
    },
    JournalPreset {
        timestamp: "2026-08-30T12:10:03Z",
        torrent_name: "Incomplete Constellation",
        message: "Scheduled simulated peer can supply the missing pieces",
        kind: JournalKind::DataRecovered,
    },
];

const PRESSURE_JOURNAL: [JournalPreset; 2] = [
    JournalPreset {
        timestamp: "2026-08-30T12:20:00Z",
        torrent_name: "Stonework Queue",
        message: "Simulated disk pressure enabled bounded write backoff",
        kind: JournalKind::DataUnavailable,
    },
    JournalPreset {
        timestamp: "2026-08-30T12:20:04Z",
        torrent_name: "Copper Footpath",
        message: "Scheduled disk pressure recovery is pending",
        kind: JournalKind::DataRecovered,
    },
];

const ERROR_JOURNAL: [JournalPreset; 2] = [
    JournalPreset {
        timestamp: "2026-08-30T12:29:59Z",
        torrent_name: "Interrupted Atlas",
        message: "Scheduled simulated disk recovery will retry safely",
        kind: JournalKind::DataRecovered,
    },
    JournalPreset {
        timestamp: "2026-08-30T12:30:00Z",
        torrent_name: "Interrupted Atlas",
        message: "Simulated disk write error paused piece persistence",
        kind: JournalKind::DataUnavailable,
    },
];

const RECOVERY_JOURNAL: [JournalPreset; 2] = [
    JournalPreset {
        timestamp: "2026-08-30T12:40:00Z",
        torrent_name: "Returning Horizon",
        message: "Simulated peer and disk failures entered recovery",
        kind: JournalKind::DataUnavailable,
    },
    JournalPreset {
        timestamp: "2026-08-30T12:40:03Z",
        torrent_name: "Restored Orchard",
        message: "Deterministic recovery restored healthy transfer paths",
        kind: JournalKind::DataRecovered,
    },
];

const DOWNLOADING: ScenarioPreset = ScenarioPreset {
    sessions: &DOWNLOADING_SESSIONS,
    journal: &STANDARD_JOURNAL,
};
const SEEDING: ScenarioPreset = ScenarioPreset {
    sessions: &SEEDING_SESSIONS,
    journal: &STANDARD_JOURNAL,
};
const MIXED: ScenarioPreset = ScenarioPreset {
    sessions: &MIXED_SESSIONS,
    journal: &STANDARD_JOURNAL,
};
const SWARM: ScenarioPreset = ScenarioPreset {
    sessions: &SWARM_SESSIONS,
    journal: &STANDARD_JOURNAL,
};
const MISSING_PIECES: ScenarioPreset = ScenarioPreset {
    sessions: &MISSING_PIECE_SESSIONS,
    journal: &MISSING_JOURNAL,
};
const DISK_PRESSURE: ScenarioPreset = ScenarioPreset {
    sessions: &DISK_PRESSURE_SESSIONS,
    journal: &PRESSURE_JOURNAL,
};
const DISK_ERROR: ScenarioPreset = ScenarioPreset {
    sessions: &DISK_ERROR_SESSIONS,
    journal: &ERROR_JOURNAL,
};
const RECOVERY: ScenarioPreset = ScenarioPreset {
    sessions: &RECOVERY_SESSIONS,
    journal: &RECOVERY_JOURNAL,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_fixture_text_is_declarative() {
        for id in ScenarioId::ALL {
            assert_eq!(ScenarioId::from_name(id.name()), Some(id));
            assert!(!id.preset().sessions.is_empty());
            assert!(!id.preset().journal.is_empty());
        }
        assert!(ScenarioId::from_name("unknown").is_none());
    }
}
