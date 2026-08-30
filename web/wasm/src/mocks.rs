// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser-owned deterministic torrent simulation and command fulfillment.

use std::{collections::HashMap, path::Path, path::PathBuf};

use superseedr::web_integration::{
    BrowserCommand, BrowserFileTreeEntry, BrowserFileUpdate, BrowserJournalKind,
    BrowserJournalUpdate, BrowserManagerEventUpdate, BrowserPeerUpdate, BrowserRssUpdate,
    BrowserRuntimeTelemetryUpdate, BrowserSession, BrowserTelemetryUpdate,
    BrowserTorrentControlState, BrowserTorrentUpdate,
};

use crate::scenarios::{
    AvailabilityPreset, DiskPreset, InitialControl, InitialPhase, JournalKind, ScenarioId,
    SessionPreset,
};

const METADATA_SECONDS: f64 = 0.6;
const PEER_DISCOVERY_SECONDS: f64 = 0.8;
const CHECKING_SECONDS: f64 = 0.7;
const FIXED_STEP_SECONDS: f64 = 0.1;
const FIXED_STEP_EPSILON: f64 = 1.0e-9;
// Keep the browser telemetry boundary consistent with the native torrent manager. Native derives
// instantaneous rates from interval byte counts and applies this time-aware EMA before publishing
// torrent and peer metrics.
const RATE_SAMPLE_INTERVAL_SECONDS: f64 = 1.0;
pub(crate) const RATE_SMOOTHING_PERIOD_SECONDS: f64 = 5.0;
const PUBLISH_INTERVAL_SECONDS: f64 = 0.1;
const HISTORY_LIMIT: usize = 120;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockTorrentPhase {
    FetchingMetadata,
    DiscoveringPeers,
    Downloading,
    CheckingPieces,
    Seeding,
}

impl MockTorrentPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::FetchingMetadata => "metadata",
            Self::DiscoveringPeers => "peers",
            Self::Downloading => "downloading",
            Self::CheckingPieces => "checking",
            Self::Seeding => "seeding",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockStall {
    Peer,
    Disk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockDiskState {
    Healthy,
    Pressure,
    Error,
    Recovering,
}

impl MockDiskState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Pressure => "pressure",
            Self::Error => "error",
            Self::Recovering => "recovering",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScenarioDiagnostics {
    pub name: &'static str,
    pub metadata: usize,
    pub peers: usize,
    pub downloading: usize,
    pub checking: usize,
    pub seeding: usize,
    pub paused: usize,
    pub deleting: usize,
    pub max_peers: usize,
    pub peer_rate_variants: usize,
    pub availability_levels: usize,
    pub piece_acquisitions: usize,
    pub missing_pieces: usize,
    pub disk_state: MockDiskState,
    pub warning: bool,
    pub recovered: bool,
}

impl MockStall {
    pub fn label(self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::Disk => "disk",
        }
    }
}

#[derive(Clone, Debug)]
struct MockTorrentSession {
    info_hash: Vec<u8>,
    name: String,
    source: String,
    phase: MockTorrentPhase,
    phase_elapsed: f64,
    seed: u64,
    peer_goal: usize,
    rate_percent: u16,
    availability: AvailabilityPreset,
    disk: DiskPreset,
    total_size: u64,
    pieces_total: u32,
    bytes_written: u64,
    session_downloaded: u64,
    session_uploaded: u64,
    fixed_tick: u64,
    download_rate_ema: f64,
    upload_rate_ema: f64,
    peer_download_rate_emas: Vec<f64>,
    peer_upload_rate_emas: Vec<f64>,
    rate_interval_elapsed: f64,
    download_rate_interval: f64,
    upload_rate_interval: f64,
    peer_download_rate_intervals: Vec<f64>,
    peer_upload_rate_intervals: Vec<f64>,
    last_download_rate_sample: u64,
    last_upload_rate_sample: u64,
    control_state: BrowserTorrentControlState,
    download_path: Option<PathBuf>,
    container_name: Option<String>,
    download_history: Vec<u64>,
    upload_history: Vec<u64>,
    blocks_in_history: Vec<u64>,
    blocks_out_history: Vec<u64>,
    peer_discovery_history: Vec<u64>,
    peer_connection_history: Vec<u64>,
    peer_disconnect_history: Vec<u64>,
    last_reported_peer_count: usize,
}

impl MockTorrentSession {
    fn new(
        info_hash: Vec<u8>,
        name: String,
        source: String,
        phase: MockTorrentPhase,
        progress: f64,
    ) -> Self {
        let seed = info_hash.iter().fold(0_u64, |value, byte| {
            value.wrapping_mul(33) + u64::from(*byte)
        });
        let total_size = (64 + seed % 24) * MIB;
        let pieces_total = 192 + (seed % 5) as u32 * 32;
        let bytes_written = (total_size as f64 * progress.clamp(0.0, 1.0)) as u64;
        Self {
            info_hash,
            name,
            source,
            phase,
            phase_elapsed: 0.0,
            seed,
            peer_goal: 4 + (seed % 4) as usize,
            rate_percent: 100,
            availability: AvailabilityPreset::Normal,
            disk: DiskPreset::Normal,
            total_size,
            pieces_total,
            bytes_written,
            session_downloaded: bytes_written,
            session_uploaded: bytes_written / 14,
            fixed_tick: 0,
            download_rate_ema: 0.0,
            upload_rate_ema: 0.0,
            peer_download_rate_emas: Vec::new(),
            peer_upload_rate_emas: Vec::new(),
            rate_interval_elapsed: 0.0,
            download_rate_interval: 0.0,
            upload_rate_interval: 0.0,
            peer_download_rate_intervals: Vec::new(),
            peer_upload_rate_intervals: Vec::new(),
            last_download_rate_sample: 0,
            last_upload_rate_sample: 0,
            control_state: BrowserTorrentControlState::Running,
            download_path: Some(PathBuf::from("/simulated/downloads")),
            container_name: None,
            download_history: vec![0],
            upload_history: vec![0],
            blocks_in_history: vec![0],
            blocks_out_history: vec![0],
            peer_discovery_history: vec![0],
            peer_connection_history: vec![0],
            peer_disconnect_history: vec![0],
            last_reported_peer_count: 0,
        }
    }

    fn from_preset(preset: SessionPreset) -> Self {
        let phase = match preset.phase {
            InitialPhase::Metadata => MockTorrentPhase::FetchingMetadata,
            InitialPhase::Peers => MockTorrentPhase::DiscoveringPeers,
            InitialPhase::Downloading => MockTorrentPhase::Downloading,
            InitialPhase::Checking => MockTorrentPhase::CheckingPieces,
            InitialPhase::Seeding => MockTorrentPhase::Seeding,
        };
        let mut torrent = Self::new(
            vec![preset.hash_byte; 20],
            preset.name.to_string(),
            format!("magnet:?xt=urn:btih:{}", hex_byte(preset.hash_byte)),
            phase,
            f64::from(preset.progress_percent) / 100.0,
        );
        torrent.phase_elapsed = f64::from(preset.phase_elapsed_ticks) * FIXED_STEP_SECONDS;
        torrent.peer_goal = usize::from(preset.peer_goal);
        torrent.rate_percent = preset.rate_percent;
        torrent.availability = preset.availability;
        torrent.disk = preset.disk;
        torrent.control_state = match preset.control {
            InitialControl::Running => BrowserTorrentControlState::Running,
            InitialControl::Paused => BrowserTorrentControlState::Paused,
            InitialControl::Deleting => BrowserTorrentControlState::Deleting,
        };
        torrent.session_uploaded = torrent
            .total_size
            .saturating_mul(u64::from(preset.uploaded_percent))
            / 100;
        torrent.download_path = Some(PathBuf::from(format!(
            "/simulated/downloads/set-{:x}",
            preset.hash_byte & 0x0f
        )));
        torrent.container_name = Some(format!("collection-{:x}", preset.hash_byte & 0x0f));
        torrent.initialize_rate_averages();
        torrent.seed_initial_histories();
        torrent
    }

