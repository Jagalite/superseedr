// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application presentation definitions and transitions.

use super::*;

pub(crate) fn clamp_selected_indices_in_state(app_state: &mut AppState) {
    let torrent_count = app_state.torrent_list_order.len();

    if torrent_count == 0 {
        app_state.ui.selected_torrent_index = 0;
    } else if app_state.ui.selected_torrent_index >= torrent_count {
        app_state.ui.selected_torrent_index = torrent_count - 1;
    }

    let peer_count = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash))
        .map_or(0, |torrent| torrent.latest_state.peers.len());

    if peer_count == 0 {
        app_state.ui.selected_peer_index = 0;
    } else if app_state.ui.selected_peer_index >= peer_count {
        app_state.ui.selected_peer_index = peer_count - 1;
    }
}

/// Advances production Normal-screen effects from platform-neutral service snapshots.
pub(crate) fn advance_ui_effects_for_frame(
    app_state: &mut AppState,
    settings: &Settings,
    dht_status: &DhtStatus,
    dht_wave_telemetry: &DhtWaveTelemetry,
) {
    let frame_wall_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    if app_state.ui.effects_last_wall_time <= 0.0 {
        app_state.ui.effects_last_wall_time = frame_wall_time;
    }
    let frame_dt = frame_wall_time - app_state.ui.effects_last_wall_time;
    app_state.ui.effects_last_wall_time = frame_wall_time;
    advance_ui_effects_for_elapsed(
        app_state,
        settings,
        dht_status,
        dht_wave_telemetry,
        frame_dt,
    );
}

pub(crate) fn advance_ui_effects_for_elapsed(
    app_state: &mut AppState,
    settings: &Settings,
    dht_status: &DhtStatus,
    dht_wave_telemetry: &DhtWaveTelemetry,
    frame_dt: f64,
) {
    let now = Instant::now();
    let mut cleared_port_highlight = false;
    if app_state
        .externally_accessable_port_v4_highlight_until
        .is_some_and(|deadline| deadline <= now)
    {
        app_state.externally_accessable_port_v4_highlight_until = None;
        cleared_port_highlight = true;
    }
    if app_state
        .externally_accessable_port_v6_highlight_until
        .is_some_and(|deadline| deadline <= now)
    {
        app_state.externally_accessable_port_v6_highlight_until = None;
        cleared_port_highlight = true;
    }
    if cleared_port_highlight {
        app_state.ui.needs_redraw = true;
    }

    let effects_dt = compute_effects_phase_delta(app_state.theme.name, frame_dt);
    let frame_dt = frame_dt.clamp(0.0, 0.25);
    let activity_speed_multiplier = compute_effects_activity_speed_multiplier(app_state, settings);
    app_state.ui.effects_speed_multiplier = activity_speed_multiplier;
    app_state.ui.effects_phase_time += effects_dt * activity_speed_multiplier;

    let (target_discovery_boost, download_steps_per_second, upload_steps_per_second) = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash))
        .map(|torrent| {
            (
                (torrent.peers_discovered_this_tick as f64 / 10.0).clamp(0.0, 1.0) * 0.18,
                file_activity_wave_steps_per_second(torrent.smoothed_download_speed_bps),
                file_activity_wave_steps_per_second(torrent.smoothed_upload_speed_bps),
            )
        })
        .unwrap_or_else(|| {
            (
                0.0,
                file_activity_wave_steps_per_second(0),
                file_activity_wave_steps_per_second(0),
            )
        });

    let target_wave = dht_wave_targets(dht_status, dht_wave_telemetry);
    advance_dht_wave_state(
        &mut app_state.ui.dht_wave,
        target_wave,
        target_discovery_boost,
        frame_dt,
    );
    app_state.ui.file_activity_download_phase += frame_dt * download_steps_per_second;
    app_state.ui.file_activity_upload_phase += frame_dt * upload_steps_per_second;
    update_swarm_availability_flash_state(app_state, now);

    let disk_phase_speed = crate::tui::animation::disk_health_phase_speed(app_state);
    app_state.disk_health_phase = (app_state.disk_health_phase + frame_dt * disk_phase_speed)
        .rem_euclid(std::f64::consts::TAU);
}

