// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::torrent_manager::{FileProbeBatchResult, FileProbeEntry};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const INTEGRITY_SCHEDULER_TICK_INTERVAL: Duration = Duration::from_secs(1);

const PROBE_BATCH_MAX_FILES: usize = 256;
const MAX_IN_FLIGHT_PROBE_BATCHES: usize = 2;
const PROBE_BATCH_TIMEOUT: Duration = Duration::from_secs(30);
const PENDING_METADATA_RETRY_INTERVAL: Duration = Duration::from_secs(15);
const RECOVERY_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const SMALL_MANIFEST_FILE_COUNT_THRESHOLD: usize = 1_000;
const SMALL_MANIFEST_HEALTHY_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const ACTIVE_HEALTHY_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const IDLE_HEALTHY_RETRY_INTERVAL: Duration = Duration::from_secs(30 * 60);
const DISPATCH_FAILURE_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataAvailabilityState {
    Unknown,
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityPriorityClass {
    Recovery,
    ActiveHealthy,
    IdleHealthy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentIntegritySnapshot {
    pub info_hash: Vec<u8>,
    pub data_available: bool,
    pub is_downloading: bool,
    pub file_count: Option<usize>,
    pub saved_location: Option<PathBuf>,
    pub download_speed_bps: u64,
    pub upload_speed_bps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeBatchRequest {
    pub info_hash: Vec<u8>,
    pub epoch: u64,
    pub start_file_index: usize,
    pub max_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeBatchOutcome {
    PendingMetadata,
    SweepInProgress,
    CompletedSweep { problem_files: Vec<FileProbeEntry> },
}

#[derive(Debug)]
struct IntegrityTorrentState {
    probe_epoch: u64,
    next_probe_file_index: usize,
    current_sweep_problem_files: Vec<FileProbeEntry>,
    in_flight: bool,
    pending_metadata: bool,
    availability: DataAvailabilityState,
    has_completed_probe: bool,
    is_downloading: bool,
    is_active: bool,
    file_count: Option<usize>,
    saved_location: Option<PathBuf>,
    next_due_at: Instant,
    last_probe_started_at: Option<Instant>,
    last_probe_completed_at: Option<Instant>,
    last_full_probe_completed_at: Option<Instant>,
}

impl IntegrityTorrentState {
    fn new(now: Instant) -> Self {
        Self {
            probe_epoch: 0,
            next_probe_file_index: 0,
            current_sweep_problem_files: Vec::new(),
            in_flight: false,
            pending_metadata: false,
            availability: DataAvailabilityState::Unknown,
            has_completed_probe: false,
            is_downloading: false,
            is_active: false,
            file_count: None,
            saved_location: None,
            next_due_at: now,
            last_probe_started_at: None,
            last_probe_completed_at: None,
            last_full_probe_completed_at: None,
        }
    }

    fn priority_class(&self) -> IntegrityPriorityClass {
        if matches!(self.availability, DataAvailabilityState::Unavailable) {
            IntegrityPriorityClass::Recovery
        } else if self.is_active {
            IntegrityPriorityClass::ActiveHealthy
        } else {
            IntegrityPriorityClass::IdleHealthy
        }
    }

    fn schedule_next_full_probe(&mut self, now: Instant) {
        self.next_due_at = now
            + match self.priority_class() {
                IntegrityPriorityClass::Recovery => RECOVERY_RETRY_INTERVAL,
                IntegrityPriorityClass::ActiveHealthy | IntegrityPriorityClass::IdleHealthy => {
                    self.healthy_retry_interval()
                }
            };
    }

    fn healthy_retry_interval(&self) -> Duration {
        if self
            .file_count
            .is_some_and(|count| count < SMALL_MANIFEST_FILE_COUNT_THRESHOLD)
        {
            SMALL_MANIFEST_HEALTHY_RETRY_INTERVAL
        } else if self.is_active {
            ACTIVE_HEALTHY_RETRY_INTERVAL
        } else {
            IDLE_HEALTHY_RETRY_INTERVAL
        }
    }

    fn expected_healthy_interval(&self) -> Option<Duration> {
        if self.has_completed_probe
            && !self.in_flight
            && !self.pending_metadata
            && matches!(self.availability, DataAvailabilityState::Available)
        {
            Some(self.healthy_retry_interval())
        } else {
            None
        }
    }

    fn healthy_deadline_mismatch(&self, now: Instant) -> Option<(Duration, Duration)> {
        let expected = self.expected_healthy_interval()?;
        if self.next_due_at <= now {
            return None;
        }

        let remaining = self.next_due_at.saturating_duration_since(now);
        if remaining.abs_diff(expected) > Duration::from_secs(5) {
            Some((remaining, expected))
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct IntegrityScheduler {
    now: Instant,
    torrents: HashMap<Vec<u8>, IntegrityTorrentState>,
    in_flight_probe_batches: usize,
}

impl IntegrityScheduler {
    pub fn new(now: Instant) -> Self {
        Self {
            now,
            torrents: HashMap::new(),
            in_flight_probe_batches: 0,
        }
    }

    pub fn advance_time(&mut self, dt: Duration) {
        self.now += dt;
    }

    pub fn sync_torrents<I>(&mut self, snapshots: I)
    where
        I: IntoIterator<Item = TorrentIntegritySnapshot>,
    {
        let mut seen = HashSet::new();

        for snapshot in snapshots {
            let info_hash = snapshot.info_hash.clone();
            seen.insert(info_hash.clone());
            let state = self
                .torrents
                .entry(info_hash.clone())
                .or_insert_with(|| IntegrityTorrentState::new(self.now));

            state.is_active = snapshot.download_speed_bps > 0 || snapshot.upload_speed_bps > 0;
            state.is_downloading = snapshot.is_downloading;
            state.file_count = snapshot.file_count;
            state.saved_location = snapshot.saved_location;

            if !snapshot.data_available {
                state.availability = DataAvailabilityState::Unavailable;
            } else if state.has_completed_probe {
                state.availability = DataAvailabilityState::Available;
            }

            if let Some((_remaining, expected)) = state.healthy_deadline_mismatch(self.now) {
                state.next_due_at = self.now + expected;
            }
        }

        self.torrents
            .retain(|info_hash, _| seen.contains(info_hash));
        self.in_flight_probe_batches = self
            .torrents
            .values()
            .filter(|state| state.in_flight)
            .count();
    }

    pub fn remove_torrent(&mut self, info_hash: &[u8]) {
        self.torrents.remove(info_hash);
        self.in_flight_probe_batches = self
            .torrents
            .values()
            .filter(|state| state.in_flight)
            .count();
    }

    pub fn next_probe_in(&self, info_hash: &[u8]) -> Option<Duration> {
        let state = self.torrents.get(info_hash)?;
        if state.is_downloading && !matches!(state.availability, DataAvailabilityState::Unavailable)
        {
            return None;
        }
        Some(if state.in_flight || state.next_due_at <= self.now {
            Duration::ZERO
        } else {
            state.next_due_at.saturating_duration_since(self.now)
        })
    }

    pub fn on_metadata_loaded(&mut self, info_hash: &[u8]) {
        if let Some(state) = self.torrents.get_mut(info_hash) {
            state.pending_metadata = false;
            state.next_probe_file_index = 0;
            state.current_sweep_problem_files.clear();
            state.next_due_at = self.now;
        }
    }

    pub fn on_data_availability_fault(&mut self, info_hash: &[u8]) {
        let shared_saved_location = self
            .torrents
            .get(info_hash)
            .and_then(|state| state.saved_location.clone());

        for (torrent_info_hash, state) in &mut self.torrents {
            let same_torrent = torrent_info_hash.as_slice() == info_hash;
            let same_saved_location = shared_saved_location
                .as_ref()
                .is_some_and(|path| state.saved_location.as_ref() == Some(path));

            if same_torrent || same_saved_location {
                state.probe_epoch = state.probe_epoch.saturating_add(1);
                if state.in_flight {
                    state.in_flight = false;
                    self.in_flight_probe_batches = self.in_flight_probe_batches.saturating_sub(1);
                }
                state.next_probe_file_index = 0;
                state.current_sweep_problem_files.clear();
                state.next_due_at = self.now;
                state.pending_metadata = false;

                if same_torrent {
                    state.availability = DataAvailabilityState::Unavailable;
                }
            }
        }
    }

    pub fn drain_due_probe_requests(&mut self) -> Vec<ProbeBatchRequest> {
        let mut requests = Vec::new();

        self.reclaim_timed_out_probe_batches();

        while self.in_flight_probe_batches < MAX_IN_FLIGHT_PROBE_BATCHES {
            let Some(info_hash) = self.pick_next_due_torrent() else {
                break;
            };

            let state = self
                .torrents
                .get_mut(&info_hash)
                .expect("due torrent should exist in scheduler state");

            if state.next_probe_file_index == 0 {
                state.current_sweep_problem_files.clear();
            }
            state.in_flight = true;
            state.last_probe_started_at = Some(self.now);
            self.in_flight_probe_batches += 1;

            requests.push(ProbeBatchRequest {
                info_hash,
                epoch: state.probe_epoch,
                start_file_index: state.next_probe_file_index,
                max_files: PROBE_BATCH_MAX_FILES,
            });
        }

        requests
    }

    fn reclaim_timed_out_probe_batches(&mut self) {
        for state in self.torrents.values_mut() {
            let timed_out = state.in_flight
                && state.last_probe_started_at.is_some_and(|started_at| {
                    self.now.saturating_duration_since(started_at) >= PROBE_BATCH_TIMEOUT
                });

            if !timed_out {
                continue;
            }

            state.in_flight = false;
            state.probe_epoch = state.probe_epoch.saturating_add(1);
            state.next_probe_file_index = 0;
            state.current_sweep_problem_files.clear();
            state.pending_metadata = false;
            state.next_due_at = self.now;
            state.last_probe_started_at = None;
            self.in_flight_probe_batches = self.in_flight_probe_batches.saturating_sub(1);
        }
    }

    fn pick_next_due_torrent(&self) -> Option<Vec<u8>> {
        self.torrents
            .iter()
            .filter(|(_, state)| {
                !state.in_flight
                    && state.next_due_at <= self.now
                    && (!state.is_downloading
                        || matches!(state.availability, DataAvailabilityState::Unavailable))
            })
            .min_by(|(left_hash, left_state), (right_hash, right_state)| {
                priority_rank(left_state.priority_class())
                    .cmp(&priority_rank(right_state.priority_class()))
                    .then_with(|| left_state.next_due_at.cmp(&right_state.next_due_at))
                    .then_with(|| left_hash.cmp(right_hash))
            })
            .map(|(info_hash, _)| info_hash.clone())
    }

    pub fn on_dispatch_failed(&mut self, info_hash: &[u8]) {
        if let Some(state) = self.torrents.get_mut(info_hash) {
            if state.in_flight {
                state.in_flight = false;
                self.in_flight_probe_batches = self.in_flight_probe_batches.saturating_sub(1);
            }
            state.next_due_at = self.now + DISPATCH_FAILURE_RETRY_INTERVAL;
        }
    }

    pub fn on_probe_batch_result(
        &mut self,
        info_hash: &[u8],
        result: FileProbeBatchResult,
    ) -> Option<ProbeBatchOutcome> {
        let state = self.torrents.get_mut(info_hash)?;

        if result.epoch != state.probe_epoch {
            return None;
        }

        if state.in_flight {
            state.in_flight = false;
            self.in_flight_probe_batches = self.in_flight_probe_batches.saturating_sub(1);
        }
        state.last_probe_completed_at = Some(self.now);

        if result.pending_metadata {
            state.pending_metadata = true;
            state.next_probe_file_index = 0;
            state.current_sweep_problem_files.clear();
            state.next_due_at = self.now + PENDING_METADATA_RETRY_INTERVAL;
            return Some(ProbeBatchOutcome::PendingMetadata);
        }

        state.pending_metadata = false;
        state
            .current_sweep_problem_files
            .extend(result.problem_files);
        state.next_probe_file_index = result.next_file_index;

        if result.reached_end_of_manifest {
            state.has_completed_probe = true;
            state.last_full_probe_completed_at = Some(self.now);
            state.next_probe_file_index = 0;

            let problem_files = std::mem::take(&mut state.current_sweep_problem_files);
            state.availability = if problem_files.is_empty() {
                DataAvailabilityState::Available
            } else {
                DataAvailabilityState::Unavailable
            };
            state.schedule_next_full_probe(self.now);

            return Some(ProbeBatchOutcome::CompletedSweep { problem_files });
        }

        state.next_due_at = self.now;
        Some(ProbeBatchOutcome::SweepInProgress)
    }
}

fn priority_rank(priority: IntegrityPriorityClass) -> u8 {
    match priority {
        IntegrityPriorityClass::Recovery => 0,
        IntegrityPriorityClass::ActiveHealthy => 1,
        IntegrityPriorityClass::IdleHealthy => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::StorageError;

    fn snapshot(
        info_hash: &[u8],
        data_available: bool,
        file_count: Option<usize>,
        saved_location: Option<&str>,
        download_speed_bps: u64,
        upload_speed_bps: u64,
    ) -> TorrentIntegritySnapshot {
        TorrentIntegritySnapshot {
            info_hash: info_hash.to_vec(),
            data_available,
            is_downloading: false,
            file_count,
            saved_location: saved_location.map(PathBuf::from),
            download_speed_bps,
            upload_speed_bps,
        }
    }

    fn downloading_snapshot(
        info_hash: &[u8],
        data_available: bool,
        file_count: Option<usize>,
        saved_location: Option<&str>,
    ) -> TorrentIntegritySnapshot {
        TorrentIntegritySnapshot {
            info_hash: info_hash.to_vec(),
            data_available,
            is_downloading: true,
            file_count,
            saved_location: saved_location.map(PathBuf::from),
            download_speed_bps: 0,
            upload_speed_bps: 0,
        }
    }

    fn missing_entry(name: &str) -> FileProbeEntry {
        FileProbeEntry {
            relative_path: name.into(),
            absolute_path: format!("/tmp/{name}").into(),
            error: StorageError::from(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory",
            )),
            expected_size: 1,
            observed_size: None,
        }
    }

    #[test]
    fn scheduler_prioritizes_recovery_before_healthy() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([
            snapshot(b"healthy", true, None, Some("/downloads/a"), 0, 0),
            snapshot(b"recovery", false, None, Some("/downloads/b"), 0, 0),
        ]);

        let requests = scheduler.drain_due_probe_requests();

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].info_hash, b"recovery".to_vec());
        assert_eq!(requests[1].info_hash, b"healthy".to_vec());
    }

    #[test]
    fn partial_batch_keeps_sweep_in_progress() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([snapshot(b"sample", false, None, Some("/downloads"), 0, 0)]);

        let request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected initial request");
        assert_eq!(request.start_file_index, 0);

        let outcome = scheduler.on_probe_batch_result(
            b"sample",
            FileProbeBatchResult {
                epoch: request.epoch,
                scanned_files: request.max_files,
                next_file_index: request.max_files,
                reached_end_of_manifest: false,
                pending_metadata: false,
                problem_files: vec![missing_entry("missing.bin")],
            },
        );
        assert_eq!(outcome, Some(ProbeBatchOutcome::SweepInProgress));

        let next_request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected continuation request");
        assert_eq!(next_request.start_file_index, request.max_files);
    }

    #[test]
    fn completed_healthy_sweep_waits_for_retry_interval() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([snapshot(b"idle", true, None, Some("/downloads"), 0, 0)]);

        let request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected initial request");

        let outcome = scheduler.on_probe_batch_result(
            b"idle",
            FileProbeBatchResult {
                epoch: request.epoch,
                scanned_files: request.max_files,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        );
        assert_eq!(
            outcome,
            Some(ProbeBatchOutcome::CompletedSweep {
                problem_files: Vec::new()
            })
        );
        assert!(scheduler.drain_due_probe_requests().is_empty());

        scheduler.advance_time(IDLE_HEALTHY_RETRY_INTERVAL - Duration::from_secs(1));
        assert!(scheduler.drain_due_probe_requests().is_empty());

        scheduler.advance_time(Duration::from_secs(1));
        assert_eq!(scheduler.drain_due_probe_requests().len(), 1);
    }

    #[test]
    fn healthy_probes_are_suppressed_while_torrent_is_still_downloading() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([downloading_snapshot(
            b"active-download",
            true,
            Some(10),
            Some("/downloads/active"),
        )]);

        assert!(scheduler.drain_due_probe_requests().is_empty());
        assert_eq!(scheduler.next_probe_in(b"active-download"), None);
    }

    #[test]
    fn downloading_torrent_still_probes_immediately_after_data_fault() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([downloading_snapshot(
            b"faulted-download",
            true,
            Some(10),
            Some("/downloads/faulted"),
        )]);

        assert!(scheduler.drain_due_probe_requests().is_empty());

        scheduler.on_data_availability_fault(b"faulted-download");

        let request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected recovery probe for faulted download");
        assert_eq!(request.info_hash, b"faulted-download".to_vec());
    }

    #[test]
    fn synthetic_million_file_sweep_makes_forward_progress() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        let total_files = 1_000_000usize;
        scheduler.sync_torrents([snapshot(
            b"large",
            false,
            Some(total_files),
            Some("/downloads/large"),
            0,
            0,
        )]);

        let mut expected_start = 0usize;
        let mut completed = false;

        while !completed {
            let request = scheduler
                .drain_due_probe_requests()
                .into_iter()
                .next()
                .expect("expected batch request");
            assert_eq!(request.start_file_index, expected_start);

            let next_file_index = (request.start_file_index + request.max_files).min(total_files);
            let reached_end = next_file_index >= total_files;
            let outcome = scheduler.on_probe_batch_result(
                b"large",
                FileProbeBatchResult {
                    epoch: request.epoch,
                    scanned_files: next_file_index - request.start_file_index,
                    next_file_index: if reached_end { 0 } else { next_file_index },
                    reached_end_of_manifest: reached_end,
                    pending_metadata: false,
                    problem_files: Vec::new(),
                },
            );

            if reached_end {
                assert_eq!(
                    outcome,
                    Some(ProbeBatchOutcome::CompletedSweep {
                        problem_files: Vec::new()
                    })
                );
                completed = true;
            } else {
                assert_eq!(outcome, Some(ProbeBatchOutcome::SweepInProgress));
                expected_start = next_file_index;
            }
        }
    }

    #[test]
    fn data_fault_schedules_same_saved_location_immediately() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([
            snapshot(
                b"faulted",
                true,
                None,
                Some("/downloads/shared/faulted"),
                0,
                0,
            ),
            snapshot(
                b"sibling",
                true,
                None,
                Some("/downloads/shared/faulted"),
                0,
                0,
            ),
            snapshot(b"other", true, None, Some("/downloads/shared/other"), 0, 0),
        ]);

        let mut settled = HashSet::new();
        while settled.len() < 3 {
            let requests = scheduler.drain_due_probe_requests();
            assert!(!requests.is_empty());

            for request in requests {
                settled.insert(request.info_hash.clone());
                let _ = scheduler.on_probe_batch_result(
                    &request.info_hash,
                    FileProbeBatchResult {
                        epoch: request.epoch,
                        scanned_files: 1,
                        next_file_index: 0,
                        reached_end_of_manifest: true,
                        pending_metadata: false,
                        problem_files: Vec::new(),
                    },
                );
            }
        }

        scheduler.on_data_availability_fault(b"faulted");
        let follow_up = scheduler.drain_due_probe_requests();
        assert_eq!(follow_up.len(), 2);
        assert!(follow_up
            .iter()
            .any(|request| request.info_hash == b"faulted".to_vec()));
        assert!(follow_up
            .iter()
            .any(|request| request.info_hash == b"sibling".to_vec()));
        assert!(!follow_up
            .iter()
            .any(|request| request.info_hash == b"other".to_vec()));
    }

    #[test]
    fn data_fault_does_not_schedule_other_torrents_with_only_shared_root() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([
            snapshot(
                b"faulted",
                true,
                None,
                Some("/downloads/shared/faulted"),
                0,
                0,
            ),
            snapshot(
                b"sibling",
                true,
                None,
                Some("/downloads/shared/sibling"),
                0,
                0,
            ),
        ]);

        let mut settled = HashSet::new();
        while settled.len() < 2 {
            let requests = scheduler.drain_due_probe_requests();
            assert!(!requests.is_empty());

            for request in requests {
                settled.insert(request.info_hash.clone());
                let _ = scheduler.on_probe_batch_result(
                    &request.info_hash,
                    FileProbeBatchResult {
                        epoch: request.epoch,
                        scanned_files: 1,
                        next_file_index: 0,
                        reached_end_of_manifest: true,
                        pending_metadata: false,
                        problem_files: Vec::new(),
                    },
                );
            }
        }

        scheduler.on_data_availability_fault(b"faulted");
        let follow_up = scheduler.drain_due_probe_requests();
        assert_eq!(follow_up.len(), 1);
        assert_eq!(follow_up[0].info_hash, b"faulted".to_vec());
    }

    #[test]
    fn stale_batch_result_is_ignored_after_fault_epoch_bump() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([snapshot(
            b"faulted",
            true,
            None,
            Some("/downloads/shared"),
            0,
            0,
        )]);

        let request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected initial request");

        scheduler.on_data_availability_fault(b"faulted");

        let outcome = scheduler.on_probe_batch_result(
            b"faulted",
            FileProbeBatchResult {
                epoch: request.epoch,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        );
        assert!(outcome.is_none());

        let replacement = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected replacement request");
        assert_eq!(replacement.info_hash, b"faulted".to_vec());
        assert!(replacement.epoch > request.epoch);
    }

    #[test]
    fn timed_out_probe_batch_is_reclaimed_and_reissued_from_start() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([snapshot(
            b"stalled",
            true,
            None,
            Some("/downloads/shared"),
            0,
            0,
        )]);

        let request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected initial request");
        assert_eq!(request.start_file_index, 0);

        scheduler.advance_time(PROBE_BATCH_TIMEOUT - Duration::from_secs(1));
        assert!(scheduler.drain_due_probe_requests().is_empty());

        scheduler.advance_time(Duration::from_secs(1));
        let replacement = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected timed-out request to be reissued");

        assert_eq!(replacement.info_hash, b"stalled".to_vec());
        assert_eq!(replacement.start_file_index, 0);
        assert!(replacement.epoch > request.epoch);
    }

    #[test]
    fn stale_batch_result_is_ignored_after_timeout_epoch_bump() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([snapshot(
            b"stalled",
            true,
            None,
            Some("/downloads/shared"),
            0,
            0,
        )]);

        let request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected initial request");

        scheduler.advance_time(PROBE_BATCH_TIMEOUT);
        let replacement = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected timed-out request to be reissued");
        assert!(replacement.epoch > request.epoch);

        let outcome = scheduler.on_probe_batch_result(
            b"stalled",
            FileProbeBatchResult {
                epoch: request.epoch,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: vec![missing_entry("missing.bin")],
            },
        );
        assert!(outcome.is_none());

        let replacement_outcome = scheduler.on_probe_batch_result(
            b"stalled",
            FileProbeBatchResult {
                epoch: replacement.epoch,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        );
        assert_eq!(
            replacement_outcome,
            Some(ProbeBatchOutcome::CompletedSweep {
                problem_files: Vec::new()
            })
        );
    }

    #[test]
    fn small_manifest_healthy_sweep_retries_after_sixty_seconds() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([snapshot(
            b"small",
            true,
            Some(SMALL_MANIFEST_FILE_COUNT_THRESHOLD - 1),
            Some("/downloads/small"),
            0,
            0,
        )]);

        let request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected initial request");

        let outcome = scheduler.on_probe_batch_result(
            b"small",
            FileProbeBatchResult {
                epoch: request.epoch,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        );
        assert_eq!(
            outcome,
            Some(ProbeBatchOutcome::CompletedSweep {
                problem_files: Vec::new()
            })
        );

        scheduler.advance_time(SMALL_MANIFEST_HEALTHY_RETRY_INTERVAL - Duration::from_secs(1));
        assert!(scheduler.drain_due_probe_requests().is_empty());

        scheduler.advance_time(Duration::from_secs(1));
        assert_eq!(scheduler.drain_due_probe_requests().len(), 1);
    }

    #[test]
    fn large_active_manifest_keeps_standard_healthy_retry_interval() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        scheduler.sync_torrents([snapshot(
            b"active-large",
            true,
            Some(SMALL_MANIFEST_FILE_COUNT_THRESHOLD),
            Some("/downloads/large"),
            1,
            0,
        )]);

        let request = scheduler
            .drain_due_probe_requests()
            .into_iter()
            .next()
            .expect("expected initial request");

        let outcome = scheduler.on_probe_batch_result(
            b"active-large",
            FileProbeBatchResult {
                epoch: request.epoch,
                scanned_files: 1,
                next_file_index: 0,
                reached_end_of_manifest: true,
                pending_metadata: false,
                problem_files: Vec::new(),
            },
        );
        assert_eq!(
            outcome,
            Some(ProbeBatchOutcome::CompletedSweep {
                problem_files: Vec::new()
            })
        );

        scheduler.advance_time(ACTIVE_HEALTHY_RETRY_INTERVAL - Duration::from_secs(1));
        assert!(scheduler.drain_due_probe_requests().is_empty());

        scheduler.advance_time(Duration::from_secs(1));
        assert_eq!(scheduler.drain_due_probe_requests().len(), 1);
    }

    #[test]
    fn healthy_deadline_mismatch_detects_small_manifest_on_long_deadline() {
        let now = Instant::now();
        let mut state = IntegrityTorrentState::new(now);
        state.has_completed_probe = true;
        state.availability = DataAvailabilityState::Available;
        state.file_count = Some(1);
        state.next_due_at = now + IDLE_HEALTHY_RETRY_INTERVAL;

        assert_eq!(
            state.healthy_deadline_mismatch(now),
            Some((
                IDLE_HEALTHY_RETRY_INTERVAL,
                SMALL_MANIFEST_HEALTHY_RETRY_INTERVAL
            ))
        );
    }

    #[test]
    fn healthy_deadline_mismatch_detects_shorter_deadlines_and_ignores_matching() {
        let now = Instant::now();
        let mut state = IntegrityTorrentState::new(now);
        state.has_completed_probe = true;
        state.availability = DataAvailabilityState::Available;
        state.file_count = Some(1);
        state.next_due_at = now + Duration::from_secs(30);
        assert_eq!(
            state.healthy_deadline_mismatch(now),
            Some((
                Duration::from_secs(30),
                SMALL_MANIFEST_HEALTHY_RETRY_INTERVAL
            ))
        );

        state.next_due_at = now + SMALL_MANIFEST_HEALTHY_RETRY_INTERVAL;
        assert_eq!(state.healthy_deadline_mismatch(now), None);
    }

    #[test]
    fn sync_torrents_shortens_stale_healthy_deadline_to_small_manifest_policy() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        let info_hash = b"small-stale".to_vec();
        let mut state = IntegrityTorrentState::new(now);
        state.has_completed_probe = true;
        state.availability = DataAvailabilityState::Available;
        state.file_count = Some(1);
        state.next_due_at = now + IDLE_HEALTHY_RETRY_INTERVAL;
        scheduler.torrents.insert(info_hash.clone(), state);

        scheduler.sync_torrents([snapshot(
            &info_hash,
            true,
            Some(1),
            Some("/downloads/small"),
            0,
            0,
        )]);

        assert_eq!(
            scheduler.next_probe_in(&info_hash),
            Some(SMALL_MANIFEST_HEALTHY_RETRY_INTERVAL)
        );
    }

    #[test]
    fn sync_torrents_extends_stale_healthy_deadline_to_idle_policy() {
        let now = Instant::now();
        let mut scheduler = IntegrityScheduler::new(now);
        let info_hash = b"idle-stale".to_vec();
        let mut state = IntegrityTorrentState::new(now);
        state.has_completed_probe = true;
        state.availability = DataAvailabilityState::Available;
        state.file_count = Some(1);
        state.next_due_at = now + SMALL_MANIFEST_HEALTHY_RETRY_INTERVAL;
        scheduler.torrents.insert(info_hash.clone(), state);

        scheduler.sync_torrents([snapshot(
            &info_hash,
            true,
            None,
            Some("/downloads/idle"),
            0,
            0,
        )]);

        assert_eq!(
            scheduler.next_probe_in(&info_hash),
            Some(IDLE_HEALTHY_RETRY_INTERVAL)
        );
    }
}