    fn advance(&mut self, delta_seconds: f64) {
        if !matches!(self.control_state, BrowserTorrentControlState::Running) {
            return;
        }

        self.fixed_tick = self.fixed_tick.saturating_add(1);
        self.phase_elapsed += delta_seconds;
        match self.phase {
            MockTorrentPhase::FetchingMetadata if self.phase_elapsed >= METADATA_SECONDS => {
                self.phase = MockTorrentPhase::DiscoveringPeers;
                self.phase_elapsed = 0.0;
            }
            MockTorrentPhase::DiscoveringPeers if self.phase_elapsed >= PEER_DISCOVERY_SECONDS => {
                self.phase = MockTorrentPhase::Downloading;
                self.phase_elapsed = 0.0;
            }
            MockTorrentPhase::CheckingPieces if self.phase_elapsed >= CHECKING_SECONDS => {
                self.phase = MockTorrentPhase::Seeding;
                self.phase_elapsed = 0.0;
            }
            _ => {}
        }

        let raw_download_speed_bps = self.raw_download_speed_bps();
        let raw_upload_speed_bps = self.raw_upload_speed_bps();
        self.accumulate_rate_sample(
            delta_seconds,
            raw_download_speed_bps,
            raw_upload_speed_bps,
        );

        match self.phase {
            MockTorrentPhase::Downloading => {
                let downloaded = (raw_download_speed_bps as f64 * delta_seconds) as u64;
                let download_ceiling = self.download_ceiling();
                self.bytes_written = self
                    .bytes_written
                    .saturating_add(downloaded)
                    .min(download_ceiling);
                self.session_downloaded = self.session_downloaded.saturating_add(downloaded);
                self.session_uploaded = self
                    .session_uploaded
                    .saturating_add((raw_upload_speed_bps as f64 * delta_seconds) as u64);
                if self.bytes_written >= self.total_size && self.missing_piece_count() == 0 {
                    self.phase = MockTorrentPhase::CheckingPieces;
                    self.phase_elapsed = 0.0;
                }
            }
            MockTorrentPhase::Seeding => {
                self.session_uploaded = self
                    .session_uploaded
                    .saturating_add((raw_upload_speed_bps as f64 * delta_seconds) as u64);
            }
            _ => {}
        }
    }

    fn stall(&self) -> Option<MockStall> {
        if self.phase != MockTorrentPhase::Downloading
            || !matches!(self.control_state, BrowserTorrentControlState::Running)
        {
            return None;
        }
        let cycle = self.phase_elapsed.rem_euclid(4.8);
        if (1.3..1.75).contains(&cycle) {
            Some(MockStall::Peer)
        } else if (3.0..3.4).contains(&cycle) {
            Some(MockStall::Disk)
        } else {
            None
        }
    }

    fn raw_download_speed_bps(&self) -> u64 {
        if !matches!(self.control_state, BrowserTorrentControlState::Running)
            || self.phase != MockTorrentPhase::Downloading
            || self.stall().is_some()
            || self.disk_state() == MockDiskState::Error
        {
            return 0;
        }
        let wave = (self.fixed_tick + self.seed) % 7;
        let base = (18 + wave * 2) * MIB * u64::from(self.rate_percent) / 100;
        match self.disk_state() {
            MockDiskState::Pressure => base / 5,
            MockDiskState::Recovering => base / 2,
            MockDiskState::Healthy => base,
            MockDiskState::Error => 0,
        }
    }

    fn raw_upload_speed_bps(&self) -> u64 {
        if !matches!(self.control_state, BrowserTorrentControlState::Running)
            || self.disk_state() == MockDiskState::Error
        {
            return 0;
        }
        let base = match self.phase {
            MockTorrentPhase::Downloading if self.stall().is_none() => {
                (320 + (self.fixed_tick + self.seed) % 5 * 96) * 1024
            }
            MockTorrentPhase::Seeding => (2 + (self.fixed_tick + self.seed) % 3) * MIB,
            _ => 0,
        } * u64::from(self.rate_percent)
            / 100;
        match self.disk_state() {
            MockDiskState::Pressure => base / 3,
            MockDiskState::Recovering => base / 2,
            MockDiskState::Healthy => base,
            MockDiskState::Error => 0,
        }
    }

    fn download_speed_bps(&self) -> u64 {
        if matches!(self.control_state, BrowserTorrentControlState::Running) {
            self.download_rate_ema.max(0.0) as u64
        } else {
            0
        }
    }

    fn upload_speed_bps(&self) -> u64 {
        if matches!(self.control_state, BrowserTorrentControlState::Running) {
            self.upload_rate_ema.max(0.0) as u64
        } else {
            0
        }
    }

    fn initialize_rate_averages(&mut self) {
        let download_speed_bps = self.raw_download_speed_bps();
        let upload_speed_bps = self.raw_upload_speed_bps();
        self.download_rate_ema = download_speed_bps as f64;
        self.upload_rate_ema = upload_speed_bps as f64;
        self.last_download_rate_sample = download_speed_bps;
        self.last_upload_rate_sample = upload_speed_bps;
        self.set_peer_rate_averages(download_speed_bps, upload_speed_bps);
    }

    fn clear_rate_averages(&mut self) {
        self.download_rate_ema = 0.0;
        self.upload_rate_ema = 0.0;
        self.peer_download_rate_emas.fill(0.0);
        self.peer_upload_rate_emas.fill(0.0);
        self.rate_interval_elapsed = 0.0;
        self.download_rate_interval = 0.0;
        self.upload_rate_interval = 0.0;
        self.peer_download_rate_intervals.fill(0.0);
        self.peer_upload_rate_intervals.fill(0.0);
        self.last_download_rate_sample = 0;
        self.last_upload_rate_sample = 0;
    }

