// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! App data-health projection from freshness-checked integrity scheduler observations.
//! The scheduler remains the owner of probe epochs, work selection and sweep assembly.

use super::super::*;
use crate::app::torrent_manager_protocol::FileProbeEntry;

pub(crate) enum ProbeSweep {
    PendingMetadata,
    SweepInProgress,
    CompletedSweep { problem_files: Vec<FileProbeEntry> },
}

pub(crate) struct DataHealthEffects {
    pub availability_transition_log: Option<AvailabilityTransitionLog>,
    pub should_notify_manager_unavailable: bool,
    pub should_request_recovery: bool,
    pub should_persist_unavailable: bool,
}

pub(crate) fn apply_probe_observation(
    state: &mut AppState,
    info_hash: &[u8],
    completed_sweep: Option<ProbeSweep>,
    probe_result_availability: Option<bool>,
) -> DataHealthEffects {
    let mut availability_transition_log: Option<AvailabilityTransitionLog> = None;
    let mut should_notify_manager_unavailable = false;
    let mut should_request_recovery = false;
    let mut should_persist_unavailable = false;

    if let Some(torrent) = state.torrents.get_mut(info_hash) {
        if completed_sweep.is_some() && matches!(probe_result_availability, Some(false)) {
            should_notify_manager_unavailable = torrent.latest_state.data_available;
            torrent.latest_state.data_available = false;
            should_persist_unavailable |= should_notify_manager_unavailable;
        }

        match completed_sweep {
            Some(ProbeSweep::PendingMetadata) => {
                torrent.latest_file_probe_status = Some(TorrentFileProbeStatus::PendingMetadata);
            }
            Some(ProbeSweep::SweepInProgress) => {}
            Some(ProbeSweep::CompletedSweep { problem_files }) => {
                let was_available = torrent.latest_state.data_available;
                let next_availability = probe_result_availability.unwrap_or(was_available);
                let issue_count = problem_files.len();
                let issue_files = problem_files
                    .iter()
                    .map(|entry| format!("{}: {}", entry.absolute_path.display(), entry.error))
                    .collect::<Vec<_>>();

                torrent.latest_file_probe_status =
                    Some(TorrentFileProbeStatus::Files(problem_files));
                if next_availability != was_available {
                    let saved_location = torrent_saved_location(&torrent.latest_state);
                    availability_transition_log = Some((
                        torrent.latest_state.torrent_name.clone(),
                        next_availability,
                        issue_count,
                        saved_location,
                        issue_files,
                    ));
                }

                if matches!(probe_result_availability, Some(false)) {
                    torrent.latest_state.data_available = false;
                    should_persist_unavailable |= was_available;
                }
                if matches!(probe_result_availability, Some(true)) && !was_available {
                    should_request_recovery = true;
                }
            }
            None => {}
        }
    }

    state.ui.needs_redraw = true;
    DataHealthEffects {
        availability_transition_log,
        should_notify_manager_unavailable,
        should_request_recovery,
        should_persist_unavailable,
    }
}

pub(crate) fn torrent_saved_location(metrics: &TorrentMetrics) -> Option<PathBuf> {
    let download_path = metrics.download_path.as_ref()?;

    match metrics.container_name.as_deref() {
        Some(container_name) if !container_name.is_empty() => {
            Some(download_path.join(container_name))
        }
        // Explicit empty-container multi-file torrents save directly into the root directory.
        Some(_) if metrics.is_multi_file => Some(download_path.clone()),
        // Flat payloads need a torrent-specific identity rather than the shared parent folder.
        _ => Some(download_path.join(&metrics.torrent_name)),
    }
}
