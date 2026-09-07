// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native execution of application effects produced by manager observations.
//!
//! Metadata/removal and foreground availability projections are reduced in shared code.
//! Freshness and sweep assembly remain with the existing integrity scheduler.

use super::*;

impl App {
    pub(super) fn observe_service(&mut self, observation: crate::app::reducer::ServiceObservation) {
        for effect in
            reduce_app_action(&mut self.app_state, AppAction::ServiceObserved(observation))
        {
            self.execute_manager_effect(effect);
        }
    }

    pub(super) fn handle_manager_event(&mut self, event: ManagerEvent) {
        for effect in reduce_app_action(&mut self.app_state, AppAction::ManagerEvent(event)) {
            self.execute_manager_effect(effect);
        }
    }

    pub(super) fn execute_manager_effect(&mut self, effect: AppEffect) {
        match effect {
            AppEffect::RefreshRss => self.refresh_rss_derived(),
            AppEffect::CheckpointRequested => self.save_state_to_disk(),
            AppEffect::TorrentRemoved {
                info_hash,
                result,
                was_present: _was_present,
                recovery,
            } => {
                if let Err(e) = &result {
                    tracing_event!(Level::ERROR, "Deletion failed for torrent: {}", e);
                }
                let can_write = self.can_write_shared_state();
                crate::app::reducer::reconcile_removed_catalog(
                    &mut self.client_configs,
                    &info_hash,
                    &result,
                    can_write,
                    recovery,
                );

                self.manager_lifetimes.remove(&info_hash);
                self.torrent_manager_command_txs.remove(&info_hash);
                self.torrent_manager_incoming_peer_txs.remove(&info_hash);
                self.torrent_metric_watch_rxs.remove(&info_hash);
                let _ = self
                    .peer_manager
                    .handle()
                    .unregister_torrent(info_hash.clone());
                self.integrity_scheduler.remove_torrent(&info_hash);
                self.save_state_to_disk();
                self.refresh_rss_derived();
                self.dispatch_integrity_probe_batches();

                self.app_state.ui.needs_redraw = true;
            }
            AppEffect::DataAvailabilityFault {
                info_hash,
                piece_index,
                error,
                availability_changed,
            } => {
                self.integrity_scheduler
                    .on_data_availability_fault(&info_hash);

                let should_log_fault = self
                    .data_availability_fault_log_cooldowns
                    .entry(info_hash.clone())
                    .or_default()
                    .should_log(Instant::now(), REPEATED_HEALTH_LOG_INTERVAL);
                if should_log_fault {
                    if let Some(torrent) = self.app_state.torrents.get(&info_hash) {
                        let saved_location = Self::torrent_saved_location(&torrent.latest_state);
                        tracing_event!(
                            Level::WARN,
                            info_hash = %hex::encode(&info_hash),
                            torrent = %torrent.latest_state.torrent_name,
                            piece = piece_index as usize,
                            saved_location = ?saved_location,
                            error = %error,
                            "Foreground disk read marked torrent data unavailable"
                        );
                    }
                }

                if availability_changed {
                    let torrent_name = self
                        .app_state
                        .torrents
                        .get(&info_hash)
                        .map(|torrent| torrent.latest_state.torrent_name.clone());
                    self.record_data_health_event(
                        &info_hash,
                        torrent_name,
                        EventType::DataUnavailable,
                        Vec::new(),
                        format!(
                            "Foreground disk read marked torrent data unavailable at piece {}",
                            piece_index
                        ),
                    );
                }

                if availability_changed {
                    self.save_state_to_disk();
                }

                self.dispatch_integrity_probe_batches();
                self.app_state.ui.needs_redraw = true;
            }
            AppEffect::ProcessFileProbeBatch { info_hash, result } => {
                let probe_result_availability = data_availability_from_file_probe_result(&result);
                let completed_sweep = self
                    .integrity_scheduler
                    .on_probe_batch_result(&info_hash, result);
                let completed_sweep = completed_sweep.map(|outcome| match outcome {
                    ProbeBatchOutcome::PendingMetadata => {
                        crate::app::reducer::health::ProbeSweep::PendingMetadata
                    }
                    ProbeBatchOutcome::SweepInProgress => {
                        crate::app::reducer::health::ProbeSweep::SweepInProgress
                    }
                    ProbeBatchOutcome::CompletedSweep { problem_files } => {
                        crate::app::reducer::health::ProbeSweep::CompletedSweep { problem_files }
                    }
                });
                let crate::app::reducer::health::DataHealthEffects {
                    availability_transition_log,
                    should_notify_manager_unavailable,
                    should_request_recovery,
                    should_persist_unavailable,
                } = crate::app::reducer::health::apply_probe_observation(
                    &mut self.app_state,
                    &info_hash,
                    completed_sweep,
                    probe_result_availability,
                );

                if should_notify_manager_unavailable {
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        let _ = manager_tx.try_send(ManagerCommand::SetDataAvailability(false));
                    }
                }
                if should_persist_unavailable && availability_transition_log.is_none() {
                    self.save_state_to_disk();
                }

                if let Some((
                    torrent_name,
                    is_available,
                    issue_count,
                    saved_location,
                    issue_files,
                )) = availability_transition_log
                {
                    if is_available {
                        let should_log_available = self
                            .probe_available_log_cooldowns
                            .entry(info_hash.clone())
                            .or_default()
                            .should_log(Instant::now(), REPEATED_HEALTH_LOG_INTERVAL);
                        if should_log_available {
                            tracing_event!(
                                Level::INFO,
                                info_hash = %hex::encode(&info_hash),
                                torrent = %torrent_name,
                                saved_location = ?saved_location,
                                "Torrent probe found data available; awaiting manager metrics confirmation"
                            );
                        }
                    } else {
                        tracing_event!(
                            Level::WARN,
                            info_hash = %hex::encode(&info_hash),
                            torrent = %torrent_name,
                            saved_location = ?saved_location,
                            issues = issue_count,
                            issue_files = ?issue_files,
                            "Torrent probe found data unavailable"
                        );
                        if should_persist_unavailable {
                            self.save_state_to_disk();
                        }
                    }

                    self.record_data_health_event(
                        &info_hash,
                        Some(torrent_name),
                        if is_available {
                            EventType::DataRecovered
                        } else {
                            EventType::DataUnavailable
                        },
                        issue_files,
                        if is_available {
                            "Torrent probe found data available".to_string()
                        } else {
                            format!(
                                "Torrent probe found data unavailable with {} issue(s)",
                                issue_count
                            )
                        },
                    );
                    if is_available || !should_persist_unavailable {
                        self.save_state_to_disk();
                    }
                }

                if should_request_recovery {
                    if let Some(manager_tx) = self.torrent_manager_command_txs.get(&info_hash) {
                        let _ = manager_tx.try_send(ManagerCommand::SetDataAvailability(true));
                    }
                }

                self.dispatch_integrity_probe_batches();
                self.app_state.ui.needs_redraw = true;
            }
            AppEffect::MetadataLoaded {
                info_hash,
                torrent,
                file_priorities,
            } => {
                self.integrity_scheduler.on_metadata_loaded(&info_hash);
                self.persist_torrent_metadata_snapshot(&info_hash, &torrent, &file_priorities);
                self.dispatch_integrity_probe_batches();
            }
            // Completion notifications are emitted by the metrics path, not manager events.
            AppEffect::TorrentCompleted { .. } => {}
        }
    }
}