    fn accumulate_rate_sample(
        &mut self,
        delta_seconds: f64,
        raw_download_speed_bps: u64,
        raw_upload_speed_bps: u64,
    ) {
        let count = self.peer_count();
        let download_targets = weighted_shares(
            raw_download_speed_bps,
            &self.peer_rate_weights(count, 0x51),
        );
        let upload_targets =
            weighted_shares(raw_upload_speed_bps, &self.peer_rate_weights(count, 0xa7));
        let peer_capacity = self.peer_goal.saturating_add(1).max(count);
        self.peer_download_rate_emas.resize(peer_capacity, 0.0);
        self.peer_upload_rate_emas.resize(peer_capacity, 0.0);
        self.peer_download_rate_intervals
            .resize(peer_capacity, 0.0);
        self.peer_upload_rate_intervals
            .resize(peer_capacity, 0.0);
        self.rate_interval_elapsed += delta_seconds;
        self.download_rate_interval += raw_download_speed_bps as f64 * delta_seconds;
        self.upload_rate_interval += raw_upload_speed_bps as f64 * delta_seconds;
        for index in 0..peer_capacity {
            self.peer_download_rate_intervals[index] +=
                download_targets.get(index).copied().unwrap_or_default() as f64 * delta_seconds;
            self.peer_upload_rate_intervals[index] +=
                upload_targets.get(index).copied().unwrap_or_default() as f64 * delta_seconds;
        }
        if self.rate_interval_elapsed + FIXED_STEP_EPSILON < RATE_SAMPLE_INTERVAL_SECONDS {
            return;
        }

        let sample_seconds = self.rate_interval_elapsed;
        let instantaneous_download = self.download_rate_interval / sample_seconds;
        let instantaneous_upload = self.upload_rate_interval / sample_seconds;
        self.last_download_rate_sample = instantaneous_download as u64;
        self.last_upload_rate_sample = instantaneous_upload as u64;
        let alpha = 1.0 - (-sample_seconds / RATE_SMOOTHING_PERIOD_SECONDS).exp();
        self.download_rate_ema = update_ema(
            self.download_rate_ema,
            instantaneous_download,
            alpha,
        );
        self.upload_rate_ema = update_ema(self.upload_rate_ema, instantaneous_upload, alpha);
        for index in 0..peer_capacity {
            self.peer_download_rate_emas[index] = update_ema(
                self.peer_download_rate_emas[index],
                self.peer_download_rate_intervals[index] / sample_seconds,
                alpha,
            );
            self.peer_upload_rate_emas[index] = update_ema(
                self.peer_upload_rate_emas[index],
                self.peer_upload_rate_intervals[index] / sample_seconds,
                alpha,
            );
        }
        self.rate_interval_elapsed = 0.0;
        self.download_rate_interval = 0.0;
        self.upload_rate_interval = 0.0;
        self.peer_download_rate_intervals.fill(0.0);
        self.peer_upload_rate_intervals.fill(0.0);
    }

    fn set_peer_rate_averages(&mut self, download_speed_bps: u64, upload_speed_bps: u64) {
        let count = self.peer_count();
        self.peer_download_rate_emas = weighted_shares(
            download_speed_bps,
            &self.peer_rate_weights(count, 0x51),
        )
        .into_iter()
        .map(|rate| rate as f64)
        .collect();
        self.peer_upload_rate_emas =
            weighted_shares(upload_speed_bps, &self.peer_rate_weights(count, 0xa7))
                .into_iter()
                .map(|rate| rate as f64)
                .collect();
        let peer_capacity = self.peer_goal.saturating_add(1).max(count);
        self.peer_download_rate_emas.resize(peer_capacity, 0.0);
        self.peer_upload_rate_emas.resize(peer_capacity, 0.0);
        self.peer_download_rate_intervals = vec![0.0; peer_capacity];
        self.peer_upload_rate_intervals = vec![0.0; peer_capacity];
    }

    fn disk_rates(&self) -> (u64, u64) {
        match self.disk_state() {
            MockDiskState::Error => return (0, 0),
            MockDiskState::Pressure => {
                let speed = self.download_speed_bps();
                return (speed / 2, speed / 4);
            }
            MockDiskState::Recovering => {
                let speed = self.download_speed_bps();
                return (speed / 3, speed / 2);
            }
            MockDiskState::Healthy => {}
        }
        match self.phase {
            MockTorrentPhase::Downloading if self.stall() != Some(MockStall::Disk) => (
                self.download_speed_bps() / 5,
                self.download_speed_bps() * 9 / 10,
            ),
            MockTorrentPhase::CheckingPieces => (12 * MIB, 2 * MIB),
            MockTorrentPhase::Seeding => (self.upload_speed_bps(), 0),
            _ => (0, 0),
        }
    }

    fn peer_count(&self) -> usize {
        if matches!(self.control_state, BrowserTorrentControlState::Deleting) {
            return 0;
        }
        let base = match self.phase {
            MockTorrentPhase::FetchingMetadata => 0,
            MockTorrentPhase::DiscoveringPeers => ((self.phase_elapsed / PEER_DISCOVERY_SECONDS
                * self.peer_goal as f64)
                .ceil() as usize)
                .min(self.peer_goal),
            MockTorrentPhase::Downloading if self.stall() == Some(MockStall::Peer) => 1,
            MockTorrentPhase::Downloading
            | MockTorrentPhase::CheckingPieces
            | MockTorrentPhase::Seeding => self.peer_goal,
        };
        if matches!(self.availability, AvailabilityPreset::MissingUntil { .. })
            && self.missing_piece_count() == 0
            && !matches!(
                self.phase,
                MockTorrentPhase::FetchingMetadata | MockTorrentPhase::DiscoveringPeers
            )
        {
            base.max(self.peer_goal).saturating_add(1)
        } else {
            base
        }
    }

    fn missing_piece_count(&self) -> usize {
        match self.availability {
            AvailabilityPreset::Normal => 0,
            AvailabilityPreset::MissingUntil {
                pieces,
                peer_arrival_tick,
            } if self.fixed_tick < u64::from(peer_arrival_tick) => usize::from(pieces),
            AvailabilityPreset::MissingUntil { .. } => 0,
        }
    }

    fn download_ceiling(&self) -> u64 {
        let missing = self.missing_piece_count() as u64;
        if missing == 0 {
            return self.total_size;
        }
        let piece_size = self
            .total_size
            .div_ceil(u64::from(self.pieces_total).max(1));
        self.total_size
            .saturating_sub(piece_size.saturating_mul(missing))
    }

    fn disk_state(&self) -> MockDiskState {
        match self.disk {
            DiskPreset::Normal => MockDiskState::Healthy,
            DiskPreset::PressureUntil { recovery_tick }
                if self.fixed_tick < u64::from(recovery_tick) =>
            {
                MockDiskState::Pressure
            }
            DiskPreset::PressureUntil { .. } => MockDiskState::Healthy,
            DiskPreset::ErrorThenRecover {
                error_until_tick, ..
            } if self.fixed_tick < u64::from(error_until_tick) => MockDiskState::Error,
            DiskPreset::ErrorThenRecover {
                healthy_at_tick, ..
            } if self.fixed_tick < u64::from(healthy_at_tick) => MockDiskState::Recovering,
            DiskPreset::ErrorThenRecover { .. } => MockDiskState::Healthy,
        }
    }

    fn has_recovered(&self) -> bool {
        match self.disk {
            DiskPreset::Normal => {
                matches!(self.availability, AvailabilityPreset::MissingUntil { .. })
                    && self.missing_piece_count() == 0
            }
            DiskPreset::PressureUntil { .. } | DiskPreset::ErrorThenRecover { .. } => {
                self.disk_state() == MockDiskState::Healthy
            }
        }
    }

    fn metadata_available(&self) -> bool {
        self.phase != MockTorrentPhase::FetchingMetadata
    }

