// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use serde::{Deserialize, Serialize};
use std::io::{self, Cursor, Read};

pub const NETWORK_HISTORY_SCHEMA_VERSION: u32 = 2;
pub const SECOND_1S_CAP: usize = 60 * 60; // 1 hour
pub const MINUTE_1M_CAP: usize = 48 * 60; // 48 hours
pub const MINUTE_15M_CAP: usize = 30 * 24 * 4; // 30 days
pub const HOUR_1H_CAP: usize = 365 * 24; // 365 days
pub(super) const NETWORK_HISTORY_FILE_NAME: &str = "network_history.bin";
const NETWORK_HISTORY_MAGIC: &[u8; 8] = b"SSNHBIN1";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct NetworkHistoryPoint {
    pub ts_unix: u64,
    pub download_bps: u64,
    pub upload_bps: u64,
    pub backoff_ms_max: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct NetworkHistoryTiers {
    pub second_1s: Vec<NetworkHistoryPoint>,
    pub minute_1m: Vec<NetworkHistoryPoint>,
    pub minute_15m: Vec<NetworkHistoryPoint>,
    pub hour_1h: Vec<NetworkHistoryPoint>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct PersistedRollupAccumulator {
    pub count: u32,
    pub dl_sum: u128,
    pub ul_sum: u128,
    pub backoff_max: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default)]
pub struct NetworkHistoryRollupSnapshot {
    pub second_to_minute: PersistedRollupAccumulator,
    pub minute_to_15m: PersistedRollupAccumulator,
    pub m15_to_hour: PersistedRollupAccumulator,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct NetworkHistoryPersistedState {
    pub schema_version: u32,
    pub updated_at_unix: u64,
    pub rollups: NetworkHistoryRollupSnapshot,
    pub tiers: NetworkHistoryTiers,
}

impl Default for NetworkHistoryPersistedState {
    fn default() -> Self {
        Self {
            schema_version: NETWORK_HISTORY_SCHEMA_VERSION,
            updated_at_unix: 0,
            rollups: NetworkHistoryRollupSnapshot::default(),
            tiers: NetworkHistoryTiers::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RollupAccumulator {
    count: u32,
    dl_sum: u128,
    ul_sum: u128,
    backoff_max: u64,
}

impl RollupAccumulator {
    fn push(&mut self, point: &NetworkHistoryPoint) {
        self.count += 1;
        self.dl_sum += point.download_bps as u128;
        self.ul_sum += point.upload_bps as u128;
        self.backoff_max = self.backoff_max.max(point.backoff_ms_max);
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

impl From<&RollupAccumulator> for PersistedRollupAccumulator {
    fn from(accumulator: &RollupAccumulator) -> Self {
        Self {
            count: accumulator.count,
            dl_sum: accumulator.dl_sum,
            ul_sum: accumulator.ul_sum,
            backoff_max: accumulator.backoff_max,
        }
    }
}

impl From<&PersistedRollupAccumulator> for RollupAccumulator {
    fn from(accumulator: &PersistedRollupAccumulator) -> Self {
        Self {
            count: accumulator.count,
            dl_sum: accumulator.dl_sum,
            ul_sum: accumulator.ul_sum,
            backoff_max: accumulator.backoff_max,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkHistoryRollupState {
    second_to_minute: RollupAccumulator,
    minute_to_15m: RollupAccumulator,
    m15_to_hour: RollupAccumulator,
}

impl NetworkHistoryRollupState {
    pub fn to_snapshot(&self) -> NetworkHistoryRollupSnapshot {
        NetworkHistoryRollupSnapshot {
            second_to_minute: PersistedRollupAccumulator::from(&self.second_to_minute),
            minute_to_15m: PersistedRollupAccumulator::from(&self.minute_to_15m),
            m15_to_hour: PersistedRollupAccumulator::from(&self.m15_to_hour),
        }
    }

    pub fn from_snapshot(snapshot: &NetworkHistoryRollupSnapshot) -> Self {
        Self {
            second_to_minute: RollupAccumulator::from(&snapshot.second_to_minute),
            minute_to_15m: RollupAccumulator::from(&snapshot.minute_to_15m),
            m15_to_hour: RollupAccumulator::from(&snapshot.m15_to_hour),
        }
    }

    pub fn ingest_second_sample(
        &mut self,
        state: &mut NetworkHistoryPersistedState,
        ts_unix: u64,
        download_bps: u64,
        upload_bps: u64,
        backoff_ms_max: u64,
    ) -> bool {
        let second_point = NetworkHistoryPoint {
            ts_unix,
            download_bps,
            upload_bps,
            backoff_ms_max,
        };
        let mut should_persist = !is_zero_point(&second_point);
        state.tiers.second_1s.push(second_point.clone());
        cap_vec(&mut state.tiers.second_1s, SECOND_1S_CAP);

        self.second_to_minute.push(&second_point);
        if self.second_to_minute.count >= 60 {
            let minute_point = make_rollup_point(&self.second_to_minute, ts_unix);
            self.second_to_minute.clear();
            should_persist |= !is_zero_point(&minute_point);

            state.tiers.minute_1m.push(minute_point.clone());
            cap_vec(&mut state.tiers.minute_1m, MINUTE_1M_CAP);

            self.minute_to_15m.push(&minute_point);
            if self.minute_to_15m.count >= 15 {
                let m15_point = make_rollup_point(&self.minute_to_15m, ts_unix);
                self.minute_to_15m.clear();
                should_persist |= !is_zero_point(&m15_point);

                state.tiers.minute_15m.push(m15_point.clone());
                cap_vec(&mut state.tiers.minute_15m, MINUTE_15M_CAP);

                self.m15_to_hour.push(&m15_point);
                if self.m15_to_hour.count >= 4 {
                    let hour_point = make_rollup_point(&self.m15_to_hour, ts_unix);
                    self.m15_to_hour.clear();
                    should_persist |= !is_zero_point(&hour_point);

                    state.tiers.hour_1h.push(hour_point);
                    cap_vec(&mut state.tiers.hour_1h, HOUR_1H_CAP);
                }
            }
        }

        state.rollups = self.to_snapshot();
        should_persist
    }
}

fn make_rollup_point(acc: &RollupAccumulator, ts_unix: u64) -> NetworkHistoryPoint {
    if acc.count == 0 {
        return NetworkHistoryPoint {
            ts_unix,
            ..Default::default()
        };
    }
    NetworkHistoryPoint {
        ts_unix,
        download_bps: (acc.dl_sum / acc.count as u128) as u64,
        upload_bps: (acc.ul_sum / acc.count as u128) as u64,
        backoff_ms_max: acc.backoff_max,
    }
}

fn cap_vec<T>(vec: &mut Vec<T>, cap: usize) {
    if vec.len() > cap {
        let overflow = vec.len() - cap;
        vec.drain(0..overflow);
    }
}

pub fn enforce_retention_caps(state: &mut NetworkHistoryPersistedState) {
    cap_vec(&mut state.tiers.second_1s, SECOND_1S_CAP);
    cap_vec(&mut state.tiers.minute_1m, MINUTE_1M_CAP);
    cap_vec(&mut state.tiers.minute_15m, MINUTE_15M_CAP);
    cap_vec(&mut state.tiers.hour_1h, HOUR_1H_CAP);
}

pub fn is_zero_point(point: &NetworkHistoryPoint) -> bool {
    point.download_bps == 0 && point.upload_bps == 0 && point.backoff_ms_max == 0
}

fn sparse_points_for_persistence(points: &[NetworkHistoryPoint]) -> Vec<NetworkHistoryPoint> {
    points
        .iter()
        .filter(|point| !is_zero_point(point))
        .cloned()
        .collect()
}

pub fn sparse_state_for_persistence(
    state: &NetworkHistoryPersistedState,
) -> NetworkHistoryPersistedState {
    NetworkHistoryPersistedState {
        schema_version: state.schema_version,
        updated_at_unix: state.updated_at_unix,
        rollups: state.rollups.clone(),
        tiers: NetworkHistoryTiers {
            second_1s: sparse_points_for_persistence(&state.tiers.second_1s),
            minute_1m: sparse_points_for_persistence(&state.tiers.minute_1m),
            minute_15m: sparse_points_for_persistence(&state.tiers.minute_15m),
            hour_1h: sparse_points_for_persistence(&state.tiers.hour_1h),
        },
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused_imports)]
pub use native::{
    load_network_history_state, network_history_state_file_path, save_network_history_state,
};
#[cfg(test)]
use native::{load_network_history_state_from_path, save_network_history_state_to_path};

#[cfg(test)]
use std::fs;

fn encode_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn encode_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn encode_u128(buf: &mut Vec<u8>, value: u128) {
    buf.extend_from_slice(&value.to_le_bytes());
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
    encode_u128(buf, accumulator.dl_sum);
    encode_u128(buf, accumulator.ul_sum);
    encode_u64(buf, accumulator.backoff_max);
}

fn decode_rollup_accumulator(cursor: &mut Cursor<&[u8]>) -> io::Result<PersistedRollupAccumulator> {
    Ok(PersistedRollupAccumulator {
        count: decode_u32(cursor)?,
        dl_sum: decode_u128(cursor)?,
        ul_sum: decode_u128(cursor)?,
        backoff_max: decode_u64(cursor)?,
    })
}

fn encode_points(buf: &mut Vec<u8>, points: &[NetworkHistoryPoint]) {
    encode_u32(buf, points.len() as u32);
    for point in points {
        encode_u64(buf, point.ts_unix);
        encode_u64(buf, point.download_bps);
        encode_u64(buf, point.upload_bps);
        encode_u64(buf, point.backoff_ms_max);
    }
}

fn decode_points(
    cursor: &mut Cursor<&[u8]>,
    max_points: usize,
) -> io::Result<Vec<NetworkHistoryPoint>> {
    let count = decode_u32(cursor)? as usize;
    if count > max_points {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "network history tier exceeds retention cap",
        ));
    }
    let mut points = Vec::with_capacity(count);
    for _ in 0..count {
        points.push(NetworkHistoryPoint {
            ts_unix: decode_u64(cursor)?,
            download_bps: decode_u64(cursor)?,
            upload_bps: decode_u64(cursor)?,
            backoff_ms_max: decode_u64(cursor)?,
        });
    }
    Ok(points)
}

pub(super) fn encode_network_history_state(state: &NetworkHistoryPersistedState) -> Vec<u8> {
    let second_points = state.tiers.second_1s.len();
    let minute_points = state.tiers.minute_1m.len();
    let minute_15_points = state.tiers.minute_15m.len();
    let hour_points = state.tiers.hour_1h.len();
    let total_points = second_points + minute_points + minute_15_points + hour_points;
    let mut buf = Vec::with_capacity(
        NETWORK_HISTORY_MAGIC.len()
            + 12
            + (3 * (4 + 16 + 16 + 8))
            + (total_points * std::mem::size_of::<NetworkHistoryPoint>()),
    );
    buf.extend_from_slice(NETWORK_HISTORY_MAGIC);
    encode_u32(&mut buf, state.schema_version);
    encode_u64(&mut buf, state.updated_at_unix);
    encode_rollup_accumulator(&mut buf, &state.rollups.second_to_minute);
    encode_rollup_accumulator(&mut buf, &state.rollups.minute_to_15m);
    encode_rollup_accumulator(&mut buf, &state.rollups.m15_to_hour);
    encode_points(&mut buf, &state.tiers.second_1s);
    encode_points(&mut buf, &state.tiers.minute_1m);
    encode_points(&mut buf, &state.tiers.minute_15m);
    encode_points(&mut buf, &state.tiers.hour_1h);
    buf
}

pub(super) fn decode_network_history_state(
    bytes: &[u8],
) -> io::Result<NetworkHistoryPersistedState> {
    let mut cursor = Cursor::new(bytes);
    let mut magic = [0_u8; NETWORK_HISTORY_MAGIC.len()];
    cursor.read_exact(&mut magic)?;
    if &magic != NETWORK_HISTORY_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid network history binary header",
        ));
    }

    let schema_version = decode_u32(&mut cursor)?;
    if schema_version != NETWORK_HISTORY_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported network history schema version {schema_version}"),
        ));
    }
    let updated_at_unix = decode_u64(&mut cursor)?;
    let rollups = NetworkHistoryRollupSnapshot {
        second_to_minute: decode_rollup_accumulator(&mut cursor)?,
        minute_to_15m: decode_rollup_accumulator(&mut cursor)?,
        m15_to_hour: decode_rollup_accumulator(&mut cursor)?,
    };
    let tiers = NetworkHistoryTiers {
        second_1s: decode_points(&mut cursor, SECOND_1S_CAP)?,
        minute_1m: decode_points(&mut cursor, MINUTE_1M_CAP)?,
        minute_15m: decode_points(&mut cursor, MINUTE_15M_CAP)?,
        hour_1h: decode_points(&mut cursor, HOUR_1H_CAP)?,
    };

    if cursor.position() != bytes.len() as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes in network history binary payload",
        ));
    }

    Ok(NetworkHistoryPersistedState {
        schema_version,
        updated_at_unix,
        rollups,
        tiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join(NETWORK_HISTORY_FILE_NAME);

        let state = load_network_history_state_from_path(&path);
        assert_eq!(state, NetworkHistoryPersistedState::default());
    }

    #[test]
    fn load_invalid_file_returns_default() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join(NETWORK_HISTORY_FILE_NAME);
        fs::write(&path, [0_u8, 1, 2, 3]).expect("write malformed binary");

        let state = load_network_history_state_from_path(&path);
        assert_eq!(state, NetworkHistoryPersistedState::default());
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join(NETWORK_HISTORY_FILE_NAME);

        let state = NetworkHistoryPersistedState {
            schema_version: NETWORK_HISTORY_SCHEMA_VERSION,
            updated_at_unix: 1_771_860_000,
            rollups: NetworkHistoryRollupSnapshot {
                second_to_minute: PersistedRollupAccumulator {
                    count: 17,
                    dl_sum: 12_345,
                    ul_sum: 678,
                    backoff_max: 9,
                },
                minute_to_15m: PersistedRollupAccumulator {
                    count: 3,
                    dl_sum: 3_333,
                    ul_sum: 444,
                    backoff_max: 7,
                },
                m15_to_hour: PersistedRollupAccumulator {
                    count: 2,
                    dl_sum: 8_888,
                    ul_sum: 999,
                    backoff_max: 5,
                },
            },
            tiers: NetworkHistoryTiers {
                second_1s: vec![NetworkHistoryPoint {
                    ts_unix: 1_771_860_000,
                    download_bps: 1024,
                    upload_bps: 256,
                    backoff_ms_max: 0,
                }],
                minute_1m: vec![],
                minute_15m: vec![],
                hour_1h: vec![],
            },
        };

        save_network_history_state_to_path(&state, &path).expect("save network history state");
        let loaded = load_network_history_state_from_path(&path);

        assert_eq!(loaded, state);
    }

    #[test]
    fn sparse_state_for_persistence_omits_zero_points() {
        let state = NetworkHistoryPersistedState {
            schema_version: NETWORK_HISTORY_SCHEMA_VERSION,
            updated_at_unix: 1_771_860_000,
            rollups: NetworkHistoryRollupSnapshot {
                second_to_minute: PersistedRollupAccumulator {
                    count: 2,
                    dl_sum: 1_024,
                    ul_sum: 0,
                    backoff_max: 0,
                },
                ..Default::default()
            },
            tiers: NetworkHistoryTiers {
                second_1s: vec![
                    NetworkHistoryPoint {
                        ts_unix: 1,
                        download_bps: 0,
                        upload_bps: 0,
                        backoff_ms_max: 0,
                    },
                    NetworkHistoryPoint {
                        ts_unix: 2,
                        download_bps: 1024,
                        upload_bps: 0,
                        backoff_ms_max: 0,
                    },
                ],
                minute_1m: vec![NetworkHistoryPoint {
                    ts_unix: 60,
                    download_bps: 0,
                    upload_bps: 0,
                    backoff_ms_max: 0,
                }],
                minute_15m: vec![],
                hour_1h: vec![],
            },
        };

        let sparse = sparse_state_for_persistence(&state);
        assert_eq!(sparse.tiers.second_1s.len(), 1);
        assert_eq!(sparse.tiers.second_1s[0].ts_unix, 2);
        assert!(sparse.tiers.minute_1m.is_empty());
        assert_eq!(sparse.rollups, state.rollups);
    }

    #[test]
    fn zero_only_second_sample_does_not_mark_persistence_dirty() {
        let mut state = NetworkHistoryPersistedState::default();
        let mut rollups = NetworkHistoryRollupState::default();

        assert!(!rollups.ingest_second_sample(&mut state, 1, 0, 0, 0));
        assert_eq!(state.tiers.second_1s.len(), 1);
        assert!(is_zero_point(&state.tiers.second_1s[0]));
        assert_eq!(state.rollups, rollups.to_snapshot());
    }

    #[test]
    fn legacy_toml_file_is_ignored() {
        let dir = tempdir().expect("create tempdir");
        let binary_path = dir.path().join(NETWORK_HISTORY_FILE_NAME);
        let legacy_toml_path = dir.path().join("network_history.toml");
        let legacy_state = NetworkHistoryPersistedState {
            schema_version: NETWORK_HISTORY_SCHEMA_VERSION,
            updated_at_unix: 1_771_860_000,
            rollups: NetworkHistoryRollupSnapshot::default(),
            tiers: NetworkHistoryTiers {
                second_1s: vec![NetworkHistoryPoint {
                    ts_unix: 1_771_860_000,
                    download_bps: 2048,
                    upload_bps: 512,
                    backoff_ms_max: 4,
                }],
                minute_1m: vec![],
                minute_15m: vec![],
                hour_1h: vec![],
            },
        };

        let legacy_toml = toml::to_string_pretty(&legacy_state).expect("serialize legacy toml");
        fs::write(&legacy_toml_path, legacy_toml).expect("write legacy toml");

        let loaded = load_network_history_state_from_path(&binary_path);
        assert_eq!(loaded, NetworkHistoryPersistedState::default());
    }

    #[test]
    fn retention_caps_trim_oldest_points() {
        let mut state = NetworkHistoryPersistedState::default();

        state.tiers.second_1s = (0..(SECOND_1S_CAP + 10))
            .map(|i| NetworkHistoryPoint {
                ts_unix: i as u64,
                ..Default::default()
            })
            .collect();
        state.tiers.minute_1m = (0..(MINUTE_1M_CAP + 10))
            .map(|i| NetworkHistoryPoint {
                ts_unix: i as u64,
                ..Default::default()
            })
            .collect();
        state.tiers.minute_15m = (0..(MINUTE_15M_CAP + 10))
            .map(|i| NetworkHistoryPoint {
                ts_unix: i as u64,
                ..Default::default()
            })
            .collect();
        state.tiers.hour_1h = (0..(HOUR_1H_CAP + 10))
            .map(|i| NetworkHistoryPoint {
                ts_unix: i as u64,
                ..Default::default()
            })
            .collect();

        enforce_retention_caps(&mut state);

        assert_eq!(state.tiers.second_1s.len(), SECOND_1S_CAP);
        assert_eq!(state.tiers.minute_1m.len(), MINUTE_1M_CAP);
        assert_eq!(state.tiers.minute_15m.len(), MINUTE_15M_CAP);
        assert_eq!(state.tiers.hour_1h.len(), HOUR_1H_CAP);
        assert_eq!(state.tiers.second_1s.first().map(|p| p.ts_unix), Some(10));
    }

    #[test]
    fn rollup_pipeline_emits_expected_aggregates() {
        let mut state = NetworkHistoryPersistedState::default();
        let mut rollups = NetworkHistoryRollupState::default();

        // 3600 seconds => 60 minute points => 4 x 15m points => 1 hour point
        for i in 1..=3600_u64 {
            let dl = i;
            let ul = i * 2;
            let backoff = i % 100;
            assert!(rollups.ingest_second_sample(&mut state, i, dl, ul, backoff));
        }

        assert_eq!(state.tiers.second_1s.len(), 3600);
        assert_eq!(state.tiers.minute_1m.len(), 60);
        assert_eq!(state.tiers.minute_15m.len(), 4);
        assert_eq!(state.tiers.hour_1h.len(), 1);

        let minute_1 = &state.tiers.minute_1m[0];
        // average of 1..=60
        assert_eq!(minute_1.download_bps, 30);
        assert_eq!(minute_1.upload_bps, 61);
        assert_eq!(minute_1.backoff_ms_max, 60);

        let hour = &state.tiers.hour_1h[0];
        // average of 1..=3600
        assert_eq!(hour.download_bps, 1800);
        assert_eq!(hour.upload_bps, 3601);
        assert_eq!(hour.backoff_ms_max, 99);
        assert_eq!(state.rollups, rollups.to_snapshot());
    }

    #[test]
    fn rollup_snapshot_round_trip_restores_partial_accumulators() {
        let snapshot = NetworkHistoryRollupSnapshot {
            second_to_minute: PersistedRollupAccumulator {
                count: 1,
                dl_sum: 61,
                ul_sum: 122,
                backoff_max: 61,
            },
            minute_to_15m: PersistedRollupAccumulator {
                count: 1,
                dl_sum: 16,
                ul_sum: 48,
                backoff_max: 16,
            },
            m15_to_hour: PersistedRollupAccumulator {
                count: 1,
                dl_sum: 5,
                ul_sum: 20,
                backoff_max: 5,
            },
        };

        let rollups = NetworkHistoryRollupState::from_snapshot(&snapshot);

        assert_eq!(rollups.to_snapshot(), snapshot);
    }

    #[test]
    fn load_schema_v1_file_returns_default() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join(NETWORK_HISTORY_FILE_NAME);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(NETWORK_HISTORY_MAGIC);
        encode_u32(&mut bytes, 1);
        encode_u64(&mut bytes, 1_771_860_000);
        encode_points(
            &mut bytes,
            &[NetworkHistoryPoint {
                ts_unix: 1,
                download_bps: 1,
                upload_bps: 2,
                backoff_ms_max: 3,
            }],
        );
        encode_points(&mut bytes, &[]);
        encode_points(&mut bytes, &[]);
        encode_points(&mut bytes, &[]);
        fs::write(&path, bytes).expect("write schema v1 binary");

        let loaded = load_network_history_state_from_path(&path);
        assert_eq!(loaded, NetworkHistoryPersistedState::default());
    }
}
