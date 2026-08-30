// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser-owned deterministic torrent simulation and command fulfillment.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use superseedr::web_integration::{
    BrowserCommand, BrowserFileActivityDirection, BrowserFileActivityUpdate, BrowserFileTreeEntry,
    BrowserFileUpdate, BrowserJournalKind, BrowserJournalUpdate, BrowserManagerEventUpdate,
    BrowserPeerTransport, BrowserPeerUpdate, BrowserRssUpdate, BrowserRuntimeTelemetryUpdate,
    BrowserSession, BrowserTelemetryUpdate, BrowserTorrentControlState, BrowserTorrentPreviewFile,
    BrowserTorrentUpdate,
};

use crate::scenarios::{
    AvailabilityPreset, DiskPreset, InitialControl, InitialPhase, JournalKind, ScenarioId,
    SessionPreset,
};

const METADATA_SECONDS: f64 = 0.6;
const PEER_DISCOVERY_SECONDS: f64 = 0.8;
const CHECKING_SECONDS: f64 = 0.7;
const FIXED_STEP_SECONDS: f64 = 1.0 / 60.0;
const FIXED_STEP_EPSILON: f64 = 1.0e-9;
const SCENARIO_TICK_SECONDS: f64 = 0.1;
// Keep the browser telemetry boundary consistent with the native torrent manager. At Rate60s the
// manager applies this time-aware EMA on every ~17 ms tick before publishing torrent and peer
// metrics.
pub(crate) const RATE_SMOOTHING_PERIOD_SECONDS: f64 = 5.0;
const HISTORY_SAMPLE_INTERVAL_SECONDS: f64 = 0.1;
const HISTORY_LIMIT: usize = 120;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const BLOCK_SIZE: u64 = 16_384;
const TRACKER_ANNOUNCE_INTERVAL_SECONDS: u64 = 30 * 60;
const PEER_STARTUP_TICKS: u64 = 3;
const ESTABLISHED_PEER_TICK: u64 = u64::MAX;
pub(crate) const MAX_SIMULATED_PEER_DOWNLOAD_BPS: u64 = 2_000_000_000;

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
    scenario_elapsed: f64,
    announce_elapsed: f64,
    next_announce_at: f64,
    fixed_tick: u64,
    download_rate_ema: f64,
    upload_rate_ema: f64,
    peer_download_rate_emas: Vec<f64>,
    peer_upload_rate_emas: Vec<f64>,
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
    active_peer_ids: Vec<usize>,
    peer_connection_started_ticks: HashMap<usize, u64>,
    last_reported_peer_ids: Vec<usize>,
    peer_total_downloaded: HashMap<usize, u64>,
    peer_total_uploaded: HashMap<usize, u64>,
    peer_connection_counts: HashMap<usize, u64>,
    peer_disconnect_counts: HashMap<usize, u64>,
    peer_download_starts: u64,
    bytes_downloaded_this_tick: u64,
    bytes_uploaded_this_tick: u64,
    pending_download_block_bytes: u64,
    pending_upload_block_bytes: u64,
    pending_disk_read_bytes: u64,
    pending_disk_write_bytes: u64,
    disk_operation_sequence: u64,
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
            scenario_elapsed: 0.0,
            announce_elapsed: 0.0,
            next_announce_at: TRACKER_ANNOUNCE_INTERVAL_SECONDS as f64,
            fixed_tick: 0,
            download_rate_ema: 0.0,
            upload_rate_ema: 0.0,
            peer_download_rate_emas: Vec::new(),
            peer_upload_rate_emas: Vec::new(),
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
            active_peer_ids: Vec::new(),
            peer_connection_started_ticks: HashMap::new(),
            last_reported_peer_ids: Vec::new(),
            peer_total_downloaded: HashMap::new(),
            peer_total_uploaded: HashMap::new(),
            peer_connection_counts: HashMap::new(),
            peer_disconnect_counts: HashMap::new(),
            peer_download_starts: 0,
            bytes_downloaded_this_tick: 0,
            bytes_uploaded_this_tick: 0,
            pending_download_block_bytes: 0,
            pending_upload_block_bytes: 0,
            pending_disk_read_bytes: 0,
            pending_disk_write_bytes: 0,
            disk_operation_sequence: 0,
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
        torrent.phase_elapsed =
            f64::from(preset.phase_elapsed_ticks) * SCENARIO_TICK_SECONDS;
        if phase == MockTorrentPhase::Seeding {
            torrent.total_size = (8 + torrent.seed % 8) * GIB;
            torrent.bytes_written = torrent.total_size;
            torrent.session_downloaded = torrent.total_size;
        }
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
        torrent.initialize_active_peer_state();
        torrent.initialize_rate_averages();
        torrent.initialize_peer_lifetime_state();
        torrent.seed_initial_histories();
        torrent
    }

    fn use_interactive_fixture_size(&mut self) {
        self.total_size = (24 + self.seed % 8) * MIB;
        self.pieces_total = 192;
        self.bytes_written = 0;
        self.session_downloaded = 0;
        self.session_uploaded = 0;
    }

    fn advance(&mut self, delta_seconds: f64) {
        self.announce_elapsed += delta_seconds;
        if !matches!(self.control_state, BrowserTorrentControlState::Running) {
            return;
        }
        while self.announce_elapsed + FIXED_STEP_EPSILON >= self.next_announce_at {
            self.next_announce_at += TRACKER_ANNOUNCE_INTERVAL_SECONDS as f64;
        }

        self.bytes_downloaded_this_tick = 0;
        self.bytes_uploaded_this_tick = 0;

        self.scenario_elapsed += delta_seconds;
        self.fixed_tick = ((self.scenario_elapsed + FIXED_STEP_EPSILON)
            / SCENARIO_TICK_SECONDS)
            .floor() as u64;
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
        self.sync_active_peer_state();

        let raw_download_speed_bps = self.raw_download_speed_bps();
        let raw_upload_speed_bps = self.raw_upload_speed_bps();
        self.accumulate_rate_sample(
            delta_seconds,
            raw_download_speed_bps,
            raw_upload_speed_bps,
        );

        match self.phase {
            MockTorrentPhase::Downloading => {
                let downloaded =
                    (raw_download_speed_bps as f64 * delta_seconds / 8.0) as u64;
                let uploaded = (raw_upload_speed_bps as f64 * delta_seconds / 8.0) as u64;
                let download_ceiling = self.download_ceiling();
                self.bytes_written = self
                    .bytes_written
                    .saturating_add(downloaded)
                    .min(download_ceiling);
                self.session_downloaded = self.session_downloaded.saturating_add(downloaded);
                self.session_uploaded = self.session_uploaded.saturating_add(uploaded);
                self.record_transfer(downloaded, uploaded);
                if self.bytes_written >= self.total_size && self.missing_piece_count() == 0 {
                    self.phase = MockTorrentPhase::CheckingPieces;
                    self.phase_elapsed = 0.0;
                }
            }
            MockTorrentPhase::Seeding => {
                let uploaded = (raw_upload_speed_bps as f64 * delta_seconds / 8.0) as u64;
                self.session_uploaded = self.session_uploaded.saturating_add(uploaded);
                self.record_transfer(0, uploaded);
            }
            _ => {}
        }
    }

    fn record_transfer(&mut self, downloaded: u64, uploaded: u64) {
        self.bytes_downloaded_this_tick = downloaded;
        self.bytes_uploaded_this_tick = uploaded;
        self.pending_download_block_bytes = self
            .pending_download_block_bytes
            .saturating_add(downloaded);
        self.pending_upload_block_bytes =
            self.pending_upload_block_bytes.saturating_add(uploaded);

        let roster = self.peer_roster();
        let download_shares = weighted_shares(
            downloaded,
            &roster
                .iter()
                .map(|peer| 32 + mix64(self.seed ^ 0x2d ^ *peer as u64) % 193)
                .collect::<Vec<_>>(),
        );
        let upload_shares = weighted_shares(uploaded, &self.peer_upload_targets(&roster));
        for (index, peer) in roster.into_iter().enumerate() {
            let downloaded_total = self.peer_total_downloaded.entry(peer).or_default();
            *downloaded_total = downloaded_total
                .saturating_add(download_shares.get(index).copied().unwrap_or_default());
            let uploaded_total = self.peer_total_uploaded.entry(peer).or_default();
            let began_new_peer_download = *uploaded_total == 0
                && upload_shares.get(index).copied().unwrap_or_default() > 0
                && self.peer_connection_started_ticks.get(&peer).is_some_and(|started| {
                    *started != ESTABLISHED_PEER_TICK
                });
            *uploaded_total = uploaded_total
                .saturating_add(upload_shares.get(index).copied().unwrap_or_default());
            self.peer_download_starts = self
                .peer_download_starts
                .saturating_add(u64::from(began_new_peer_download));
        }
    }

    fn initialize_active_peer_state(&mut self) {
        self.active_peer_ids = self.peer_roster();
        for peer_id in &self.active_peer_ids {
            self.peer_connection_started_ticks
                .insert(*peer_id, ESTABLISHED_PEER_TICK);
        }
    }

    fn sync_active_peer_state(&mut self) {
        let current = self.peer_roster();
        for peer_id in self
            .active_peer_ids
            .iter()
            .filter(|peer_id| !current.contains(peer_id))
        {
            self.peer_connection_started_ticks.remove(peer_id);
            if let Some(rate) = self.peer_download_rate_emas.get_mut(*peer_id) {
                *rate = 0.0;
            }
            if let Some(rate) = self.peer_upload_rate_emas.get_mut(*peer_id) {
                *rate = 0.0;
            }
        }
        for peer_id in current
            .iter()
            .filter(|peer_id| !self.active_peer_ids.contains(peer_id))
        {
            self.peer_connection_started_ticks
                .insert(*peer_id, self.fixed_tick);
        }
        self.active_peer_ids = current;
    }

    fn peer_is_ready(&self, peer_id: usize) -> bool {
        self.peer_connection_started_ticks
            .get(&peer_id)
            .is_some_and(|started| {
                *started == ESTABLISHED_PEER_TICK
                    || self.fixed_tick.saturating_sub(*started) >= PEER_STARTUP_TICKS
            })
    }

    fn stall(&self) -> Option<MockStall> {
        if self.phase != MockTorrentPhase::Downloading
            || !matches!(self.control_state, BrowserTorrentControlState::Running)
        {
            return None;
        }
        let cycle = self.phase_elapsed.rem_euclid(4.8);
        if (1.3 - FIXED_STEP_EPSILON..1.75 - FIXED_STEP_EPSILON).contains(&cycle) {
            Some(MockStall::Peer)
        } else if (3.0 - FIXED_STEP_EPSILON..3.4 - FIXED_STEP_EPSILON).contains(&cycle) {
            Some(MockStall::Disk)
        } else {
            None
        }
    }

    fn raw_download_speed_bps(&self) -> u64 {
        if !matches!(self.control_state, BrowserTorrentControlState::Running)
            || self.phase != MockTorrentPhase::Downloading
            || self.stall().is_some()
            || self.peer_count() == 0
            || self.disk_state() == MockDiskState::Error
        {
            return 0;
        }
        let peer_factor = self.peer_count() as f64 / self.peer_goal.max(1) as f64;
        let base = 22.0
            * MIB as f64
            * (f64::from(self.rate_percent) / 100.0)
            * peer_factor.clamp(0.15, 1.0)
            * self.transfer_envelope(0x51);
        let base = base.max(0.0) as u64;
        match self.disk_state() {
            MockDiskState::Pressure => base / 5,
            MockDiskState::Recovering => base / 2,
            MockDiskState::Healthy => base,
            MockDiskState::Error => 0,
        }
    }

    fn raw_upload_speed_bps(&self) -> u64 {
        if !matches!(self.control_state, BrowserTorrentControlState::Running)
            || self.upload_recipient_count() == 0
            || self.disk_state() == MockDiskState::Error
        {
            return 0;
        }
        if self.phase == MockTorrentPhase::Seeding {
            return self
                .peer_upload_targets(&self.peer_roster())
                .into_iter()
                .sum();
        }
        let peer_factor = self.upload_recipient_count() as f64 / self.peer_goal.max(1) as f64;
        let baseline = match self.phase {
            MockTorrentPhase::Downloading if self.stall().is_none() => 720.0 * 1024.0,
            MockTorrentPhase::Seeding => 3.2 * MIB as f64,
            _ => 0.0,
        };
        let base = (baseline
            * (f64::from(self.rate_percent) / 100.0)
            * peer_factor.clamp(0.15, 1.0)
            * self.transfer_envelope(0xa7))
        .max(0.0) as u64;
        match self.disk_state() {
            MockDiskState::Pressure => base / 3,
            MockDiskState::Recovering => base / 2,
            MockDiskState::Healthy => base,
            MockDiskState::Error => 0,
        }
    }

    fn download_speed_bps(&self) -> u64 {
        if matches!(self.control_state, BrowserTorrentControlState::Running)
            && matches!(
                self.phase,
                MockTorrentPhase::Downloading
                    | MockTorrentPhase::CheckingPieces
                    | MockTorrentPhase::Seeding
            )
        {
            self.download_rate_ema.max(0.0) as u64
        } else {
            0
        }
    }

    fn upload_speed_bps(&self) -> u64 {
        if matches!(self.control_state, BrowserTorrentControlState::Running)
            && matches!(
                self.phase,
                MockTorrentPhase::Downloading
                    | MockTorrentPhase::CheckingPieces
                    | MockTorrentPhase::Seeding
            )
        {
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
        self.active_peer_ids.clear();
        self.peer_connection_started_ticks.clear();
        self.last_download_rate_sample = 0;
        self.last_upload_rate_sample = 0;
        self.bytes_downloaded_this_tick = 0;
        self.bytes_uploaded_this_tick = 0;
        self.pending_download_block_bytes = 0;
        self.pending_upload_block_bytes = 0;
        self.pending_disk_read_bytes = 0;
        self.pending_disk_write_bytes = 0;
    }

    fn accumulate_rate_sample(
        &mut self,
        delta_seconds: f64,
        raw_download_speed_bps: u64,
        raw_upload_speed_bps: u64,
    ) {
        let roster = self.peer_roster();
        let download_targets = weighted_shares(
            raw_download_speed_bps,
            &self.peer_rate_weights(&roster, 0x51),
        );
        let upload_targets = if self.phase == MockTorrentPhase::Seeding {
            self.peer_upload_targets(&roster)
        } else {
            weighted_shares(raw_upload_speed_bps, &self.peer_upload_targets(&roster))
        };
        let peer_capacity = self.peer_capacity();
        self.peer_download_rate_emas.resize(peer_capacity, 0.0);
        self.peer_upload_rate_emas.resize(peer_capacity, 0.0);
        self.last_download_rate_sample = raw_download_speed_bps;
        self.last_upload_rate_sample = raw_upload_speed_bps;
        let alpha = 1.0 - (-delta_seconds / RATE_SMOOTHING_PERIOD_SECONDS).exp();
        self.download_rate_ema = update_ema(
            self.download_rate_ema,
            raw_download_speed_bps as f64,
            alpha,
        );
        self.upload_rate_ema =
            update_ema(self.upload_rate_ema, raw_upload_speed_bps as f64, alpha);
        for (index, peer_id) in roster.into_iter().enumerate() {
            self.peer_download_rate_emas[peer_id] = update_ema(
                self.peer_download_rate_emas[peer_id],
                download_targets.get(index).copied().unwrap_or_default() as f64,
                alpha,
            );
            self.peer_upload_rate_emas[peer_id] = update_ema(
                self.peer_upload_rate_emas[peer_id],
                upload_targets.get(index).copied().unwrap_or_default() as f64,
                alpha,
            );
        }
    }

    fn set_peer_rate_averages(&mut self, download_speed_bps: u64, upload_speed_bps: u64) {
        let roster = self.peer_roster();
        let download_rates = weighted_shares(
            download_speed_bps,
            &self.peer_rate_weights(&roster, 0x51),
        );
        let upload_rates = if self.phase == MockTorrentPhase::Seeding {
            self.peer_upload_targets(&roster)
        } else {
            weighted_shares(upload_speed_bps, &self.peer_upload_targets(&roster))
        };
        let peer_capacity = self.peer_capacity();
        self.peer_download_rate_emas = vec![0.0; peer_capacity];
        self.peer_upload_rate_emas = vec![0.0; peer_capacity];
        for (index, peer_id) in roster.into_iter().enumerate() {
            self.peer_download_rate_emas[peer_id] =
                download_rates.get(index).copied().unwrap_or_default() as f64;
            self.peer_upload_rate_emas[peer_id] =
                upload_rates.get(index).copied().unwrap_or_default() as f64;
        }
    }

    fn initialize_peer_lifetime_state(&mut self) {
        let roster = self.peer_roster();
        let download_weights = roster
            .iter()
            .map(|peer| 32 + mix64(self.seed ^ 0x2d ^ *peer as u64) % 193)
            .collect::<Vec<_>>();
        let upload_weights = roster
            .iter()
            .map(|peer| 32 + mix64(self.seed ^ 0xc3 ^ *peer as u64) % 193)
            .collect::<Vec<_>>();
        let download_shares = weighted_shares(self.session_downloaded, &download_weights);
        let upload_shares = weighted_shares(self.session_uploaded, &upload_weights);
        for (index, peer) in roster.into_iter().enumerate() {
            self.peer_total_downloaded
                .insert(peer, download_shares[index]);
            self.peer_total_uploaded.insert(peer, upload_shares[index]);
            self.peer_connection_counts.insert(peer, 1);
        }
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

    fn disk_latencies_micros(&self) -> (u64, u64, u64) {
        match self.disk_state() {
            MockDiskState::Error => (0, 0, 0),
            MockDiskState::Pressure => (8_500, 15_000, 24_000),
            MockDiskState::Recovering => (3_200, 5_800, 9_200),
            MockDiskState::Healthy if self.stall() == Some(MockStall::Disk) => {
                (2_400, 7_200, 12_500)
            }
            MockDiskState::Healthy if self.phase == MockTorrentPhase::CheckingPieces => {
                (1_800, 2_600, 4_100)
            }
            MockDiskState::Healthy => (620, 940, 1_450),
        }
    }

    fn drain_transfer_events(&mut self) -> Option<BrowserManagerEventUpdate> {
        let (disk_read_bps, disk_write_bps) = self.disk_rates();
        self.pending_disk_read_bytes = self.pending_disk_read_bytes.saturating_add(
            (disk_read_bps as f64 * FIXED_STEP_SECONDS / 8.0) as u64,
        );
        self.pending_disk_write_bytes = self.pending_disk_write_bytes.saturating_add(
            (disk_write_bps as f64 * FIXED_STEP_SECONDS / 8.0) as u64,
        );

        let blocks_received = self.pending_download_block_bytes / BLOCK_SIZE;
        let blocks_sent = self.pending_upload_block_bytes / BLOCK_SIZE;
        let disk_read_operations = self.pending_disk_read_bytes / BLOCK_SIZE;
        let disk_write_operations = self.pending_disk_write_bytes / BLOCK_SIZE;
        self.pending_download_block_bytes %= BLOCK_SIZE;
        self.pending_upload_block_bytes %= BLOCK_SIZE;
        self.pending_disk_read_bytes %= BLOCK_SIZE;
        self.pending_disk_write_bytes %= BLOCK_SIZE;
        if blocks_received == 0
            && blocks_sent == 0
            && disk_read_operations == 0
            && disk_write_operations == 0
        {
            return None;
        }

        let (disk_read_latency_micros, disk_write_latency_micros, recv_to_write_latency_micros) =
            self.disk_latencies_micros();
        let disk_operation_sequence = self.disk_operation_sequence;
        self.disk_operation_sequence = self.disk_operation_sequence.saturating_add(
            disk_read_operations.max(disk_write_operations),
        );
        Some(BrowserManagerEventUpdate {
            info_hash: self.info_hash.clone(),
            blocks_received: blocks_received as usize,
            blocks_sent: blocks_sent as usize,
            disk_read_bytes: disk_read_operations.saturating_mul(BLOCK_SIZE),
            disk_write_bytes: disk_write_operations.saturating_mul(BLOCK_SIZE),
            disk_read_operations: disk_read_operations as usize,
            disk_write_operations: disk_write_operations as usize,
            disk_operation_sequence,
            disk_read_latency_micros,
            disk_write_latency_micros,
            recv_to_write_latency_micros,
            ..BrowserManagerEventUpdate::default()
        })
    }

    fn peer_count(&self) -> usize {
        self.peer_roster().len()
    }

    fn normal_peer_count(&self) -> usize {
        if !matches!(self.control_state, BrowserTorrentControlState::Running) {
            return 0;
        }
        match self.phase {
            MockTorrentPhase::FetchingMetadata => 0,
            MockTorrentPhase::DiscoveringPeers => ((self.phase_elapsed / PEER_DISCOVERY_SECONDS
                * self.peer_goal as f64)
                .ceil() as usize)
                .min(self.peer_goal),
            MockTorrentPhase::Downloading if self.stall() == Some(MockStall::Peer) => 0,
            MockTorrentPhase::Downloading
            | MockTorrentPhase::CheckingPieces
            | MockTorrentPhase::Seeding => {
                let epoch = ((self.scenario_elapsed + self.peer_time_offset()) / 1.8).floor() as u64;
                let departure_count = match mix64(self.seed ^ epoch.wrapping_mul(0x9e37_79b9)) % 5 {
                    0 | 1 => 0,
                    2 | 3 => 1,
                    _ => 2,
                };
                self.peer_goal.saturating_sub(departure_count).max(1)
            }
        }
    }

    fn peer_roster(&self) -> Vec<usize> {
        let count = self.normal_peer_count();
        let pool_size = self.peer_goal.saturating_add(3).max(1);
        let epoch = ((self.scenario_elapsed + self.peer_time_offset()) / 2.4).floor() as usize;
        let start = (epoch + self.seed as usize) % pool_size;
        let mut roster = (0..count)
            .map(|offset| (start + offset) % pool_size)
            .collect::<Vec<_>>();
        if matches!(self.availability, AvailabilityPreset::MissingUntil { .. })
            && self.missing_piece_count() == 0
            && !matches!(
                self.phase,
                MockTorrentPhase::FetchingMetadata | MockTorrentPhase::DiscoveringPeers
            )
        {
            roster.push(pool_size);
        }
        roster
    }

    fn upload_recipient_count(&self) -> usize {
        self.upload_recipient_ids().len()
    }

    fn upload_recipient_ids(&self) -> Vec<usize> {
        if !matches!(
            self.phase,
            MockTorrentPhase::Downloading | MockTorrentPhase::Seeding
        ) || !matches!(self.control_state, BrowserTorrentControlState::Running)
        {
            return Vec::new();
        }
        let roster = self.peer_roster();
        let connected = roster.len();
        if connected == 0 {
            return Vec::new();
        }
        let cycle = (self.scenario_elapsed + self.peer_time_offset() * 0.7).rem_euclid(6.4);
        if cycle < 1.05 - FIXED_STEP_EPSILON {
            return Vec::new();
        }
        let epoch = ((self.scenario_elapsed + self.peer_time_offset()) / 1.3).floor() as u64;
        let uninterested = (mix64(self.seed ^ epoch.wrapping_mul(0xc2b2_ae35)) % 3) as usize;
        let target = connected.saturating_sub(uninterested).max(1);
        roster
            .into_iter()
            .filter(|peer_id| self.peer_is_ready(*peer_id))
            .filter(|peer_id| {
                self.phase != MockTorrentPhase::Seeding || !self.peer_has_complete_copy(*peer_id)
            })
            .take(target)
            .collect()
    }

    fn peer_capacity(&self) -> usize {
        self.peer_goal.saturating_add(4).max(1)
    }

    fn peer_has_complete_copy(&self, peer_id: usize) -> bool {
        self.peer_total_uploaded
            .get(&peer_id)
            .copied()
            .unwrap_or_default()
            >= self.total_size
    }

    fn peer_upload_targets(&self, roster: &[usize]) -> Vec<u64> {
        let recipients = self.upload_recipient_ids();
        roster
            .iter()
            .map(|peer_id| {
                if !recipients.contains(peer_id) {
                    return 0;
                }
                if self.phase == MockTorrentPhase::Seeding {
                    return self.remote_peer_download_bps(*peer_id);
                }
                32 + mix64(self.seed ^ 0xc3 ^ *peer_id as u64) % 193
            })
            .collect()
    }

    fn remote_peer_download_bps(&self, peer_id: usize) -> u64 {
        let range = MAX_SIMULATED_PEER_DOWNLOAD_BPS - 8_000_000;
        let capacity = 8_000_000
            + mix64(self.seed ^ 0x6a09_e667 ^ (peer_id as u64).wrapping_mul(0x9e37_79b9))
                % (range + 1);
        let phase = (mix64(self.seed ^ 0xbb67_ae85 ^ peer_id as u64) % 1_000) as f64
            / 1_000.0
            * std::f64::consts::TAU;
        let slow_wave = (self.scenario_elapsed * 0.31 + phase).sin();
        let burst = (self.scenario_elapsed * 0.73 + phase * 0.47)
            .sin()
            .max(0.0)
            .powi(6);
        let envelope = (0.62 + slow_wave * 0.24 + burst * 0.46).clamp(0.2, 1.3);
        let rate = capacity as f64
            * envelope
            * (f64::from(self.rate_percent) / 100.0);
        let rate = rate.max(0.0) as u64;
        let rate = match self.disk_state() {
            MockDiskState::Pressure => rate / 3,
            MockDiskState::Recovering => rate / 2,
            MockDiskState::Healthy => rate,
            MockDiskState::Error => 0,
        };
        rate.min(MAX_SIMULATED_PEER_DOWNLOAD_BPS)
    }

    fn peer_time_offset(&self) -> f64 {
        (self.seed % 19) as f64 * 0.23
    }

    fn transfer_envelope(&self, salt: u64) -> f64 {
        let phase = (mix64(self.seed ^ salt) % 1_000) as f64 / 1_000.0
            * std::f64::consts::TAU;
        let elapsed = self.scenario_elapsed;
        let slow_wave = (elapsed * 0.43 + phase).sin();
        let fine_wave = (elapsed * 1.37 + phase * 0.61).sin();
        let burst = (elapsed * 0.61 + phase * 1.17).sin().max(0.0).powi(8);
        let lull_cycle = (elapsed + self.peer_time_offset() + (salt & 7) as f64 * 0.17)
            .rem_euclid(11.0);
        let lull = if (7.4..8.7).contains(&lull_cycle) {
            0.16
        } else if (8.7..9.4).contains(&lull_cycle) {
            0.48
        } else {
            1.0
        };
        ((0.76 + slow_wave * 0.24 + fine_wave * 0.08 + burst * 0.72) * lull)
            .clamp(0.08, 1.8)
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
        if matches!(
            self.phase,
            MockTorrentPhase::Downloading | MockTorrentPhase::Seeding
        ) && self.peer_count() > 0
            && self.upload_recipient_count() == 0
        {
            return "No simulated peers are currently requesting upload data".to_string();
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
        let roster = self.peer_roster();
        let download_speed = self.download_speed_bps();
        let upload_speed = self.upload_speed_bps();
        let download_rate_emas = roster
            .iter()
            .map(|peer_id| {
                self.peer_download_rate_emas
                    .get(*peer_id)
                    .copied()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let upload_rate_emas = roster
            .iter()
            .map(|peer_id| {
                self.peer_upload_rate_emas
                    .get(*peer_id)
                    .copied()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let download_shares = normalized_ema_shares(
            download_speed,
            &download_rate_emas,
        );
        let upload_shares = normalized_ema_shares(upload_speed, &upload_rate_emas);
        let upload_recipients = self.upload_recipient_ids();
        roster
            .into_iter()
            .enumerate()
            .map(|(index, peer_slot)| {
                let peer_download_speed = download_shares[index];
                let peer_upload_speed = upload_shares[index];
                let am_interested = matches!(self.phase, MockTorrentPhase::Downloading)
                    && self.peer_bitfield(peer_slot).iter().any(|has_piece| *has_piece);
                let peer_interested = upload_recipients.contains(&peer_slot);
                BrowserPeerUpdate {
                    address: if peer_slot.is_multiple_of(2) {
                        format!(
                            "192.0.2.{}:{}",
                            10 + (self.seed as usize + peer_slot) % 180,
                            6881 + peer_slot
                        )
                    } else {
                        format!(
                            "198.51.100.{}:{}",
                            10 + (self.seed as usize + peer_slot) % 180,
                            51413 + peer_slot
                        )
                    },
                    client: format!(
                        "simulated-peer-{:02}",
                        (self.seed as usize + peer_slot) % 97
                    ),
                    download_speed_bps: peer_download_speed,
                    upload_speed_bps: peer_upload_speed,
                    total_downloaded: self
                        .peer_total_downloaded
                        .get(&peer_slot)
                        .copied()
                        .unwrap_or_default(),
                    total_uploaded: self
                        .peer_total_uploaded
                        .get(&peer_slot)
                        .copied()
                        .unwrap_or_default(),
                    bitfield: self.peer_bitfield(peer_slot),
                    transport: if peer_slot.is_multiple_of(3) {
                        BrowserPeerTransport::Utp
                    } else {
                        BrowserPeerTransport::Tcp
                    },
                    am_choking: !peer_interested || peer_upload_speed == 0,
                    peer_choking: !am_interested || peer_download_speed == 0,
                    am_interested,
                    peer_interested,
                    connection_count: self
                        .peer_connection_counts
                        .get(&peer_slot)
                        .copied()
                        .unwrap_or(1),
                    disconnect_count: self
                        .peer_disconnect_counts
                        .get(&peer_slot)
                        .copied()
                        .unwrap_or_default(),
                    last_action: if peer_download_speed > 0 {
                        "Receiving".to_string()
                    } else if peer_upload_speed > 0 {
                        "Requesting".to_string()
                    } else if am_interested || peer_interested {
                        "Unchoked".to_string()
                    } else {
                        "Idle".to_string()
                    },
                }
            })
            .collect()
    }

    fn peer_rate_weights(&self, roster: &[usize], salt: u64) -> Vec<u64> {
        roster
            .iter()
            .map(|peer_id| {
                32 + mix64(
                    self.seed
                        ^ salt
                        ^ (self.fixed_tick / 5).wrapping_mul(0x9e37_79b9)
                        ^ (*peer_id as u64).wrapping_mul(0x85eb_ca6b),
                ) % 193
            })
            .collect()
    }

    fn peer_bitfield(&self, offset: usize) -> Vec<bool> {
        if !self.metadata_available() {
            return Vec::new();
        }
        if self.phase == MockTorrentPhase::Seeding {
            return self.seeding_peer_bitfield(offset);
        }
        let count = self.peer_goal.max(1);
        let configured_missing = self.configured_missing_piece_count();
        let missing_start = self.pieces_total as usize - configured_missing;
        let supplier_arrived = configured_missing > 0 && self.missing_piece_count() == 0;
        let supplying_peer = supplier_arrived && offset == self.peer_goal.saturating_add(3);
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

    fn seeding_peer_bitfield(&self, peer_id: usize) -> Vec<bool> {
        let downloaded = self
            .peer_total_uploaded
            .get(&peer_id)
            .copied()
            .unwrap_or_default()
            .min(self.total_size);
        if downloaded == 0 {
            return vec![false; self.pieces_total as usize];
        }
        if downloaded >= self.total_size {
            return vec![true; self.pieces_total as usize];
        }
        let threshold = (u128::from(downloaded) * u128::from(u64::MAX)
            / u128::from(self.total_size)) as u64;
        (0..self.pieces_total as usize)
            .map(|piece_index| {
                mix64(
                    self.seed
                        ^ 0x3c6e_f372
                        ^ (peer_id as u64).wrapping_mul(0xc2b2_ae35)
                        ^ (piece_index as u64).wrapping_mul(0x9e37_79b9),
                ) <= threshold
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
        self.peer_roster()
            .into_iter()
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
        if self.phase == MockTorrentPhase::Seeding && tick == 0 {
            return vec![false; self.pieces_total as usize];
        }
        if self.phase == MockTorrentPhase::Seeding {
            return self.seeding_peer_bitfield(peer_index);
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
        let supplying_peer = supplier_arrived && peer_index == self.peer_goal.saturating_add(3);
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

    fn eta(&self) -> Duration {
        if !self.metadata_available() || self.pieces_completed() == self.pieces_total {
            return Duration::ZERO;
        }
        let download_speed_bps = self.download_speed_bps();
        if download_speed_bps == 0 {
            return Duration::MAX;
        }
        let bytes_remaining = self.total_size.saturating_sub(self.bytes_written);
        Duration::from_secs(
            bytes_remaining
                .saturating_mul(8)
                .checked_div(download_speed_bps)
                .unwrap_or_default(),
        )
    }

    fn next_announce_in(&self) -> Duration {
        Duration::from_secs_f64((self.next_announce_at - self.announce_elapsed).max(0.0))
    }

    fn file_activity_updates(&self) -> Vec<BrowserFileActivityUpdate> {
        if !self.metadata_available() {
            return Vec::new();
        }
        let files = self.files();
        let selected = if self.total_size == 0
            || self.bytes_written.saturating_mul(5) < self.total_size.saturating_mul(3)
        {
            files.first()
        } else {
            files.get(1)
        };
        let Some(file) = selected else {
            return Vec::new();
        };
        let mut updates = Vec::with_capacity(2);
        if self.bytes_downloaded_this_tick > 0 {
            updates.push(BrowserFileActivityUpdate {
                touched_relative_paths: vec![file.relative_path.clone()],
                direction: BrowserFileActivityDirection::Download,
            });
        }
        if self.bytes_uploaded_this_tick > 0 {
            updates.push(BrowserFileActivityUpdate {
                touched_relative_paths: vec![file.relative_path.clone()],
                direction: BrowserFileActivityDirection::Upload,
            });
        }
        updates
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
            self.blocks_in_history
                .push((download_bps / 8) / BLOCK_SIZE);
            self.blocks_out_history
                .push((upload_bps / 8) / BLOCK_SIZE);
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
        self.last_reported_peer_ids = self.peer_roster();
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
            bytes_downloaded_this_tick: self.bytes_downloaded_this_tick,
            bytes_uploaded_this_tick: self.bytes_uploaded_this_tick,
            eta: self.eta(),
            next_announce_in: self.next_announce_in(),
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
            file_activity_updates: self.file_activity_updates(),
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
        let mut fulfilled = Vec::new();
        for _ in 0..8 {
            let commands = session.drain_commands();
            if commands.is_empty() {
                break;
            }
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
                    torrent.use_interactive_fixture_size();
                    torrent.download_path = download_path.clone().or(torrent.download_path);
                    torrent.container_name = container_name.clone();
                    self.last_added_hash = Some(hex_encode(&torrent.info_hash));
                    session.upsert_mock_torrent(torrent.update());
                    self.insert(torrent);
                }
                BrowserCommand::Pause { info_hash_hex } => {
                    if let Some(torrent) = self.sessions.get_mut(info_hash_hex) {
                        let disconnected = torrent.last_reported_peer_ids.len();
                        for peer_id in torrent.last_reported_peer_ids.drain(..) {
                            let count = torrent
                                .peer_disconnect_counts
                                .entry(peer_id)
                                .or_default();
                            *count = count.saturating_add(1);
                        }
                        torrent.control_state = BrowserTorrentControlState::Paused;
                        torrent.clear_rate_averages();
                        session.upsert_mock_torrent(torrent.update());
                        if disconnected > 0 {
                            session.apply_mock_manager_events(BrowserManagerEventUpdate {
                                info_hash: torrent.info_hash.clone(),
                                peers_disconnected: disconnected,
                                ..BrowserManagerEventUpdate::default()
                            });
                        }
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
                BrowserCommand::FetchTorrentPreview {
                    browser_generation,
                    request_id,
                    path,
                } => {
                    let (name, protocol_version, files) = mock_torrent_preview(path);
                    let _ = session.apply_mock_torrent_preview(
                        *browser_generation,
                        *request_id,
                        path.clone(),
                        name,
                        protocol_version,
                        files,
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
                    torrent.use_interactive_fixture_size();
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
            session.refresh_mock_peer_manager();
            }
            fulfilled.extend(commands);
        }
        fulfilled
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
            let transfer_events = self
                .sessions
                .values_mut()
                .filter_map(MockTorrentSession::drain_transfer_events)
                .collect::<Vec<_>>();
            for update in transfer_events {
                session.apply_mock_manager_events(update);
            }
            self.elapsed_seconds += FIXED_STEP_SECONDS;
            self.publish_elapsed += FIXED_STEP_SECONDS;
            self.second_elapsed += FIXED_STEP_SECONDS;

            while self.publish_elapsed + FIXED_STEP_EPSILON >= HISTORY_SAMPLE_INTERVAL_SECONDS {
                self.publish_elapsed =
                    (self.publish_elapsed - HISTORY_SAMPLE_INTERVAL_SECONDS).max(0.0);
                self.record_torrent_samples();
            }
            self.publish_torrents(session);
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
    pub fn peer_ids_hex(&self, info_hash_hex: &str) -> Option<Vec<usize>> {
        self.sessions
            .get(info_hash_hex)
            .map(MockTorrentSession::peer_roster)
    }

    pub fn upload_recipient_count_hex(&self, info_hash_hex: &str) -> Option<usize> {
        self.sessions
            .get(info_hash_hex)
            .map(MockTorrentSession::upload_recipient_count)
    }

    #[cfg(test)]
    pub fn aggregate_peer_rates_hex(&self, info_hash_hex: &str) -> Option<(u64, u64)> {
        self.sessions.get(info_hash_hex).map(|torrent| {
            torrent.peers().iter().fold((0, 0), |totals, peer| {
                (
                    totals.0 + peer.download_speed_bps,
                    totals.1 + peer.upload_speed_bps,
                )
            })
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

    pub fn max_remote_peer_download_bps_hex(&self, info_hash_hex: &str) -> Option<u64> {
        self.sessions.get(info_hash_hex).map(|torrent| {
            let roster = torrent.peer_roster();
            let rates = roster
                .iter()
                .map(|peer_id| {
                    torrent
                        .peer_upload_rate_emas
                        .get(*peer_id)
                        .copied()
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>();
            normalized_ema_shares(torrent.upload_speed_bps(), &rates)
                .into_iter()
                .max()
                .unwrap_or_default()
        })
    }

    pub fn zero_progress_peer_count_hex(&self, info_hash_hex: &str) -> Option<usize> {
        self.sessions.get(info_hash_hex).map(|torrent| {
            if torrent.phase == MockTorrentPhase::Seeding {
                return torrent
                    .peer_roster()
                    .into_iter()
                    .filter(|peer_id| {
                        torrent
                            .peer_total_uploaded
                            .get(peer_id)
                            .copied()
                            .unwrap_or_default()
                            == 0
                    })
                    .count();
            }
            torrent
                .peer_roster()
                .into_iter()
                .filter(|peer_id| torrent.peer_bitfield(*peer_id).iter().all(|piece| !*piece))
                .count()
        })
    }

    pub fn peer_download_starts_hex(&self, info_hash_hex: &str) -> Option<u64> {
        self.sessions
            .get(info_hash_hex)
            .map(|torrent| torrent.peer_download_starts)
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

    fn record_torrent_samples(&mut self) {
        for torrent in self.sessions.values_mut() {
            torrent.record_sample();
        }
    }

    fn publish_torrents(&mut self, session: &mut BrowserSession) {
        let mut hashes = self.sessions.keys().cloned().collect::<Vec<_>>();
        hashes.sort_unstable();
        for hash in hashes {
            if let Some(torrent) = self.sessions.get_mut(&hash) {
                session.upsert_mock_torrent(torrent.update());
            }
        }
        session.refresh_mock_peer_manager();
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
                torrent.last_reported_peer_ids = torrent.peer_roster();
                continue;
            }
            let current_peer_ids = torrent.peer_roster();
            let peers_connected = current_peer_ids
                .iter()
                .filter(|peer_id| !torrent.last_reported_peer_ids.contains(peer_id))
                .count();
            let peers_disconnected = torrent
                .last_reported_peer_ids
                .iter()
                .filter(|peer_id| !current_peer_ids.contains(peer_id))
                .count();
            for peer_id in current_peer_ids
                .iter()
                .filter(|peer_id| !torrent.last_reported_peer_ids.contains(peer_id))
            {
                let count = torrent.peer_connection_counts.entry(*peer_id).or_default();
                *count = count.saturating_add(1);
            }
            for peer_id in torrent
                .last_reported_peer_ids
                .iter()
                .filter(|peer_id| !current_peer_ids.contains(peer_id))
            {
                let count = torrent.peer_disconnect_counts.entry(*peer_id).or_default();
                *count = count.saturating_add(1);
            }
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
            torrent.last_reported_peer_ids = current_peer_ids;
            session.apply_mock_manager_events(BrowserManagerEventUpdate {
                info_hash: torrent.info_hash.clone(),
                peers_discovered,
                peers_connected,
                peers_disconnected,
                disk_backoff_ms: if torrent.stall() == Some(MockStall::Disk)
                    || torrent.disk_state() != MockDiskState::Healthy
                {
                    45
                } else {
                    0
                },
                ..BrowserManagerEventUpdate::default()
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
    let total_weight = weights.iter().copied().sum::<u64>();
    if total_weight == 0 {
        return vec![0; weights.len()];
    }
    let last_weighted = weights.iter().rposition(|weight| *weight > 0).unwrap_or(0);
    let mut assigned = 0_u64;
    weights
        .iter()
        .enumerate()
        .map(|(index, weight)| {
            if index == last_weighted {
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
        .map(|rate| {
            if *rate <= f64::EPSILON {
                0
            } else {
                ((*rate / total_rate) * 1_000_000.0).max(1.0) as u64
            }
        })
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

fn mock_torrent_preview(path: &Path) -> (String, String, Vec<BrowserTorrentPreviewFile>) {
    let fixture = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let name = match fixture {
        "nested-fixture" => "Nested Aurora Set",
        "fixture-input" => "Aurora Packet Set",
        _ => "Incoming Demo Set",
    }
    .to_string();
    let files = vec![
        BrowserTorrentPreviewFile {
            relative_path: "bundle/segment-a.bin".to_string(),
            size: 12 * MIB,
        },
        BrowserTorrentPreviewFile {
            relative_path: "bundle/segment-b.bin".to_string(),
            size: 7 * MIB,
        },
        BrowserTorrentPreviewFile {
            relative_path: "bundle/notes.txt".to_string(),
            size: 12 * 1024,
        },
    ];
    (name, "v1 metainfo".to_string(), files)
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