    fn pieces_completed(&self) -> u32 {
        match self.phase {
            MockTorrentPhase::FetchingMetadata | MockTorrentPhase::DiscoveringPeers => 0,
            MockTorrentPhase::Downloading => {
                ((self.bytes_written as u128 * u128::from(self.pieces_total)
                    / u128::from(self.total_size)) as u32)
                    .min(self.pieces_total)
            }
            MockTorrentPhase::CheckingPieces | MockTorrentPhase::Seeding => self.pieces_total,
        }
    }

    fn activity(&self) -> String {
        if matches!(self.control_state, BrowserTorrentControlState::Paused) {
            return format!("Paused during simulated {}", self.phase.label());
        }
        if matches!(self.control_state, BrowserTorrentControlState::Deleting) {
            return "Removing simulated torrent".to_string();
        }
        match self.disk_state() {
            MockDiskState::Error => {
                return "Simulated disk error; piece persistence is paused".to_string();
            }
            MockDiskState::Pressure => {
                return "Simulated disk pressure; writes are throttled".to_string();
            }
            MockDiskState::Recovering => {
                return "Recovering simulated disk writes".to_string();
            }
            MockDiskState::Healthy => {}
        }
        if self.missing_piece_count() > 0 {
            return format!(
                "Waiting for {} simulated missing pieces",
                self.missing_piece_count()
            );
        }
        if let Some(stall) = self.stall() {
            return match stall {
                MockStall::Peer => "Peer swarm stalled; retrying deterministically".to_string(),
                MockStall::Disk => "Disk backoff; buffering simulated pieces".to_string(),
            };
        }
        match self.phase {
            MockTorrentPhase::FetchingMetadata => "Discovering simulated metadata".to_string(),
            MockTorrentPhase::DiscoveringPeers => "Discovering simulated peers".to_string(),
            MockTorrentPhase::Downloading => "Downloading simulated pieces".to_string(),
            MockTorrentPhase::CheckingPieces => "Checking simulated pieces".to_string(),
            MockTorrentPhase::Seeding => "Seeding simulated data".to_string(),
        }
    }

    fn peers(&self) -> Vec<BrowserPeerUpdate> {
        let count = self.peer_count();
        let download_speed = self.download_speed_bps();
        let upload_speed = self.upload_speed_bps();
        let lifetime_download_weights = self.peer_lifetime_weights(count, 0x2d);
        let lifetime_upload_weights = self.peer_lifetime_weights(count, 0xc3);
        let download_shares = normalized_ema_shares(
            download_speed,
            &self.peer_download_rate_emas[..count.min(self.peer_download_rate_emas.len())],
        );
        let upload_shares = normalized_ema_shares(
            upload_speed,
            &self.peer_upload_rate_emas[..count.min(self.peer_upload_rate_emas.len())],
        );
        let lifetime_download_shares =
            weighted_shares(self.session_downloaded, &lifetime_download_weights);
        let lifetime_upload_shares =
            weighted_shares(self.session_uploaded, &lifetime_upload_weights);
        (0..count)
            .map(|index| {
                let peer_download_speed = download_shares[index];
                let peer_upload_speed = upload_shares[index];
                let active = peer_download_speed > 0 || peer_upload_speed > 0;
                BrowserPeerUpdate {
                    address: if index.is_multiple_of(2) {
                        format!(
                            "192.0.2.{}:{}",
                            10 + (self.seed as usize + index) % 180,
                            6881 + index
                        )
                    } else {
                        format!(
                            "198.51.100.{}:{}",
                            10 + (self.seed as usize + index) % 180,
                            51413 + index
                        )
                    },
                    client: format!("simulated-peer-{:02}", (self.seed as usize + index) % 97),
                    download_speed_bps: peer_download_speed,
                    upload_speed_bps: peer_upload_speed,
                    total_downloaded: lifetime_download_shares[index],
                    total_uploaded: lifetime_upload_shares[index],
                    bitfield: self.peer_bitfield(index),
                    active,
                }
            })
            .collect()
    }

    fn peer_rate_weights(&self, count: usize, salt: u64) -> Vec<u64> {
        (0..count)
            .map(|index| {
                32 + mix64(
                    self.seed
                        ^ salt
                        ^ (self.fixed_tick / 5).wrapping_mul(0x9e37_79b9)
                        ^ (index as u64).wrapping_mul(0x85eb_ca6b),
                ) % 193
            })
            .collect()
    }

    fn peer_lifetime_weights(&self, count: usize, salt: u64) -> Vec<u64> {
        (0..count)
            .map(|index| {
                32 + mix64(self.seed ^ salt ^ (index as u64).wrapping_mul(0xc2b2_ae35)) % 193
            })
            .collect()
    }

    fn peer_bitfield(&self, offset: usize) -> Vec<bool> {
        if !self.metadata_available() {
            return Vec::new();
        }
        let count = self.peer_goal.max(1);
        let configured_missing = self.configured_missing_piece_count();
        let missing_start = self.pieces_total as usize - configured_missing;
        let supplier_arrived = configured_missing > 0 && self.missing_piece_count() == 0;
        let supplying_peer = supplier_arrived && offset == self.peer_goal;
        (0..self.pieces_total as usize)
            .map(|piece| {
                if piece >= missing_start && configured_missing > 0 {
                    supplier_arrived && supplying_peer
                } else {
                    self.peer_has_regular_piece(offset, piece, count, self.fixed_tick)
                }
            })
            .collect()
    }

    fn peer_has_regular_piece(
        &self,
        peer_index: usize,
        piece_index: usize,
        peer_count: usize,
        tick: u64,
    ) -> bool {
        let piece_key =
            mix64(self.seed ^ (piece_index as u64).wrapping_mul(0x9e37_79b9) ^ 0x6a09_e667);
        let base_copies = 1 + piece_key as usize % peer_count;
        let rotation = mix64(piece_key ^ 0xbb67_ae85) as usize % peer_count;
        let peer_rank = (peer_index + rotation) % peer_count;
        if peer_rank < base_copies {
            return true;
        }

        let acquisition_tick =
            4 + mix64(piece_key ^ (peer_index as u64).wrapping_mul(0xc2b2_ae35) ^ 0x3c6e_f372)
                % 200;
        tick >= acquisition_tick
    }

    fn configured_missing_piece_count(&self) -> usize {
        match self.availability {
            AvailabilityPreset::Normal => 0,
            AvailabilityPreset::MissingUntil { pieces, .. } => usize::from(pieces),
        }
        .min(self.pieces_total as usize)
    }

    fn piece_acquisition_count(&self) -> usize {
        (0..self.peer_count())
            .map(|peer_index| {
                let initial = self.peer_bitfield_at_tick(peer_index, 0);
                let current = self.peer_bitfield(peer_index);
                initial
                    .iter()
                    .zip(current)
                    .filter(|(before, after)| !**before && *after)
                    .count()
            })
            .sum()
    }

    fn peer_rate_variant_count(&self) -> usize {
        let mut rates = self
            .peers()
            .into_iter()
            .map(|peer| (peer.download_speed_bps, peer.upload_speed_bps))
            .filter(|rates| *rates != (0, 0))
            .collect::<Vec<_>>();
        rates.sort_unstable();
        rates.dedup();
        rates.len()
    }

