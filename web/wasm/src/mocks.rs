// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Browser-owned deterministic torrent simulation and command fulfillment.

use std::{collections::HashMap, path::Path, path::PathBuf};

use superseedr::web_integration::{
    BrowserCommand, BrowserFileTreeEntry, BrowserFileUpdate, BrowserJournalUpdate,
    BrowserPeerUpdate, BrowserRssUpdate, BrowserRuntimeTelemetryUpdate, BrowserSession,
    BrowserTelemetryUpdate, BrowserTorrentControlState, BrowserTorrentUpdate,
};

const METADATA_SECONDS: f64 = 0.6;
const PEER_DISCOVERY_SECONDS: f64 = 0.8;
const CHECKING_SECONDS: f64 = 0.7;
const MAX_STEP_SECONDS: f64 = 0.1;
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
    total_size: u64,
    pieces_total: u32,
    bytes_written: u64,
    session_downloaded: u64,
    session_uploaded: u64,
    tick: u64,
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
            total_size,
            pieces_total,
            bytes_written,
            session_downloaded: bytes_written,
            session_uploaded: bytes_written / 14,
            tick: 0,
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
        }
    }

    fn advance(&mut self, delta_seconds: f64) {
        if !matches!(self.control_state, BrowserTorrentControlState::Running) {
            return;
        }

        self.tick = self.tick.saturating_add(1);
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
            MockTorrentPhase::Downloading => {
                let downloaded = (self.download_speed_bps() as f64 * delta_seconds) as u64;
                self.bytes_written = self
                    .bytes_written
                    .saturating_add(downloaded)
                    .min(self.total_size);
                self.session_downloaded = self.session_downloaded.saturating_add(downloaded);
                self.session_uploaded = self
                    .session_uploaded
                    .saturating_add((self.upload_speed_bps() as f64 * delta_seconds) as u64);
                if self.bytes_written >= self.total_size {
                    self.phase = MockTorrentPhase::CheckingPieces;
                    self.phase_elapsed = 0.0;
                }
            }
            MockTorrentPhase::CheckingPieces if self.phase_elapsed >= CHECKING_SECONDS => {
                self.phase = MockTorrentPhase::Seeding;
                self.phase_elapsed = 0.0;
            }
            MockTorrentPhase::Seeding => {
                self.session_uploaded = self
                    .session_uploaded
                    .saturating_add((self.upload_speed_bps() as f64 * delta_seconds) as u64);
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

    fn download_speed_bps(&self) -> u64 {
        if !matches!(self.control_state, BrowserTorrentControlState::Running)
            || self.phase != MockTorrentPhase::Downloading
            || self.stall().is_some()
        {
            return 0;
        }
        let wave = (self.tick + self.seed) % 7;
        (18 + wave * 2) * MIB
    }

    fn upload_speed_bps(&self) -> u64 {
        if !matches!(self.control_state, BrowserTorrentControlState::Running) {
            return 0;
        }
        match self.phase {
            MockTorrentPhase::Downloading if self.stall().is_none() => {
                (320 + (self.tick + self.seed) % 5 * 96) * 1024
            }
            MockTorrentPhase::Seeding => (2 + (self.tick + self.seed) % 3) * MIB,
            _ => 0,
        }
    }

    fn disk_rates(&self) -> (u64, u64) {
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
        match self.phase {
            MockTorrentPhase::FetchingMetadata => 0,
            MockTorrentPhase::DiscoveringPeers => ((self.phase_elapsed / PEER_DISCOVERY_SECONDS
                * self.peer_goal as f64)
                .ceil() as usize)
                .min(self.peer_goal),
            MockTorrentPhase::Downloading if self.stall() == Some(MockStall::Peer) => 1,
            MockTorrentPhase::Downloading
            | MockTorrentPhase::CheckingPieces
            | MockTorrentPhase::Seeding => self.peer_goal,
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
        (0..count)
            .map(|index| {
                let divisor = count.max(1) as u64;
                let active = download_speed > 0 || upload_speed > 0;
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
                    download_speed_bps: download_speed / divisor,
                    upload_speed_bps: upload_speed / divisor,
                    total_downloaded: self.session_downloaded / divisor,
                    total_uploaded: self.session_uploaded / divisor,
                    bitfield: self.peer_bitfield(index),
                    active,
                }
            })
            .collect()
    }

    fn peer_bitfield(&self, offset: usize) -> Vec<bool> {
        if !self.metadata_available() {
            return Vec::new();
        }
        if self.phase == MockTorrentPhase::Seeding {
            return vec![true; self.pieces_total as usize];
        }
        (0..self.pieces_total as usize)
            .map(|piece| !(piece + offset + self.seed as usize).is_multiple_of(5))
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
        let peer_count = self.peer_count() as u64;
        let download_speed_bps = self.download_speed_bps();
        let upload_speed_bps = self.upload_speed_bps();
        let peer_stalled = self.stall() == Some(MockStall::Peer);
        push_history(&mut self.download_history, download_speed_bps);
        push_history(&mut self.upload_history, upload_speed_bps);
        push_history(
            &mut self.blocks_in_history,
            download_speed_bps / (16 * 1024),
        );
        push_history(&mut self.blocks_out_history, upload_speed_bps / (16 * 1024));
        push_history(
            &mut self.peer_discovery_history,
            if self.phase == MockTorrentPhase::DiscoveringPeers {
                peer_count
            } else {
                0
            },
        );
        push_history(
            &mut self.peer_connection_history,
            if peer_stalled { 0 } else { peer_count },
        );
        push_history(&mut self.peer_disconnect_history, u64::from(peer_stalled));
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
    sessions: HashMap<String, MockTorrentSession>,
    next_torrent_id: u8,
    elapsed_seconds: f64,
    publish_elapsed: f64,
    last_added_hash: Option<String>,
    total_download_history: Vec<u64>,
    total_upload_history: Vec<u64>,
    disk_read_history: Vec<u64>,
    disk_write_history: Vec<u64>,
    disk_backoff_history_ms: Vec<u64>,
}

impl Default for DemoCommandService {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            next_torrent_id: 0xb0,
            elapsed_seconds: 0.0,
            publish_elapsed: 0.0,
            last_added_hash: None,
            total_download_history: vec![0],
            total_upload_history: vec![0],
            disk_read_history: vec![0],
            disk_write_history: vec![0],
            disk_backoff_history_ms: vec![0],
        }
    }
}