pub(super) fn update_swarm_availability_flash_state(app_state: &mut AppState, now: Instant) {
    let selected = app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| {
            app_state
                .torrents
                .get(info_hash)
                .map(|torrent| (info_hash, torrent))
        });
    let flash = &mut app_state.ui.swarm_availability_flash;
    let Some((info_hash, torrent)) = selected else {
        *flash = SwarmAvailabilityFlashState::default();
        return;
    };
    let peers = &torrent.latest_state.peers;
    let total_pieces = torrent.latest_state.number_of_pieces_total;
    if flash.matches_peers(
        info_hash,
        peers,
        total_pieces,
        torrent.latest_state.availability_revision,
    ) {
        // Animation expiration continues even when availability inputs are unchanged.
        flash.clear_expired(now);
        return;
    }
    let current_availability = swarm_availability_counts(peers, total_pieces);
    let current_peer_bitfields = swarm_availability_peer_bitfields(peers, total_pieces as usize);
    flash.update_from_peer_availability(
        info_hash,
        current_availability,
        current_peer_bitfields,
        now,
        SWARM_AVAILABILITY_FLASH_DURATION,
    );
    flash.remember_peer_keys(peers);
    flash.availability_revision = torrent.latest_state.availability_revision;
}

pub(crate) fn file_activity_wave_steps_per_second(speed_bps: u64) -> f64 {
    if speed_bps == 0 {
        12.0
    } else if speed_bps < 50_000 {
        11.0
    } else if speed_bps < 500_000 {
        12.5
    } else if speed_bps < 2_000_000 {
        14.0
    } else if speed_bps < 10_000_000 {
        16.0
    } else if speed_bps < 20_000_000 {
        17.5
    } else if speed_bps < 50_000_000 {
        19.0
    } else if speed_bps < 100_000_000 {
        21.0
    } else {
        23.0
    }
}

pub(crate) fn sort_and_filter_torrent_list_state(app_state: &mut AppState) {
    let torrents_map = &app_state.torrents;
    let (sort_by, sort_direction) = app_state.torrent_sort;
    let search_query = &app_state.ui.search_query;

    let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
    let mut torrent_list: Vec<Vec<u8>> = torrents_map.keys().cloned().collect();

    if !search_query.is_empty() {
        torrent_list.retain(|info_hash| {
            let torrent_name = torrents_map
                .get(info_hash)
                .map_or("", |t| &t.latest_state.torrent_name);
            matcher.fuzzy_match(torrent_name, search_query).is_some()
        });
    }

    torrent_list.sort_by(|a_info_hash, b_info_hash| {
        let Some(a_torrent) = torrents_map.get(a_info_hash) else {
            return std::cmp::Ordering::Equal;
        };
        let Some(b_torrent) = torrents_map.get(b_info_hash) else {
            return std::cmp::Ordering::Equal;
        };

        if !app_state.torrent_sort_pinned {
            let availability_ordering = a_torrent
                .latest_state
                .data_available
                .cmp(&b_torrent.latest_state.data_available);
            if availability_ordering != std::cmp::Ordering::Equal {
                return availability_ordering;
            }
        }

        let ordering = match sort_by {
            TorrentSortColumn::Name => a_torrent
                .latest_state
                .torrent_name
                .cmp(&b_torrent.latest_state.torrent_name),
            TorrentSortColumn::Down => b_torrent
                .smoothed_download_speed_bps
                .cmp(&a_torrent.smoothed_download_speed_bps),
            TorrentSortColumn::Up => b_torrent
                .smoothed_upload_speed_bps
                .cmp(&a_torrent.smoothed_upload_speed_bps),
            TorrentSortColumn::Progress => {
                let calc_progress = |t: &TorrentDisplayState| -> f64 {
                    if t.latest_state.number_of_pieces_total == 0 {
                        0.0
                    } else {
                        t.latest_state.number_of_pieces_completed as f64
                            / t.latest_state.number_of_pieces_total as f64
                    }
                };

                let a_prog = calc_progress(a_torrent);
                let b_prog = calc_progress(b_torrent);
                a_prog.total_cmp(&b_prog)
            }
        };

        let default_direction = sort_by.default_direction();
        let primary_ordering = if sort_direction != default_direction {
            ordering.reverse()
        } else {
            ordering
        };

        primary_ordering.then_with(|| {
            let calculate_weighted_activity = |t: &TorrentDisplayState| -> u64 {
                let window = 60;
                let mut score = 0;
                let mut sum_vec = |history: &Vec<u64>| {
                    for (i, &count) in history.iter().rev().take(window).enumerate() {
                        if count > 0 {
                            let weight = if i < 5 { (5 - i) as u64 * 10 } else { 1 };
                            score += count * weight;
                        }
                    }
                };
                sum_vec(&t.peer_discovery_history);
                sum_vec(&t.peer_connection_history);
                sum_vec(&t.peer_disconnect_history);
                score
            };

            let a_activity = calculate_weighted_activity(a_torrent);
            let b_activity = calculate_weighted_activity(b_torrent);
            b_activity.cmp(&a_activity)
        })
    });

    app_state.torrent_list_order = torrent_list;
    // Keep the cursor on its row as live metrics reorder torrents, including the top row.
    clamp_selected_indices_in_state(app_state);
}