    fn availability_level_count(&self) -> usize {
        let total_pieces = self.pieces_total as usize;
        if total_pieces == 0 {
            return 0;
        }
        let mut availability = vec![0_u32; total_pieces];
        for peer in self.peers() {
            if peer.bitfield.len() >= total_pieces
                && peer.bitfield.iter().take(total_pieces).all(|has| *has)
            {
                continue;
            }
            for (piece_index, has_piece) in peer.bitfield.into_iter().enumerate().take(total_pieces)
            {
                availability[piece_index] += u32::from(has_piece);
            }
        }
        availability.sort_unstable();
        availability.dedup();
        availability.len()
    }

    fn peer_bitfield_at_tick(&self, peer_index: usize, tick: u64) -> Vec<bool> {
        if !self.metadata_available() {
            return Vec::new();
        }
        let count = self.peer_goal.max(1);
        let configured_missing = self.configured_missing_piece_count();
        let missing_start = self.pieces_total as usize - configured_missing;
        let supplier_arrived = match self.availability {
            AvailabilityPreset::Normal => false,
            AvailabilityPreset::MissingUntil {
                peer_arrival_tick, ..
            } => tick >= u64::from(peer_arrival_tick),
        };
        let supplying_peer = supplier_arrived && peer_index == self.peer_goal;
        (0..self.pieces_total as usize)
            .map(|piece| {
                if piece >= missing_start && configured_missing > 0 {
                    supplier_arrived && supplying_peer
                } else {
                    self.peer_has_regular_piece(peer_index, piece, count, tick)
                }
            })
            .collect()
    }

    fn files(&self) -> Vec<BrowserFileUpdate> {
        if !self.metadata_available() {
            return Vec::new();
        }
        vec![
            BrowserFileUpdate {
                relative_path: "collection/segment-a.bin".to_string(),
                size: self.total_size * 3 / 5,
            },
            BrowserFileUpdate {
                relative_path: "collection/segment-b.bin".to_string(),
                size: self.total_size * 2 / 5,
            },
        ]
    }

    fn record_sample(&mut self) {
        let download_speed_bps = self.download_speed_bps();
        let upload_speed_bps = self.upload_speed_bps();
        push_history(&mut self.download_history, download_speed_bps);
        push_history(&mut self.upload_history, upload_speed_bps);
    }

    fn seed_initial_histories(&mut self) {
        let download_base = self.download_speed_bps();
        let upload_base = self.upload_speed_bps();
        self.download_history = seeded_history(
            download_base,
            download_base / 5,
            self.seed as usize % 40,
        );
        self.upload_history = seeded_history(
            upload_base,
            upload_base / 4,
            self.seed.wrapping_add(13) as usize % 40,
        );
        self.blocks_in_history.clear();
        self.blocks_out_history.clear();
        self.peer_discovery_history.clear();
        self.peer_connection_history.clear();
        self.peer_disconnect_history.clear();
        for sample in 0..HISTORY_LIMIT {
            let download_bps = self.download_history[sample];
            let upload_bps = self.upload_history[sample];
            self.blocks_in_history.push(download_bps / (512 * 1024));
            self.blocks_out_history.push(upload_bps / (256 * 1024));
            self.peer_discovery_history.push(u64::from(
                (sample as u64).wrapping_add(self.seed).is_multiple_of(19),
            ));
            self.peer_connection_history.push(u64::from(
                (sample as u64)
                    .wrapping_add(self.seed.wrapping_mul(3))
                    .is_multiple_of(23),
            ));
            self.peer_disconnect_history.push(u64::from(
                (sample as u64)
                    .wrapping_add(self.seed.wrapping_mul(5))
                    .is_multiple_of(47),
            ));
        }
        self.last_reported_peer_count = self.peer_count();
    }

    fn update(&self) -> BrowserTorrentUpdate {
        let metadata_available = self.metadata_available();
        let (disk_read_bps, disk_write_bps) = self.disk_rates();
        BrowserTorrentUpdate {
            info_hash: self.info_hash.clone(),
            torrent_name: self.name.clone(),
            torrent_or_magnet: self.source.clone(),
            pieces_total: if metadata_available {
                self.pieces_total
            } else {
                0
            },
            pieces_completed: self.pieces_completed(),
            download_speed_bps: self.download_speed_bps(),
            upload_speed_bps: self.upload_speed_bps(),
            activity_message: self.activity(),
            download_path: self.download_path.clone(),
            container_name: self.container_name.clone(),
            control_state: self.control_state,
            data_available: metadata_available,
            is_complete: self.phase == MockTorrentPhase::Seeding,
            total_size: if metadata_available {
                self.total_size
            } else {
                0
            },
            bytes_written: self.bytes_written,
            session_downloaded: self.session_downloaded,
            session_uploaded: self.session_uploaded,
            peers: self.peers(),
            files: self.files(),
            download_history: self.download_history.clone(),
            upload_history: self.upload_history.clone(),
            blocks_in_history: self.blocks_in_history.clone(),
            blocks_out_history: self.blocks_out_history.clone(),
            disk_read_bps,
            disk_write_bps,
            peer_discovery_history: self.peer_discovery_history.clone(),
            peer_connection_history: self.peer_connection_history.clone(),
            peer_disconnect_history: self.peer_disconnect_history.clone(),
        }
    }
}

pub struct DemoCommandService {
    scenario: ScenarioId,
    sessions: HashMap<String, MockTorrentSession>,
    next_torrent_id: u8,
    elapsed_seconds: f64,
    fixed_step_accumulator: f64,
    publish_elapsed: f64,
    second_elapsed: f64,
    last_added_hash: Option<String>,
    total_download_history: Vec<u64>,
    total_upload_history: Vec<u64>,
    disk_read_history: Vec<u64>,
    disk_write_history: Vec<u64>,
    disk_backoff_history_ms: Vec<u64>,
}

impl Default for DemoCommandService {
    fn default() -> Self {
        Self::for_scenario(ScenarioId::default())
    }
}

impl DemoCommandService {
    pub fn for_scenario(scenario: ScenarioId) -> Self {
        Self {
            scenario,
            sessions: HashMap::new(),
            next_torrent_id: 0xb0,
            elapsed_seconds: 0.0,
            fixed_step_accumulator: 0.0,
            publish_elapsed: 0.0,
            second_elapsed: 0.0,
            last_added_hash: None,
            total_download_history: seeded_history(18 * MIB, 7 * MIB, 11),
            total_upload_history: seeded_history(4 * MIB, 2 * MIB, 17),
            disk_read_history: seeded_history(9 * MIB, 4 * MIB, 23),
            disk_write_history: seeded_history(16 * MIB, 6 * MIB, 29),
            disk_backoff_history_ms: (0..HISTORY_LIMIT)
                .map(|sample| if sample.is_multiple_of(41) { 7 } else { 0 })
                .collect(),
        }
    }

    pub fn install_initial_state(&mut self, session: &mut BrowserSession) {
        if self.sessions.is_empty() {
            for initial in scenario_sessions(self.scenario) {
                self.insert(initial);
            }
        }
        self.publish_torrents(session);
        install_supporting_views(session, self.scenario);
        self.publish_runtime(session);
    }