impl DemoCommandService {
    pub fn install_initial_state(&mut self, session: &mut BrowserSession) {
        if self.sessions.is_empty() {
            for initial in initial_sessions() {
                self.insert(initial);
            }
        }
        self.publish(session);
        install_supporting_views(session);
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

    pub fn advance(&mut self, session: &mut BrowserSession, delta_seconds: f64) {
        let delta_seconds = delta_seconds.clamp(0.0, 30.0);
        if delta_seconds == 0.0 {
            return;
        }
        let mut remaining = delta_seconds;
        while remaining > f64::EPSILON {
            let step = remaining.min(MAX_STEP_SECONDS);
            for torrent in self.sessions.values_mut() {
                torrent.advance(step);
            }
            self.elapsed_seconds += step;
            remaining -= step;
        }
        self.publish_elapsed += delta_seconds;
        if self.publish_elapsed >= PUBLISH_INTERVAL_SECONDS
            || delta_seconds >= PUBLISH_INTERVAL_SECONDS
        {
            self.publish_elapsed = 0.0;
            self.publish(session);
        }
        session.advance_mock_visualizations(delta_seconds);
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
        let mut hashes = self.sessions.keys().cloned().collect::<Vec<_>>();
        hashes.sort_unstable();
        for hash in hashes {
            if let Some(torrent) = self.sessions.get_mut(&hash) {
                torrent.record_sample();
                session.upsert_mock_torrent(torrent.update());
            }
        }

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
        let has_disk_backoff = self
            .sessions
            .values()
            .any(|torrent| torrent.stall() == Some(MockStall::Disk));
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
}

fn initial_sessions() -> Vec<MockTorrentSession> {
    let mut metadata = initial_session(
        0x5a,
        "Nebula Field Sample",
        MockTorrentPhase::FetchingMetadata,
        0.0,
    );
    metadata.download_path = Some(PathBuf::from("/simulated/downloads/set-0"));

    let mut downloading = initial_session(
        0x6b,
        "Orbit Archive 02",
        MockTorrentPhase::Downloading,
        0.36,
    );
    downloading.phase_elapsed = 0.4;

    let mut stalled = initial_session(0x7c, "Lattice Study", MockTorrentPhase::Downloading, 0.36);
    stalled.phase_elapsed = 1.5;

    let checking = initial_session(0x8d, "Prism Notes", MockTorrentPhase::CheckingPieces, 1.0);
    let seeding = initial_session(0x9e, "Signal Garden", MockTorrentPhase::Seeding, 1.0);
    let mut deleting = initial_session(0xaf, "Vector Almanac", MockTorrentPhase::Seeding, 1.0);
    deleting.control_state = BrowserTorrentControlState::Deleting;

    vec![metadata, downloading, stalled, checking, seeding, deleting]
}

fn initial_session(
    byte: u8,
    name: &str,
    phase: MockTorrentPhase,
    progress: f64,
) -> MockTorrentSession {
    let mut torrent = MockTorrentSession::new(
        vec![byte; 20],
        name.to_string(),
        format!("magnet:?xt=urn:btih:{}", hex_byte(byte)),
        phase,
        progress,
    );
    torrent.download_path = Some(PathBuf::from(format!(
        "/simulated/downloads/set-{:x}",
        byte & 0x0f
    )));
    torrent.container_name = Some(format!("collection-{:x}", byte & 0x0f));
    torrent
}

fn install_supporting_views(session: &mut BrowserSession) {
    session.apply_mock_telemetry(BrowserTelemetryUpdate {
        cpu_usage: 17.5,
        ram_usage_percent: 42.0,
        app_ram_usage: 96 * MIB,
        run_time: 7_321,
        total_download_history: vec![300_000, 900_000, 1_800_000, 2_700_000, 3_200_000],
        total_upload_history: vec![40_000, 64_000, 88_000, 104_000, 128_000],
        disk_read_history: vec![400_000, 700_000, 1_000_000, 1_400_000],
        disk_write_history: vec![900_000, 1_600_000, 2_100_000, 2_800_000],
        disk_read_bps: 1_400_000,
        disk_write_bps: 2_800_000,
        disk_backoff_history_ms: vec![0, 3, 0, 7, 1],
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
        journal: vec![
            BrowserJournalUpdate {
                timestamp: "2026-08-30T12:00:00Z".to_string(),
                torrent_name: Some("Signal Garden".to_string()),
                message: "Simulated metadata resolved".to_string(),
            },
            BrowserJournalUpdate {
                timestamp: "2026-08-30T12:03:00Z".to_string(),
                torrent_name: Some("Prism Notes".to_string()),
                message: "Simulated piece check completed".to_string(),
            },
        ],
        rss: vec![BrowserRssUpdate {
            feed_url: "https://feed.invalid/simulated.xml".to_string(),
            filter_query: "signal garden".to_string(),
            item_title: "Signal Garden Dispatch".to_string(),
            item_link: "magnet:?xt=urn:btih:b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0".to_string(),
            timestamp: "2026-08-30T12:04:00Z".to_string(),
        }],
    });
}

#[cfg(test)]
pub fn install_simulated_state(session: &mut BrowserSession) {
    DemoCommandService::default().install_initial_state(session);
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