pub(crate) fn has_effectively_incomplete_torrents(app_state: &AppState) -> bool {
    app_state
        .torrents
        .values()
        .any(|torrent| torrent_is_effectively_incomplete(&torrent.latest_state))
}

pub(crate) fn refresh_autosort_after_stats(
    app_state: &mut AppState,
    previous_torrent_sort: (TorrentSortColumn, SortDirection),
    previous_peer_sort: (PeerSortColumn, SortDirection),
) -> bool {
    let previous_torrent_order = app_state.torrent_list_order.clone();
    let torrent_sort_changed = app_state.torrent_sort != previous_torrent_sort;
    sort_and_filter_torrent_list_state(app_state);

    let peer_sort_changed = app_state.peer_sort != previous_peer_sort;

    torrent_sort_changed
        || app_state.torrent_list_order != previous_torrent_order
        || peer_sort_changed
}

pub(super) fn set_torrent_sort_to_column(app_state: &mut AppState, column: TorrentSortColumn) {
    app_state.torrent_sort = (column, column.default_direction());
}

pub(super) fn set_peer_sort_to_column(app_state: &mut AppState, column: PeerSortColumn) {
    app_state.peer_sort = (column, column.default_direction());
}

pub(crate) fn set_automatic_torrent_sort(app_state: &mut AppState, column: TorrentSortColumn) {
    set_torrent_sort_to_column(app_state, column);
    app_state.torrent_sort_pinned = false;
    sort_and_filter_torrent_list_state(app_state);
    app_state.ui.needs_redraw = true;
}

pub(crate) fn reset_torrent_sort_for_current_lifecycle(app_state: &mut AppState) {
    let column = if has_effectively_incomplete_torrents(app_state) {
        TorrentSortColumn::Down
    } else {
        TorrentSortColumn::Up
    };
    set_automatic_torrent_sort(app_state, column);
}

pub(crate) fn refresh_torrent_sort_after_removal(app_state: &mut AppState) {
    if app_state.torrent_sort_pinned {
        sort_and_filter_torrent_list_state(app_state);
        app_state.ui.needs_redraw = true;
    } else {
        reset_torrent_sort_for_current_lifecycle(app_state);
    }
}

pub(crate) fn align_unpinned_peer_sort_with_visible_activity(app_state: &mut AppState) {
    if !app_state.peer_sort_pinned {
        let selected_torrent = app_state
            .torrent_list_order
            .get(app_state.ui.selected_torrent_index)
            .and_then(|info_hash| app_state.torrents.get(info_hash));
        let has_download_activity = selected_torrent.is_some_and(|torrent| {
            torrent
                .latest_state
                .peers
                .iter()
                .any(|peer| peer.download_speed_bps > 0)
        });
        let has_upload_activity = selected_torrent.is_some_and(|torrent| {
            torrent
                .latest_state
                .peers
                .iter()
                .any(|peer| peer.upload_speed_bps > 0)
        });

        let target = if has_download_activity && (!app_state.is_seeding || !has_upload_activity) {
            PeerSortColumn::DL
        } else if has_upload_activity || app_state.is_seeding {
            PeerSortColumn::UL
        } else {
            PeerSortColumn::DL
        };

        if app_state.peer_sort.0 != target {
            set_peer_sort_to_column(app_state, target);
        }
    }
}