    pub fn scenario_name(&self) -> &'static str {
        self.scenario.name()
    }

    pub fn diagnostics(&self) -> ScenarioDiagnostics {
        let mut diagnostics = ScenarioDiagnostics {
            name: self.scenario.name(),
            metadata: 0,
            peers: 0,
            downloading: 0,
            checking: 0,
            seeding: 0,
            paused: 0,
            deleting: 0,
            max_peers: 0,
            peer_rate_variants: 0,
            availability_levels: 0,
            piece_acquisitions: 0,
            missing_pieces: 0,
            disk_state: MockDiskState::Healthy,
            warning: false,
            recovered: false,
        };
        for torrent in self.sessions.values() {
            match torrent.phase {
                MockTorrentPhase::FetchingMetadata => diagnostics.metadata += 1,
                MockTorrentPhase::DiscoveringPeers => diagnostics.peers += 1,
                MockTorrentPhase::Downloading => diagnostics.downloading += 1,
                MockTorrentPhase::CheckingPieces => diagnostics.checking += 1,
                MockTorrentPhase::Seeding => diagnostics.seeding += 1,
            }
            diagnostics.paused += usize::from(matches!(
                torrent.control_state,
                BrowserTorrentControlState::Paused
            ));
            diagnostics.deleting += usize::from(matches!(
                torrent.control_state,
                BrowserTorrentControlState::Deleting
            ));
            diagnostics.max_peers = diagnostics.max_peers.max(torrent.peer_count());
            diagnostics.peer_rate_variants = diagnostics
                .peer_rate_variants
                .max(torrent.peer_rate_variant_count());
            diagnostics.availability_levels = diagnostics
                .availability_levels
                .max(torrent.availability_level_count());
            diagnostics.piece_acquisitions = diagnostics
                .piece_acquisitions
                .saturating_add(torrent.piece_acquisition_count());
            diagnostics.missing_pieces += torrent.missing_piece_count();
            diagnostics.recovered |= torrent.has_recovered();
            diagnostics.disk_state =
                dominant_disk_state(diagnostics.disk_state, torrent.disk_state());
        }
        diagnostics.warning =
            diagnostics.missing_pieces > 0 || diagnostics.disk_state != MockDiskState::Healthy;
        diagnostics
    }

    #[cfg(test)]
    pub fn torrent_hashes(&self) -> Vec<String> {
        let mut hashes = self.sessions.keys().cloned().collect::<Vec<_>>();
        hashes.sort_unstable();
        hashes
    }

    pub fn fulfill_pending(&mut self, session: &mut BrowserSession) -> Vec<BrowserCommand> {
        let commands = session.drain_commands();
        for command in &commands {
            match command {
                BrowserCommand::AddMagnet {
                    magnet_link,
                    download_path,
                    container_name,
                    ..
                } => {
                    let info_hash =
                        magnet_info_hash(magnet_link).unwrap_or_else(|| self.next_hash());
                    let id = info_hash.first().copied().unwrap_or_default();
                    let mut torrent = MockTorrentSession::new(
                        info_hash,
                        format!("Orbit Archive {id:02x}"),
                        magnet_link.clone(),
                        MockTorrentPhase::FetchingMetadata,
                        0.0,
                    );
                    torrent.download_path = download_path.clone().or(torrent.download_path);
                    torrent.container_name = container_name.clone();
                    self.last_added_hash = Some(hex_encode(&torrent.info_hash));
                    session.upsert_mock_torrent(torrent.update());
                    self.insert(torrent);
                }
                BrowserCommand::Pause { info_hash_hex } => {
                    if let Some(torrent) = self.sessions.get_mut(info_hash_hex) {
                        torrent.control_state = BrowserTorrentControlState::Paused;
                        torrent.clear_rate_averages();
                        session.upsert_mock_torrent(torrent.update());
                    } else {
                        let _ = session.set_torrent_paused_hex(info_hash_hex, true);
                    }
                }
                BrowserCommand::Resume { info_hash_hex } => {
                    if let Some(torrent) = self.sessions.get_mut(info_hash_hex) {
                        torrent.control_state = BrowserTorrentControlState::Running;
                        session.upsert_mock_torrent(torrent.update());
                    } else {
                        let _ = session.set_torrent_paused_hex(info_hash_hex, false);
                    }
                }
                BrowserCommand::Delete { info_hash_hex, .. } => {
                    self.sessions.remove(info_hash_hex);
                    if self.last_added_hash.as_deref() == Some(info_hash_hex) {
                        self.last_added_hash = None;
                    }
                    let _ = session.remove_torrent_hex(info_hash_hex);
                }
                BrowserCommand::FetchFileTree {
                    browser_generation,
                    path,
                    highlight_path,
                } => {
                    let _ = session.apply_mock_file_tree(
                        *browser_generation,
                        path.clone(),
                        mock_file_tree(path),
                        highlight_path.clone(),
                    );
                }
                BrowserCommand::AddTorrentFromFile { path } => {
                    let info_hash = self.next_hash();
                    let id = info_hash[0];
                    let name = path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("Simulated File")
                        .to_string();
                    let mut torrent = MockTorrentSession::new(
                        info_hash,
                        name,
                        path.to_string_lossy().into_owned(),
                        MockTorrentPhase::DiscoveringPeers,
                        0.0,
                    );
                    torrent.download_path = Some(PathBuf::from("/simulated/downloads"));
                    torrent.container_name = Some(format!("collection-{id:02x}"));
                    self.last_added_hash = Some(hex_encode(&torrent.info_hash));
                    session.upsert_mock_torrent(torrent.update());
                    self.insert(torrent);
                }
                BrowserCommand::SetTorrentConfig {
                    info_hash_hex,
                    download_path,
                    container_name,
                    file_priorities,
                } => {
                    if let Some(torrent) = self.sessions.get_mut(info_hash_hex) {
                        torrent.download_path = download_path.clone();
                        torrent.container_name = container_name.clone();
                    }
                    let _ = session.apply_mock_torrent_config(
                        info_hash_hex,
                        download_path.clone(),
                        container_name.clone(),
                        file_priorities,
                    );
                }
            }
        }
        commands
    }

    pub fn advance(&mut self, session: &mut BrowserSession, delta_seconds: f64) -> bool {
        let delta_seconds = delta_seconds.clamp(0.0, 30.0);
        if delta_seconds == 0.0 {
            return false;
        }
        self.fixed_step_accumulator += delta_seconds;
        let complete_steps = ((self.fixed_step_accumulator + FIXED_STEP_EPSILON)
            / FIXED_STEP_SECONDS)
            .floor() as usize;
        self.fixed_step_accumulator -= complete_steps as f64 * FIXED_STEP_SECONDS;
        if self.fixed_step_accumulator.abs() < FIXED_STEP_EPSILON {
            self.fixed_step_accumulator = 0.0;
        }

        for _ in 0..complete_steps {
            for torrent in self.sessions.values_mut() {
                torrent.advance(FIXED_STEP_SECONDS);
            }
            self.elapsed_seconds += FIXED_STEP_SECONDS;
            self.publish_elapsed += FIXED_STEP_SECONDS;
            self.second_elapsed += FIXED_STEP_SECONDS;

            while self.publish_elapsed + FIXED_STEP_EPSILON >= PUBLISH_INTERVAL_SECONDS {
                self.publish_elapsed = (self.publish_elapsed - PUBLISH_INTERVAL_SECONDS).max(0.0);
                self.publish(session);
            }
            while self.second_elapsed + FIXED_STEP_EPSILON >= 1.0 {
                self.second_elapsed = (self.second_elapsed - 1.0).max(0.0);
                self.publish_torrents(session);
                self.emit_manager_events(session);
                let total_download_bps = self
                    .sessions
                    .values()
                    .map(MockTorrentSession::download_speed_bps)
                    .sum::<u64>();
                session.run_mock_second_tick(
                    9.0 + (total_download_bps as f32 / (12.0 * MIB as f32)).min(28.0),
                    34.0 + self.sessions.len() as f32 * 1.2,
                    (72 + self.sessions.len() as u64 * 6) * MIB,
                    7_321 + self.elapsed_seconds.floor() as u64,
                );
                self.publish_runtime(session);
            }
        }

        // Presentation effects use elapsed time directly so animation remains smooth between the
        // deterministic 100 ms model updates.
        session.advance_mock_visualizations(delta_seconds);
        complete_steps > 0
    }

    pub fn phase_hex(&self, info_hash_hex: &str) -> Option<MockTorrentPhase> {
        self.sessions
            .get(info_hash_hex)
            .map(|torrent| torrent.phase)
    }

    pub fn stall_hex(&self, info_hash_hex: &str) -> Option<MockStall> {
        self.sessions
            .get(info_hash_hex)
            .and_then(MockTorrentSession::stall)
    }

    #[cfg(test)]
    pub fn disk_state_hex(&self, info_hash_hex: &str) -> Option<MockDiskState> {
        self.sessions
            .get(info_hash_hex)
            .map(MockTorrentSession::disk_state)
    }

    #[cfg(test)]
    pub fn rate_state_hex(&self, info_hash_hex: &str) -> Option<(u64, u64, u64, u64)> {
        self.sessions.get(info_hash_hex).map(|torrent| {
            (
                torrent.download_speed_bps(),
                torrent.last_download_rate_sample,
                torrent.upload_speed_bps(),
                torrent.last_upload_rate_sample,
            )
        })
    }

    #[cfg(test)]
    pub fn missing_pieces_hex(&self, info_hash_hex: &str) -> Option<usize> {
        self.sessions
            .get(info_hash_hex)
            .map(MockTorrentSession::missing_piece_count)
    }

    #[cfg(test)]
    pub fn peers_hex(&self, info_hash_hex: &str) -> Option<Vec<BrowserPeerUpdate>> {
        self.sessions
            .get(info_hash_hex)
            .map(MockTorrentSession::peers)
    }

    pub fn last_added_hash(&self) -> Option<&str> {
        self.last_added_hash.as_deref()
    }

    fn next_hash(&mut self) -> Vec<u8> {
        self.next_torrent_id = self.next_torrent_id.wrapping_add(1).max(1);
        vec![self.next_torrent_id; 20]
    }

    fn insert(&mut self, torrent: MockTorrentSession) {
        self.sessions
            .insert(hex_encode(&torrent.info_hash), torrent);
    }

    fn publish(&mut self, session: &mut BrowserSession) {
        self.publish_torrents(session);
        self.publish_runtime(session);
    }

    fn publish_torrents(&mut self, session: &mut BrowserSession) {
        let mut hashes = self.sessions.keys().cloned().collect::<Vec<_>>();
        hashes.sort_unstable();
        for hash in hashes {
            if let Some(torrent) = self.sessions.get_mut(&hash) {
                torrent.record_sample();
                session.upsert_mock_torrent(torrent.update());
            }
        }
    }

    fn publish_runtime(&mut self, session: &mut BrowserSession) {
        let total_download_bps = self
            .sessions
            .values()
            .map(MockTorrentSession::download_speed_bps)
            .sum();
        let total_upload_bps = self
            .sessions
            .values()
            .map(MockTorrentSession::upload_speed_bps)
            .sum();
        let (disk_read_bps, disk_write_bps) = self
            .sessions
            .values()
            .map(MockTorrentSession::disk_rates)
            .fold((0_u64, 0_u64), |total, rates| {
                (total.0 + rates.0, total.1 + rates.1)
            });
        let has_disk_backoff = self.sessions.values().any(|torrent| {
            torrent.stall() == Some(MockStall::Disk)
                || torrent.disk_state() != MockDiskState::Healthy
        });
        push_history(&mut self.total_download_history, total_download_bps);
        push_history(&mut self.total_upload_history, total_upload_bps);
        push_history(&mut self.disk_read_history, disk_read_bps);
        push_history(&mut self.disk_write_history, disk_write_bps);
        push_history(
            &mut self.disk_backoff_history_ms,
            if has_disk_backoff { 45 } else { 0 },
        );

        session.apply_mock_runtime_telemetry(BrowserRuntimeTelemetryUpdate {
            cpu_usage: 9.0 + (total_download_bps as f32 / (12.0 * MIB as f32)).min(28.0),
            ram_usage_percent: 34.0 + self.sessions.len() as f32 * 1.2,
            app_ram_usage: (72 + self.sessions.len() as u64 * 6) * MIB,
            run_time: 7_321 + self.elapsed_seconds.floor() as u64,
            total_download_history: self.total_download_history.clone(),
            total_upload_history: self.total_upload_history.clone(),
            disk_read_history: self.disk_read_history.clone(),
            disk_write_history: self.disk_write_history.clone(),
            disk_read_bps,
            disk_write_bps,
            disk_backoff_history_ms: self.disk_backoff_history_ms.clone(),
            dht_nodes: 1_248 + (self.elapsed_seconds as usize % 23),
            dht_active_lookups: self
                .sessions
                .values()
                .filter(|torrent| {
                    matches!(
                        torrent.phase,
                        MockTorrentPhase::FetchingMetadata | MockTorrentPhase::DiscoveringPeers
                    )
                })
                .count(),
            dht_peers_found: self
                .sessions
                .values()
                .map(MockTorrentSession::peer_count)
                .sum(),
        });
    }

    fn emit_manager_events(&mut self, session: &mut BrowserSession) {
        let mut hashes = self.sessions.keys().cloned().collect::<Vec<_>>();
        hashes.sort_unstable();
        for hash in hashes {
            let Some(torrent) = self.sessions.get_mut(&hash) else {
                continue;
            };
            if !matches!(torrent.control_state, BrowserTorrentControlState::Running) {
                torrent.last_reported_peer_count = torrent.peer_count();
                continue;
            }
            let current_peer_count = torrent.peer_count();
            let peers_connected =
                current_peer_count.saturating_sub(torrent.last_reported_peer_count);
            let peers_disconnected = torrent
                .last_reported_peer_count
                .saturating_sub(current_peer_count);
            let peers_discovered = if torrent.phase == MockTorrentPhase::DiscoveringPeers {
                peers_connected.saturating_add(1)
            } else if (self.elapsed_seconds.floor() as u64)
                .wrapping_add(torrent.seed)
                .is_multiple_of(9)
            {
                1
            } else {
                0
            };
            torrent.last_reported_peer_count = current_peer_count;
            let download_bps = torrent.download_speed_bps();
            let upload_bps = torrent.upload_speed_bps();
            let (disk_read_bps, disk_write_bps) = torrent.disk_rates();
            session.apply_mock_manager_events(BrowserManagerEventUpdate {
                info_hash: torrent.info_hash.clone(),
                peers_discovered,
                peers_connected,
                peers_disconnected,
                blocks_received: (download_bps / 180_000).min(24) as usize,
                blocks_sent: (upload_bps / 140_000).min(12) as usize,
                disk_read_bps,
                disk_write_bps,
                disk_backoff_ms: if torrent.stall() == Some(MockStall::Disk)
                    || torrent.disk_state() != MockDiskState::Healthy
                {
                    45
                } else {
                    0
                },
            });
        }
    }
}

