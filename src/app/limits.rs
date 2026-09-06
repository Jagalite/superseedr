// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application limits definitions and transitions.

use super::*;

pub const RSS_MAX_TORRENT_DOWNLOAD_BYTES: usize = 10 * 1024 * 1024;
pub(super) const NETWORK_HISTORY_PERSIST_INTERVAL_SECS: u64 = 15 * 60;
pub(super) const SHARED_RECOVERY_BACKUP_REFRESH_INTERVAL_SECS: u64 = 15 * 60;
pub(super) const WATCH_FOLDER_RESCAN_INTERVAL_SECS: u64 = 5;
pub(super) const SHARED_ROLE_RETRY_INTERVAL_SECS: u64 = 2;
pub(super) const STARTUP_ROLLING_BATCH_INTERVAL_SECS: u64 = 1;
pub(super) const STARTUP_ROLLING_LOADS_PER_INTERVAL: usize = 1;
pub(super) const REPEATED_HEALTH_LOG_INTERVAL: Duration = Duration::from_secs(60);

pub(super) const SHUTDOWN_TIMEOUT_SECS: u64 = 20;
pub(super) const INCOMING_HANDSHAKE_TIMEOUT_SECS: u64 = 10;
// DHT owns a one-second transport-drain budget during reconfiguration. Keep
// the app-level liveness bound comfortably outside that healthy inner path.
pub(super) const PORT_REBIND_DHT_TIMEOUT: Duration = Duration::from_secs(3);
pub(super) const INCOMING_PEER_HANDSHAKE_QUEUE_SIZE: usize = 1024;
pub(super) const PORT_FAMILY_HIGHLIGHT_DURATION: Duration = Duration::from_millis(450);
pub(super) const DUAL_STACK_EPHEMERAL_BIND_ATTEMPTS: usize = 16;
pub(super) const UI_FPS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const UI_RESPONSIVENESS_EMA_ALPHA: f64 = 0.35;
pub(super) const WAKE_LAG_PEER_THROTTLE_BAD_RATIO: f64 = 0.25;
pub(super) const WAKE_LAG_PEER_THROTTLE_BAD_MIN_DELAY: Duration = Duration::from_millis(20);
pub(super) const WAKE_LAG_PEER_THROTTLE_GOOD_RATIO: f64 = 0.12;
pub(super) const WAKE_LAG_PEER_THROTTLE_GOOD_TICKS: u8 = 3;
pub(super) const WAKE_LAG_PEER_THROTTLE_ADDITIVE_STEP_PEERS: usize = 256;
pub(super) const WAKE_LAG_PEER_THROTTLE_ADDITIVE_STEP_PERCENT: usize = 10;
pub(super) const WAKE_LAG_PEER_THROTTLE_RECOVERY_HEADROOM_PEERS: usize = 512;
pub(super) const WAKE_LAG_PEER_THROTTLE_MIN_PEERS: usize = 8;
pub(super) const WAKE_LAG_PEER_THROTTLE_DOWNLOAD_FLOOR_PERCENT: usize = 25;
pub(super) const NORMAL_IDLE_FRAME_CHECK_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const NORMAL_ANIMATION_RECENT_BLOCK_ROWS: usize = 64;
pub(super) const NORMAL_ANIMATION_RECENT_PEER_EVENTS: usize = 120;
pub(super) const NORMAL_ANIMATION_FILE_ACTIVITY_WINDOW: Duration = Duration::from_secs(4);
pub(super) const SWARM_AVAILABILITY_FLASH_DURATION: Duration = Duration::from_millis(350);
pub(super) const DISK_WRITE_THROTTLE_START_BYTES_PER_SEC: f64 = 1_000_000_000.0 / 8.0;
pub(super) const DISK_WRITE_THROTTLE_MIN_BYTES_PER_SEC: f64 = 1_000_000.0 / 8.0;
pub(super) const DISK_WRITE_THROTTLE_WINDOW_TICKS: u8 = 5;
pub(super) const DISK_WRITE_THROTTLE_STEP_MIN: f64 = 0.80;
pub(super) const DISK_WRITE_THROTTLE_STEP_MAX: f64 = 1.20;
pub(super) const DISK_WRITE_THROTTLE_BURST_SECS: f64 = 1.0;
pub(super) const DISK_WRITE_THROTTLE_TARGET_LATENCY_SECS: f64 = 2.0;
pub(super) const BITTORRENT_PROTOCOL_STR: &[u8] = b"BitTorrent protocol";
