// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::TorrentMetrics;
use std::sync::atomic::{AtomicU64, Ordering};

// Unique across manager lifetimes; reconnecting/restarting cannot reuse an old
// UI cache token. Exhaustion falls back to value comparison instead of wrapping.
static NEXT_AVAILABILITY_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default)]
pub struct ManagerTelemetry {
    last_sent_metrics: Option<TorrentMetrics>,
}

impl ManagerTelemetry {
    /// Stamp the complete snapshot at publication, covering every state mutation
    /// path without putting UI invalidation logic in TorrentState.
    pub fn prepare_snapshot(&mut self, metrics: &mut TorrentMetrics) -> bool {
        let unchanged = self.last_sent_metrics.as_ref().is_some_and(|previous| {
            previous.info_hash == metrics.info_hash
                && previous.number_of_pieces_total == metrics.number_of_pieces_total
                && previous.peers.len() == metrics.peers.len()
                && previous.peers.iter().zip(&metrics.peers).all(|(old, new)| {
                    old.address == new.address
                        && old.peer_id == new.peer_id
                        && old.bitfield == new.bitfield
                })
        });
        metrics.availability_revision = if unchanged {
            self.last_sent_metrics
                .as_ref()
                .and_then(|previous| previous.availability_revision)
        } else {
            NEXT_AVAILABILITY_REVISION
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                    next.checked_add(1)
                })
                .ok()
        };
        self.should_emit(metrics)
    }

    pub fn should_emit(&mut self, metrics: &TorrentMetrics) -> bool {
        let force_emit =
            metrics.bytes_downloaded_this_tick > 0 || metrics.bytes_uploaded_this_tick > 0;

        if !force_emit {
            if let Some(previous) = self.last_sent_metrics.as_mut() {
                // Compare every derived Eq field without copying peer snapshots.
                // Only these transient fields are ignored, as before. Restore the
                // private cached snapshot so suppression cannot mutate history.
                let saved = (
                    previous.next_announce_in,
                    previous.eta,
                    previous.availability_revision,
                );
                previous.next_announce_in = metrics.next_announce_in;
                previous.eta = metrics.eta;
                previous.availability_revision = metrics.availability_revision;
                let equal = *previous == *metrics;
                (
                    previous.next_announce_in,
                    previous.eta,
                    previous.availability_revision,
                ) = saved;
                if equal {
                    return false;
                }
            }
        }

        self.last_sent_metrics = Some(metrics.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::ManagerTelemetry;
    use crate::app::TorrentMetrics;
    use std::time::Duration;

    fn sample_metrics() -> TorrentMetrics {
        TorrentMetrics {
            info_hash: vec![1; 20],
            torrent_name: "example".to_string(),
            number_of_pieces_total: 100,
            number_of_pieces_completed: 20,
            download_speed_bps: 1024,
            upload_speed_bps: 0,
            bytes_downloaded_this_tick: 0,
            bytes_uploaded_this_tick: 0,
            eta: Duration::from_secs(120),
            activity_message: "Downloading".to_string(),
            next_announce_in: Duration::from_secs(10),
            total_size: 1_000_000,
            bytes_written: 200_000,
            ..Default::default()
        }
    }

    #[test]
    fn ignored_fields_leave_cached_snapshot_unchanged() {
        let mut telemetry = ManagerTelemetry::default();
        let original = sample_metrics();
        assert!(telemetry.should_emit(&original));
        let mut next = original.clone();
        next.next_announce_in += Duration::from_secs(5);
        next.eta += Duration::from_secs(10);
        next.availability_revision = Some(123);
        assert!(!telemetry.should_emit(&next));
        assert_eq!(telemetry.last_sent_metrics.as_ref(), Some(&original));
        next.total_size += 1;
        assert!(telemetry.should_emit(&next));
        assert_eq!(telemetry.last_sent_metrics.as_ref(), Some(&next));
    }

    proptest::proptest! {
        #[test]
        fn publication_matches_clone_based_reference(
            changes in proptest::collection::vec((0u8..8, proptest::prelude::any::<u16>()), 1..128)
        ) {
            let mut telemetry = ManagerTelemetry::default();
            let mut expected: Option<TorrentMetrics> = None;
            let mut current = sample_metrics();
            current.peers.push(crate::app::PeerInfo {
                bitfield: vec![false; 17],
                ..Default::default()
            });
            for (kind, value) in changes {
                current.bytes_downloaded_this_tick = 0;
                current.bytes_uploaded_this_tick = 0;
                match kind {
                    0 => current.next_announce_in = Duration::from_secs(value as u64),
                    1 => current.eta = Duration::from_secs(value as u64),
                    2 => current.availability_revision = Some(value as u64),
                    3 => current.peers[0].bitfield[value as usize % 17] = value % 2 == 0,
                    4 => current.total_size = value as u64,
                    5 => current.bytes_downloaded_this_tick = value as u64,
                    6 => current.bytes_uploaded_this_tick = value as u64,
                    _ => current.activity_message = value.to_string(),
                }
                let normalize = |metrics: &TorrentMetrics| {
                    let mut copy = metrics.clone();
                    copy.next_announce_in = Duration::ZERO;
                    copy.eta = Duration::ZERO;
                    copy.availability_revision = None;
                    copy
                };
                let emit = current.bytes_downloaded_this_tick > 0
                    || current.bytes_uploaded_this_tick > 0
                    || expected.as_ref().is_none_or(|old| normalize(old) != normalize(&current));
                proptest::prop_assert_eq!(telemetry.should_emit(&current), emit);
                if emit { expected = Some(current.clone()); }
                proptest::prop_assert_eq!(&telemetry.last_sent_metrics, &expected);
            }
        }
    }

    #[test]
    fn publication_keeps_owned_history_and_ignores_non_availability_changes() {
        let mut telemetry = ManagerTelemetry::default();
        let mut metrics = sample_metrics();
        metrics.peers.push(crate::app::PeerInfo {
            address: "192.0.2.10:6881".into(),
            bitfield: vec![false, true],
            ..Default::default()
        });
        assert!(telemetry.prepare_snapshot(&mut metrics));
        let revision = metrics.availability_revision;
        assert!(revision.is_some());
        assert!(!telemetry.prepare_snapshot(&mut metrics));
        metrics.eta = Duration::ZERO;
        assert!(!telemetry.prepare_snapshot(&mut metrics));
        metrics.bytes_downloaded_this_tick = 123;
        metrics.peers[0].download_speed_bps = 456;
        assert!(telemetry.prepare_snapshot(&mut metrics));
        assert_eq!(metrics.availability_revision, revision);
        metrics.peers[0].bitfield[0] = true;
        assert_eq!(
            telemetry.last_sent_metrics.as_ref().unwrap().peers[0].bitfield,
            vec![false, true]
        );
        assert!(telemetry.prepare_snapshot(&mut metrics));
        assert_ne!(metrics.availability_revision, revision);
    }

    #[test]
    fn emits_first_snapshot() {
        let mut telemetry = ManagerTelemetry::default();
        let metrics = sample_metrics();
        assert!(telemetry.should_emit(&metrics));
    }

    #[test]
    fn suppresses_identical_snapshot() {
        let mut telemetry = ManagerTelemetry::default();
        let metrics = sample_metrics();

        assert!(telemetry.should_emit(&metrics));
        assert!(!telemetry.should_emit(&metrics));
    }

    #[test]
    fn ignores_countdown_only_drift() {
        let mut telemetry = ManagerTelemetry::default();
        let first = sample_metrics();
        let mut second = first.clone();
        second.next_announce_in = Duration::from_secs(5);
        second.eta = Duration::from_secs(110);

        assert!(telemetry.should_emit(&first));
        assert!(!telemetry.should_emit(&second));
    }

    #[test]
    fn forces_emit_when_bytes_nonzero() {
        let mut telemetry = ManagerTelemetry::default();
        let first = sample_metrics();
        let mut second = first.clone();
        second.bytes_downloaded_this_tick = 4096;

        assert!(telemetry.should_emit(&first));
        assert!(telemetry.should_emit(&second));
    }

    #[test]
    fn emits_on_meaningful_change() {
        let mut telemetry = ManagerTelemetry::default();
        let first = sample_metrics();
        let mut second = first.clone();
        second.number_of_pieces_completed += 1;

        assert!(telemetry.should_emit(&first));
        assert!(telemetry.should_emit(&second));
    }
}