fn scenario_sessions(scenario: ScenarioId) -> Vec<MockTorrentSession> {
    scenario
        .preset()
        .sessions
        .iter()
        .copied()
        .map(MockTorrentSession::from_preset)
        .collect()
}

fn install_supporting_views(session: &mut BrowserSession, scenario: ScenarioId) {
    session.apply_mock_telemetry(BrowserTelemetryUpdate {
        cpu_usage: 17.5,
        ram_usage_percent: 42.0,
        app_ram_usage: 96 * MIB,
        run_time: 7_321,
        total_download_history: seeded_history(18 * MIB, 7 * MIB, 11),
        total_upload_history: seeded_history(4 * MIB, 2 * MIB, 17),
        disk_read_history: seeded_history(9 * MIB, 4 * MIB, 23),
        disk_write_history: seeded_history(16 * MIB, 6 * MIB, 29),
        disk_read_bps: 1_400_000,
        disk_write_bps: 2_800_000,
        disk_backoff_history_ms: (0..HISTORY_LIMIT)
            .map(|sample| if sample.is_multiple_of(41) { 7 } else { 0 })
            .collect(),
        dht_nodes: 1_248,
        dht_active_lookups: 3,
        dht_peers_found: 11,
        filesystem: vec![
            BrowserFileUpdate {
                relative_path: "incoming-demo.torrent".to_string(),
                size: 18_432,
            },
            BrowserFileUpdate {
                relative_path: "queued-example.torrent".to_string(),
                size: 22_016,
            },
        ],
        journal: scenario
            .preset()
            .journal
            .iter()
            .map(|entry| BrowserJournalUpdate {
                timestamp: entry.timestamp.to_string(),
                torrent_name: Some(entry.torrent_name.to_string()),
                message: entry.message.to_string(),
                kind: match entry.kind {
                    JournalKind::Lifecycle => BrowserJournalKind::Lifecycle,
                    JournalKind::DataUnavailable => BrowserJournalKind::DataUnavailable,
                    JournalKind::DataRecovered => BrowserJournalKind::DataRecovered,
                },
            })
            .collect(),
        rss: vec![BrowserRssUpdate {
            feed_url: "https://feed.invalid/simulated.xml".to_string(),
            filter_query: "signal garden".to_string(),
            item_title: "Signal Garden Dispatch".to_string(),
            item_link: "magnet:?xt=urn:btih:b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0".to_string(),
            timestamp: "2026-08-30T12:04:00Z".to_string(),
        }],
    });
}

