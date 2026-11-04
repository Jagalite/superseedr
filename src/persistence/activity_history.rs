// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::persistence::network_history::{
    HOUR_1H_CAP, MINUTE_15M_CAP, MINUTE_1M_CAP, SECOND_1S_CAP,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, Cursor, Read};

pub const ACTIVITY_HISTORY_SCHEMA_VERSION: u32 = 3;
// Connected peer counts use thousandths to preserve fractional rollup averages.
pub const PEER_COUNT_SCALE: u64 = 1_000;
pub(super) const ACTIVITY_HISTORY_FILE_NAME: &str = "activity_history.bin";
const ACTIVITY_HISTORY_MAGIC: &[u8; 8] = b"SSAHBIN1";
const MAX_ACTIVITY_HISTORY_TORRENTS: usize = 100_000;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ActivityHistoryPoint {
    pub ts_unix: u64,
    pub primary: u64,
    pub secondary: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ActivityHistoryTiers {
    pub second_1s: Vec<ActivityHistoryPoint>,
    pub minute_1m: Vec<ActivityHistoryPoint>,
    pub minute_15m: Vec<ActivityHistoryPoint>,
    pub hour_1h: Vec<ActivityHistoryPoint>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct PersistedRollupAccumulator {
    pub count: u32,
    pub primary_sum: u128,
    pub secondary_sum: u128,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ActivityHistoryRollupSnapshot {
    pub second_to_minute: PersistedRollupAccumulator,
    pub minute_to_15m: PersistedRollupAccumulator,
    pub m15_to_hour: PersistedRollupAccumulator,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ActivityHistorySeries {
    pub rollups: ActivityHistoryRollupSnapshot,
    pub tiers: ActivityHistoryTiers,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct ActivityHistoryPersistedState {
    pub schema_version: u32,
    pub updated_at_unix: u64,
    pub cpu: ActivityHistorySeries,
    pub ram: ActivityHistorySeries,
    pub disk: ActivityHistorySeries,
    pub tuning: ActivityHistorySeries,
    pub peers: ActivityHistorySeries,
    pub torrents: HashMap<String, ActivityHistorySeries>,
}

impl ActivityHistoryPersistedState {
    /// Versions 1 and 2 stored RAM as tenths of a percentage, not bytes.
    pub(crate) fn upgrade_ram_units(&mut self) {
        if self.schema_version < 3 {
            self.ram = ActivityHistorySeries::default();
            self.schema_version = ACTIVITY_HISTORY_SCHEMA_VERSION;
        }
    }
}

impl Default for ActivityHistoryPersistedState {
    fn default() -> Self {
        Self {
            schema_version: ACTIVITY_HISTORY_SCHEMA_VERSION,
            updated_at_unix: 0,
            cpu: ActivityHistorySeries::default(),
            ram: ActivityHistorySeries::default(),
            disk: ActivityHistorySeries::default(),
            tuning: ActivityHistorySeries::default(),
            peers: ActivityHistorySeries::default(),
            torrents: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RollupAccumulator {
    count: u32,
    primary_sum: u128,
    secondary_sum: u128,
}

impl RollupAccumulator {
    fn push(&mut self, point: &ActivityHistoryPoint) {
        self.count += 1;
        self.primary_sum += point.primary as u128;
        self.secondary_sum += point.secondary as u128;
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

impl From<&RollupAccumulator> for PersistedRollupAccumulator {
    fn from(accumulator: &RollupAccumulator) -> Self {
        Self {
            count: accumulator.count,
            primary_sum: accumulator.primary_sum,
            secondary_sum: accumulator.secondary_sum,
        }
    }
}

impl From<&PersistedRollupAccumulator> for RollupAccumulator {
    fn from(accumulator: &PersistedRollupAccumulator) -> Self {
        Self {
            count: accumulator.count,
            primary_sum: accumulator.primary_sum,
            secondary_sum: accumulator.secondary_sum,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityHistorySeriesRollupState {
    second_to_minute: RollupAccumulator,
    minute_to_15m: RollupAccumulator,
    m15_to_hour: RollupAccumulator,
}

impl ActivityHistorySeriesRollupState {
    pub fn to_snapshot(&self) -> ActivityHistoryRollupSnapshot {
        ActivityHistoryRollupSnapshot {
            second_to_minute: PersistedRollupAccumulator::from(&self.second_to_minute),
            minute_to_15m: PersistedRollupAccumulator::from(&self.minute_to_15m),
            m15_to_hour: PersistedRollupAccumulator::from(&self.m15_to_hour),
        }
    }

    pub fn from_snapshot(snapshot: &ActivityHistoryRollupSnapshot) -> Self {
        Self {
            second_to_minute: RollupAccumulator::from(&snapshot.second_to_minute),
            minute_to_15m: RollupAccumulator::from(&snapshot.minute_to_15m),
            m15_to_hour: RollupAccumulator::from(&snapshot.m15_to_hour),
        }
    }

    pub fn ingest_second_sample(
        &mut self,
        series: &mut ActivityHistorySeries,
        ts_unix: u64,
        primary: u64,
        secondary: u64,
    ) -> bool {
        let second_point = ActivityHistoryPoint {
            ts_unix,
            primary,
            secondary,
        };
        let mut should_persist = !is_zero_point(&second_point);
        push_retained(
            &mut series.tiers.second_1s,
            second_point.clone(),
            SECOND_1S_CAP,
        );

        self.second_to_minute.push(&second_point);
        if self.second_to_minute.count >= 60 {
            let minute_point = make_rollup_point(&self.second_to_minute, ts_unix);
            self.second_to_minute.clear();
            should_persist |= !is_zero_point(&minute_point);

            push_retained(
                &mut series.tiers.minute_1m,
                minute_point.clone(),
                MINUTE_1M_CAP,
            );

            self.minute_to_15m.push(&minute_point);
            if self.minute_to_15m.count >= 15 {
                let m15_point = make_rollup_point(&self.minute_to_15m, ts_unix);
                self.minute_to_15m.clear();
                should_persist |= !is_zero_point(&m15_point);

                push_retained(
                    &mut series.tiers.minute_15m,
                    m15_point.clone(),
                    MINUTE_15M_CAP,
                );

                self.m15_to_hour.push(&m15_point);
                if self.m15_to_hour.count >= 4 {
                    let hour_point = make_rollup_point(&self.m15_to_hour, ts_unix);
                    self.m15_to_hour.clear();
                    should_persist |= !is_zero_point(&hour_point);

                    push_retained(&mut series.tiers.hour_1h, hour_point, HOUR_1H_CAP);
                }
            }
        }

        series.rollups = self.to_snapshot();
        should_persist
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityHistoryRollupState {
    pub cpu: ActivityHistorySeriesRollupState,
    pub ram: ActivityHistorySeriesRollupState,
    pub disk: ActivityHistorySeriesRollupState,
    pub tuning: ActivityHistorySeriesRollupState,
    pub peers: ActivityHistorySeriesRollupState,
    pub torrents: HashMap<String, ActivityHistorySeriesRollupState>,
}

impl ActivityHistoryRollupState {
    pub fn from_persisted(state: &ActivityHistoryPersistedState) -> Self {
        let torrents = state
            .torrents
            .iter()
            .map(|(info_hash, series)| {
                (
                    info_hash.clone(),
                    ActivityHistorySeriesRollupState::from_snapshot(&series.rollups),
                )
            })
            .collect();
        Self {
            cpu: ActivityHistorySeriesRollupState::from_snapshot(&state.cpu.rollups),
            ram: ActivityHistorySeriesRollupState::from_snapshot(&state.ram.rollups),
            disk: ActivityHistorySeriesRollupState::from_snapshot(&state.disk.rollups),
            tuning: ActivityHistorySeriesRollupState::from_snapshot(&state.tuning.rollups),
            peers: ActivityHistorySeriesRollupState::from_snapshot(&state.peers.rollups),
            torrents,
        }
    }

    pub fn sync_snapshots_to_state(&self, state: &mut ActivityHistoryPersistedState) {
        state.cpu.rollups = self.cpu.to_snapshot();
        state.ram.rollups = self.ram.to_snapshot();
        state.disk.rollups = self.disk.to_snapshot();
        state.tuning.rollups = self.tuning.to_snapshot();
        state.peers.rollups = self.peers.to_snapshot();
        for (info_hash, rollups) in &self.torrents {
            if let Some(series) = state.torrents.get_mut(info_hash) {
                series.rollups = rollups.to_snapshot();
            }
        }
    }
}

fn make_rollup_point(acc: &RollupAccumulator, ts_unix: u64) -> ActivityHistoryPoint {
    if acc.count == 0 {
        return ActivityHistoryPoint {
            ts_unix,
            ..Default::default()
        };
    }
    ActivityHistoryPoint {
        ts_unix,
        primary: (acc.primary_sum / acc.count as u128) as u64,
        secondary: (acc.secondary_sum / acc.count as u128) as u64,
    }
}

// Keep reservation headroom small for the many independently growing series.
const HISTORY_RESERVE_CHUNK: usize = 64;

fn push_retained<T>(vec: &mut Vec<T>, point: T, cap: usize) {
    if cap == 0 {
        vec.clear();
        return;
    }
    // Trim before pushing so a full tier never grows just to evict its oldest point.
    // This also preserves push-then-trim behavior for an already oversized tier.
    cap_vec(vec, cap - 1);
    if vec.len() == vec.capacity() {
        let additional = vec
            .len()
            .clamp(4, HISTORY_RESERVE_CHUNK)
            .min(cap - vec.len());
        vec.reserve_exact(additional);
    }
    vec.push(point);
}

fn cap_vec<T>(vec: &mut Vec<T>, cap: usize) {
    if vec.len() > cap {
        let overflow = vec.len() - cap;
        vec.drain(0..overflow);
    }
}

pub fn enforce_retention_caps(state: &mut ActivityHistoryPersistedState) {
    cap_series(&mut state.cpu);
    cap_series(&mut state.ram);
    cap_series(&mut state.disk);
    cap_series(&mut state.tuning);
    cap_series(&mut state.peers);
    for series in state.torrents.values_mut() {
        cap_series(series);
    }
}

pub fn retain_only_torrent_series_for_keys(
    state: &mut ActivityHistoryPersistedState,
    rollups: &mut ActivityHistoryRollupState,
    keep_keys: &HashSet<String>,
) {
    state.torrents.retain(|key, _| keep_keys.contains(key));
    rollups.torrents.retain(|key, _| keep_keys.contains(key));
}

fn cap_series(series: &mut ActivityHistorySeries) {
    cap_vec(&mut series.tiers.second_1s, SECOND_1S_CAP);
    cap_vec(&mut series.tiers.minute_1m, MINUTE_1M_CAP);
    cap_vec(&mut series.tiers.minute_15m, MINUTE_15M_CAP);
    cap_vec(&mut series.tiers.hour_1h, HOUR_1H_CAP);
}

pub fn is_zero_point(point: &ActivityHistoryPoint) -> bool {
    point.primary == 0 && point.secondary == 0
}

fn sparse_points_for_persistence(points: &[ActivityHistoryPoint]) -> Vec<ActivityHistoryPoint> {
    // Keep coverage boundaries, including idle samples, so sparse files can
    // reconstruct how much history was recorded after a restart.
    points
        .iter()
        .enumerate()
        .filter(|(index, point)| *index == 0 || *index + 1 == points.len() || !is_zero_point(point))
        .map(|(_, point)| point.clone())
        .collect()
}

fn sparse_series_for_persistence(series: &ActivityHistorySeries) -> ActivityHistorySeries {
    ActivityHistorySeries {
        rollups: series.rollups.clone(),
        tiers: ActivityHistoryTiers {
            second_1s: sparse_points_for_persistence(&series.tiers.second_1s),
            minute_1m: sparse_points_for_persistence(&series.tiers.minute_1m),
            minute_15m: sparse_points_for_persistence(&series.tiers.minute_15m),
            hour_1h: sparse_points_for_persistence(&series.tiers.hour_1h),
        },
    }
}

pub(crate) fn sparse_state_for_persistence(
    state: &ActivityHistoryPersistedState,
) -> ActivityHistoryPersistedState {
    let mut sparse = ActivityHistoryPersistedState {
        schema_version: state.schema_version,
        updated_at_unix: state.updated_at_unix,
        cpu: sparse_series_for_persistence(&state.cpu),
        ram: sparse_series_for_persistence(&state.ram),
        disk: sparse_series_for_persistence(&state.disk),
        tuning: sparse_series_for_persistence(&state.tuning),
        peers: sparse_series_for_persistence(&state.peers),
        torrents: HashMap::new(),
    };

    for (info_hash, series) in &state.torrents {
        let sparse_series = sparse_series_for_persistence(series);
        if has_any_point(&sparse_series) {
            sparse.torrents.insert(info_hash.clone(), sparse_series);
        }
    }

    sparse
}

fn has_any_point(series: &ActivityHistorySeries) -> bool {
    !series.tiers.second_1s.is_empty()
        || !series.tiers.minute_1m.is_empty()
        || !series.tiers.minute_15m.is_empty()
        || !series.tiers.hour_1h.is_empty()
}

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused_imports)]
pub use native::{
    activity_history_state_file_path, load_activity_history_state, save_activity_history_state,
};
#[cfg(test)]
use native::{load_activity_history_state_from_path, save_activity_history_state_to_path};

fn encode_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn encode_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn encode_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn encode_u128(buf: &mut Vec<u8>, value: u128) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn decode_u16(cursor: &mut Cursor<&[u8]>) -> io::Result<u16> {
    let mut bytes = [0_u8; 2];
    cursor.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn decode_u32(cursor: &mut Cursor<&[u8]>) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    cursor.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn decode_u64(cursor: &mut Cursor<&[u8]>) -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    cursor.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn decode_u128(cursor: &mut Cursor<&[u8]>) -> io::Result<u128> {
    let mut bytes = [0_u8; 16];
    cursor.read_exact(&mut bytes)?;
    Ok(u128::from_le_bytes(bytes))
}

fn encode_rollup_accumulator(buf: &mut Vec<u8>, accumulator: &PersistedRollupAccumulator) {
    encode_u32(buf, accumulator.count);
    encode_u128(buf, accumulator.primary_sum);
    encode_u128(buf, accumulator.secondary_sum);
}

fn decode_rollup_accumulator(cursor: &mut Cursor<&[u8]>) -> io::Result<PersistedRollupAccumulator> {
    Ok(PersistedRollupAccumulator {
        count: decode_u32(cursor)?,
        primary_sum: decode_u128(cursor)?,
        secondary_sum: decode_u128(cursor)?,
    })
}

fn encode_points(buf: &mut Vec<u8>, points: &[ActivityHistoryPoint]) {
    encode_u32(buf, points.len() as u32);
    for point in points {
        encode_u64(buf, point.ts_unix);
        encode_u64(buf, point.primary);
        encode_u64(buf, point.secondary);
    }
}

fn decode_points(
    cursor: &mut Cursor<&[u8]>,
    max_points: usize,
) -> io::Result<Vec<ActivityHistoryPoint>> {
    let count = decode_u32(cursor)? as usize;
    if count > max_points {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activity history tier exceeds retention cap",
        ));
    }

    let mut points = Vec::with_capacity(count);
    for _ in 0..count {
        points.push(ActivityHistoryPoint {
            ts_unix: decode_u64(cursor)?,
            primary: decode_u64(cursor)?,
            secondary: decode_u64(cursor)?,
        });
    }

    Ok(points)
}

fn encode_series(buf: &mut Vec<u8>, series: &ActivityHistorySeries) {
    encode_rollup_accumulator(buf, &series.rollups.second_to_minute);
    encode_rollup_accumulator(buf, &series.rollups.minute_to_15m);
    encode_rollup_accumulator(buf, &series.rollups.m15_to_hour);
    encode_points(buf, &series.tiers.second_1s);
    encode_points(buf, &series.tiers.minute_1m);
    encode_points(buf, &series.tiers.minute_15m);
    encode_points(buf, &series.tiers.hour_1h);
}

fn decode_series(cursor: &mut Cursor<&[u8]>) -> io::Result<ActivityHistorySeries> {
    Ok(ActivityHistorySeries {
        rollups: ActivityHistoryRollupSnapshot {
            second_to_minute: decode_rollup_accumulator(cursor)?,
            minute_to_15m: decode_rollup_accumulator(cursor)?,
            m15_to_hour: decode_rollup_accumulator(cursor)?,
        },
        tiers: ActivityHistoryTiers {
            second_1s: decode_points(cursor, SECOND_1S_CAP)?,
            minute_1m: decode_points(cursor, MINUTE_1M_CAP)?,
            minute_15m: decode_points(cursor, MINUTE_15M_CAP)?,
            hour_1h: decode_points(cursor, HOUR_1H_CAP)?,
        },
    })
}

fn encode_string(buf: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    encode_u16(buf, bytes.len() as u16);
    buf.extend_from_slice(bytes);
}

fn decode_string(cursor: &mut Cursor<&[u8]>) -> io::Result<String> {
    let len = decode_u16(cursor)? as usize;
    let mut bytes = vec![0_u8; len];
    cursor.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(super) fn encode_activity_history_state(state: &ActivityHistoryPersistedState) -> Vec<u8> {
    let mut torrents: Vec<_> = state.torrents.iter().collect();
    torrents.sort_by_key(|(left, _)| *left);

    let mut buf = Vec::new();
    buf.extend_from_slice(ACTIVITY_HISTORY_MAGIC);
    encode_u32(&mut buf, ACTIVITY_HISTORY_SCHEMA_VERSION);
    encode_u64(&mut buf, state.updated_at_unix);
    encode_series(&mut buf, &state.cpu);
    encode_series(&mut buf, &state.ram);
    encode_series(&mut buf, &state.disk);
    encode_series(&mut buf, &state.tuning);
    encode_u32(&mut buf, torrents.len() as u32);
    for (info_hash, series) in torrents {
        encode_string(&mut buf, info_hash);
        encode_series(&mut buf, series);
    }
    encode_series(&mut buf, &state.peers);
    buf
}

pub(super) fn decode_activity_history_state(
    bytes: &[u8],
) -> io::Result<ActivityHistoryPersistedState> {
    let mut cursor = Cursor::new(bytes);
    let mut magic = [0_u8; ACTIVITY_HISTORY_MAGIC.len()];
    cursor.read_exact(&mut magic)?;
    if &magic != ACTIVITY_HISTORY_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid activity history binary header",
        ));
    }

    let schema_version = decode_u32(&mut cursor)?;
    if !(1..=ACTIVITY_HISTORY_SCHEMA_VERSION).contains(&schema_version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported activity history schema version {schema_version}"),
        ));
    }

    let updated_at_unix = decode_u64(&mut cursor)?;
    let cpu = decode_series(&mut cursor)?;
    let ram = decode_series(&mut cursor)?;
    let disk = decode_series(&mut cursor)?;
    let tuning = decode_series(&mut cursor)?;
    let torrent_count = decode_u32(&mut cursor)? as usize;
    if torrent_count > MAX_ACTIVITY_HISTORY_TORRENTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activity history torrent count exceeds decoder limit",
        ));
    }
    let mut torrents = HashMap::with_capacity(torrent_count);
    for _ in 0..torrent_count {
        let info_hash = decode_string(&mut cursor)?;
        let series = decode_series(&mut cursor)?;
        torrents.insert(info_hash, series);
    }

    let peers = if schema_version >= 2 {
        decode_series(&mut cursor)?
    } else {
        ActivityHistorySeries::default()
    };

    if cursor.position() != bytes.len() as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes in activity history binary payload",
        ));
    }

    let mut state = ActivityHistoryPersistedState {
        schema_version,
        updated_at_unix,
        cpu,
        ram,
        disk,
        tuning,
        peers,
        torrents,
    };
    state.upgrade_ram_units();
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Frozen push-then-trim implementation from the parent revision.
    fn legacy_ingest_second_sample(
        rollups: &mut ActivityHistorySeriesRollupState,
        series: &mut ActivityHistorySeries,
        ts_unix: u64,
        primary: u64,
        secondary: u64,
    ) -> bool {
        let second_point = ActivityHistoryPoint {
            ts_unix,
            primary,
            secondary,
        };
        let mut should_persist = !is_zero_point(&second_point);
        series.tiers.second_1s.push(second_point.clone());
        cap_vec(&mut series.tiers.second_1s, SECOND_1S_CAP);

        rollups.second_to_minute.push(&second_point);
        if rollups.second_to_minute.count >= 60 {
            let minute_point = make_rollup_point(&rollups.second_to_minute, ts_unix);
            rollups.second_to_minute.clear();
            should_persist |= !is_zero_point(&minute_point);

            series.tiers.minute_1m.push(minute_point.clone());
            cap_vec(&mut series.tiers.minute_1m, MINUTE_1M_CAP);

            rollups.minute_to_15m.push(&minute_point);
            if rollups.minute_to_15m.count >= 15 {
                let m15_point = make_rollup_point(&rollups.minute_to_15m, ts_unix);
                rollups.minute_to_15m.clear();
                should_persist |= !is_zero_point(&m15_point);

                series.tiers.minute_15m.push(m15_point.clone());
                cap_vec(&mut series.tiers.minute_15m, MINUTE_15M_CAP);

                rollups.m15_to_hour.push(&m15_point);
                if rollups.m15_to_hour.count >= 4 {
                    let hour_point = make_rollup_point(&rollups.m15_to_hour, ts_unix);
                    rollups.m15_to_hour.clear();
                    should_persist |= !is_zero_point(&hour_point);

                    series.tiers.hour_1h.push(hour_point);
                    cap_vec(&mut series.tiers.hour_1h, HOUR_1H_CAP);
                }
            }
        }

        series.rollups = rollups.to_snapshot();
        should_persist
    }

    #[test]
    fn version_two_resets_percentage_ram_but_preserves_other_history() {
        let mut state = ActivityHistoryPersistedState::default();
        let series = ActivityHistorySeries {
            tiers: ActivityHistoryTiers {
                second_1s: vec![ActivityHistoryPoint {
                    ts_unix: 100,
                    primary: 450,
                    secondary: 0,
                }],
                ..Default::default()
            },
            rollups: ActivityHistoryRollupSnapshot {
                second_to_minute: PersistedRollupAccumulator {
                    count: 1,
                    primary_sum: 450,
                    secondary_sum: 0,
                },
                ..Default::default()
            },
        };
        state.cpu = series.clone();
        state.ram = series.clone();
        state.peers = series.clone();
        let mut bytes = encode_activity_history_state(&state);
        bytes[8..12].copy_from_slice(&2_u32.to_le_bytes());
        let loaded = decode_activity_history_state(&bytes).unwrap();
        assert_eq!(loaded.ram, ActivityHistorySeries::default());
        assert_eq!(loaded.cpu, series);
        assert_eq!(loaded.peers, series);
        assert_eq!(loaded.schema_version, ACTIVITY_HISTORY_SCHEMA_VERSION);
        assert_eq!(
            decode_activity_history_state(&encode_activity_history_state(&state)).unwrap(),
            state
        );
    }

    #[test]
    fn retained_push_matches_push_then_trim_at_capacity_boundaries() {
        for cap in [
            0,
            1,
            63,
            64,
            65,
            SECOND_1S_CAP,
            MINUTE_1M_CAP,
            MINUTE_15M_CAP,
            HOUR_1H_CAP,
        ] {
            for initial_len in [0, cap.saturating_sub(1), cap, cap + 3] {
                let mut actual: Vec<_> = (0..initial_len).collect();
                let mut expected = actual.clone();
                for value in initial_len..initial_len + 130 {
                    expected.push(value);
                    cap_vec(&mut expected, cap);
                    push_retained(&mut actual, value, cap);
                    assert_eq!(actual, expected, "cap={cap}, initial_len={initial_len}");
                }
            }
        }
    }

    #[test]
    fn retained_push_reserves_incrementally_and_reuses_full_tiers() {
        for cap in [
            1,
            63,
            64,
            65,
            SECOND_1S_CAP,
            MINUTE_1M_CAP,
            MINUTE_15M_CAP,
            HOUR_1H_CAP,
        ] {
            let mut points = Vec::new();
            for value in 0..cap {
                push_retained(&mut points, value, cap);
                if value == 0 {
                    assert!(points.capacity() <= 4);
                }
                assert!(points.capacity() >= points.len());
                assert!(points.capacity() <= cap);
                assert!(points.capacity() - points.len() < HISTORY_RESERVE_CHUNK);
            }
            let capacity = points.capacity();
            let allocation = points.as_ptr();
            for value in cap..cap + 130 {
                push_retained(&mut points, value, cap);
                assert_eq!(points.capacity(), capacity);
                assert_eq!(points.as_ptr(), allocation);
            }
        }
    }

    #[test]
    fn ingest_matches_legacy_rollups_retention_and_persistence_decisions() {
        // Full tiers and nearly complete accumulators exercise every rollover on
        // the first sample; the rest covers zero runs, rounding and restored state.
        let point = ActivityHistoryPoint {
            ts_unix: 1,
            primary: 7,
            secondary: 13,
        };
        let mut actual = ActivityHistorySeries {
            tiers: ActivityHistoryTiers {
                second_1s: vec![point.clone(); SECOND_1S_CAP],
                minute_1m: vec![point.clone(); MINUTE_1M_CAP],
                minute_15m: vec![point.clone(); MINUTE_15M_CAP],
                hour_1h: vec![point; HOUR_1H_CAP],
            },
            rollups: ActivityHistoryRollupSnapshot {
                second_to_minute: PersistedRollupAccumulator {
                    count: 59,
                    primary_sum: 413,
                    secondary_sum: 767,
                },
                minute_to_15m: PersistedRollupAccumulator {
                    count: 14,
                    primary_sum: 98,
                    secondary_sum: 182,
                },
                m15_to_hour: PersistedRollupAccumulator {
                    count: 3,
                    primary_sum: 21,
                    secondary_sum: 39,
                },
            },
        };
        let mut expected = actual.clone();
        let mut actual_rollups = ActivityHistorySeriesRollupState::from_snapshot(&actual.rollups);
        let mut expected_rollups = actual_rollups.clone();
        for i in 0..7_201_u64 {
            let (primary, secondary) = if i < 3_601 {
                (0, 0)
            } else {
                (i % 101, u64::MAX - i)
            };
            let changed =
                actual_rollups.ingest_second_sample(&mut actual, i + 2, primary, secondary);
            let expected_changed = legacy_ingest_second_sample(
                &mut expected_rollups,
                &mut expected,
                i + 2,
                primary,
                secondary,
            );
            assert_eq!(changed, expected_changed, "sample {i}");
            assert_eq!(actual, expected, "sample {i}");
            assert_eq!(actual_rollups, expected_rollups);
            assert_eq!(actual.tiers.second_1s.capacity(), SECOND_1S_CAP);
            assert_eq!(actual.tiers.minute_1m.capacity(), MINUTE_1M_CAP);
            assert_eq!(actual.tiers.minute_15m.capacity(), MINUTE_15M_CAP);
            assert_eq!(actual.tiers.hour_1h.capacity(), HOUR_1H_CAP);
            if i == 3_631 {
                actual_rollups = ActivityHistorySeriesRollupState::from_snapshot(&actual.rollups);
            }
        }
    }

    #[test]
    fn rollup_ingest_creates_minute_point_after_sixty_seconds() {
        let mut series = ActivityHistorySeries::default();
        let mut rollups = ActivityHistorySeriesRollupState::default();
        for i in 0..60 {
            let changed = rollups.ingest_second_sample(&mut series, i, 10, 20);
            assert!(changed);
        }

        assert_eq!(series.tiers.second_1s.len(), 60);
        assert_eq!(series.tiers.minute_1m.len(), 1);
        assert_eq!(series.tiers.minute_1m[0].primary, 10);
        assert_eq!(series.tiers.minute_1m[0].secondary, 20);
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join(ACTIVITY_HISTORY_FILE_NAME);

        let mut state = ActivityHistoryPersistedState {
            updated_at_unix: 1_777_777_777,
            ..Default::default()
        };
        state.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 1,
            primary: 250,
            secondary: 0,
        });
        let mut peer_rollups = ActivityHistorySeriesRollupState::default();
        for second in 1..=61 {
            peer_rollups.ingest_second_sample(
                &mut state.peers,
                second,
                if second <= 30 { PEER_COUNT_SCALE } else { 0 },
                0,
            );
        }
        state.torrents.insert(
            "abcd".to_string(),
            ActivityHistorySeries {
                tiers: ActivityHistoryTiers {
                    second_1s: vec![ActivityHistoryPoint {
                        ts_unix: 1,
                        primary: 100,
                        secondary: 200,
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        save_activity_history_state_to_path(&state, &path).expect("save state");
        // Early checkpoint sparsification must produce exactly the same file,
        // including partial rollup accumulators, as the former dense input.
        let original_bytes = std::fs::read(&path).unwrap();
        let early_sparse = sparse_state_for_persistence(&state);
        save_activity_history_state_to_path(&early_sparse, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        let loaded = load_activity_history_state_from_path(&path);

        assert_eq!(loaded.updated_at_unix, state.updated_at_unix);
        assert_eq!(loaded.cpu.tiers.second_1s, state.cpu.tiers.second_1s);
        assert_eq!(loaded.torrents.get("abcd"), state.torrents.get("abcd"));
        assert_eq!(loaded.peers, sparse_series_for_persistence(&state.peers));
        assert_eq!(
            loaded.peers.tiers.minute_1m[0].primary,
            PEER_COUNT_SCALE / 2
        );
    }

    #[test]
    fn idle_history_coverage_survives_save_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut state = ActivityHistoryPersistedState::default();
        state.peers.tiers.minute_1m = (1..=120)
            .map(|minute| ActivityHistoryPoint {
                ts_unix: minute * 60,
                ..Default::default()
            })
            .collect();
        state
            .torrents
            .insert("0123".to_string(), state.peers.clone());
        save_activity_history_state_to_path(&state, &path).unwrap();
        let loaded = load_activity_history_state_from_path(&path);
        assert_eq!(loaded.peers.tiers.minute_1m.len(), 2);
        assert_eq!(loaded.torrents["0123"].tiers.minute_1m.len(), 2);
        let dense = crate::telemetry::restore_densify::densify_points_for_restore(
            &loaded.peers.tiers.minute_1m,
            60,
            MINUTE_1M_CAP,
            7200,
            |p| p.ts_unix,
            |ts_unix| ActivityHistoryPoint {
                ts_unix,
                ..Default::default()
            },
        );
        assert_eq!(dense, state.peers.tiers.minute_1m);
    }

    #[test]
    fn version_one_history_loads_without_losing_existing_series() {
        let mut series = ActivityHistorySeries::default();
        series.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 100,
            primary: 250,
            secondary: 20,
        });
        // The original layout ended after the torrent map, with no peer series.
        let mut bytes = ACTIVITY_HISTORY_MAGIC.to_vec();
        encode_u32(&mut bytes, 1);
        encode_u64(&mut bytes, 100);
        for _ in 0..4 {
            encode_series(&mut bytes, &series);
        }
        encode_u32(&mut bytes, 1);
        encode_string(&mut bytes, "abcd");
        encode_series(&mut bytes, &series);

        let loaded = decode_activity_history_state(&bytes).unwrap();
        assert_eq!(loaded.schema_version, ACTIVITY_HISTORY_SCHEMA_VERSION);
        assert_eq!(loaded.cpu, series);
        assert_eq!(loaded.ram, ActivityHistorySeries::default());
        assert_eq!(loaded.disk, series);
        assert_eq!(loaded.tuning, series);
        assert_eq!(loaded.torrents["abcd"], series);
        assert_eq!(loaded.peers, ActivityHistorySeries::default());
        assert_eq!(
            decode_activity_history_state(&encode_activity_history_state(&loaded)).unwrap(),
            loaded
        );
    }

    #[test]
    fn retain_only_torrent_series_prunes_absent_keys() {
        let mut state = ActivityHistoryPersistedState::default();
        state
            .torrents
            .insert("keep".to_string(), ActivityHistorySeries::default());
        state
            .torrents
            .insert("drop".to_string(), ActivityHistorySeries::default());

        let mut rollups = ActivityHistoryRollupState::default();
        rollups.torrents.insert(
            "keep".to_string(),
            ActivityHistorySeriesRollupState::default(),
        );
        rollups.torrents.insert(
            "drop".to_string(),
            ActivityHistorySeriesRollupState::default(),
        );

        let keep = HashSet::from(["keep".to_string()]);
        retain_only_torrent_series_for_keys(&mut state, &mut rollups, &keep);

        assert!(state.torrents.contains_key("keep"));
        assert!(!state.torrents.contains_key("drop"));
        assert!(rollups.torrents.contains_key("keep"));
        assert!(!rollups.torrents.contains_key("drop"));
    }
}