fn seeded_history(base: u64, amplitude: u64, stride: usize) -> Vec<u64> {
    let alpha = 1.0 - (-1.0_f64 / RATE_SMOOTHING_PERIOD_SECONDS).exp();
    let mut average = base as f64;
    let mut history = (0..HISTORY_LIMIT)
        .map(|sample| {
            let position = (sample + stride) % 40;
            let triangle = if position < 20 {
                position as f64 / 10.0 - 1.0
            } else {
                3.0 - position as f64 / 10.0
            };
            let target = (base as f64 + amplitude as f64 * triangle).max(0.0);
            average = update_ema(average, target, alpha);
            average as u64
        })
        .collect::<Vec<_>>();
    let endpoint = history.last().copied().unwrap_or(base);
    if endpoint <= base {
        let adjustment = base - endpoint;
        for sample in &mut history {
            *sample = sample.saturating_add(adjustment);
        }
    } else {
        let adjustment = endpoint - base;
        for sample in &mut history {
            *sample = sample.saturating_sub(adjustment);
        }
    }
    history
}

fn weighted_shares(total: u64, weights: &[u64]) -> Vec<u64> {
    if weights.is_empty() {
        return Vec::new();
    }
    let total_weight = weights.iter().copied().sum::<u64>().max(1);
    let mut assigned = 0_u64;
    weights
        .iter()
        .enumerate()
        .map(|(index, weight)| {
            if index + 1 == weights.len() {
                return total.saturating_sub(assigned);
            }
            let share = ((u128::from(total) * u128::from(*weight)) / u128::from(total_weight))
                .min(u128::from(u64::MAX)) as u64;
            assigned = assigned.saturating_add(share);
            share
        })
        .collect()
}

fn normalized_ema_shares(total: u64, rates: &[f64]) -> Vec<u64> {
    if rates.is_empty() {
        return Vec::new();
    }
    let total_rate = rates.iter().copied().sum::<f64>();
    if total_rate <= f64::EPSILON {
        return weighted_shares(total, &vec![1; rates.len()]);
    }
    let weights = rates
        .iter()
        .map(|rate| ((*rate / total_rate) * 1_000_000.0).max(1.0) as u64)
        .collect::<Vec<_>>();
    weighted_shares(total, &weights)
}

fn update_ema(previous: f64, instantaneous: f64, alpha: f64) -> f64 {
    instantaneous.mul_add(alpha, previous * (1.0 - alpha))
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
pub fn install_simulated_state(session: &mut BrowserSession) {
    DemoCommandService::default().install_initial_state(session);
}

fn dominant_disk_state(left: MockDiskState, right: MockDiskState) -> MockDiskState {
    fn priority(state: MockDiskState) -> u8 {
        match state {
            MockDiskState::Healthy => 0,
            MockDiskState::Recovering => 1,
            MockDiskState::Pressure => 2,
            MockDiskState::Error => 3,
        }
    }
    if priority(right) > priority(left) {
        right
    } else {
        left
    }
}

fn mock_file_tree(path: &Path) -> Vec<BrowserFileTreeEntry> {
    match path.to_string_lossy().as_ref() {
        "/" => vec![BrowserFileTreeEntry {
            name: "simulated".to_string(),
            is_dir: true,
            ..BrowserFileTreeEntry::default()
        }],
        "/simulated/incoming" => vec![BrowserFileTreeEntry {
            name: "nested-fixture.torrent".to_string(),
            size: 16_384,
            is_dir: false,
        }],
        _ => vec![
            BrowserFileTreeEntry {
                name: "fixture-input.torrent".to_string(),
                size: 18_432,
                is_dir: false,
            },
            BrowserFileTreeEntry {
                name: "incoming".to_string(),
                is_dir: true,
                ..BrowserFileTreeEntry::default()
            },
        ],
    }
}

fn magnet_info_hash(magnet: &str) -> Option<Vec<u8>> {
    let hash = magnet.split("btih:").nth(1)?.split('&').next()?;
    decode_hex_hash(hash)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex_hash(value: &str) -> Option<Vec<u8>> {
    if value.len() != 40 {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

fn push_history(history: &mut Vec<u64>, value: u64) {
    history.push(value);
    if history.len() > HISTORY_LIMIT {
        history.drain(..history.len() - HISTORY_LIMIT);
    }
}

fn hex_byte(byte: u8) -> String {
    format!("{byte:02x}").repeat(20)
}
