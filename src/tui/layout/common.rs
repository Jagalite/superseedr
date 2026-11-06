// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::torrent_is_effectively_incomplete;
use crate::app::AppState;
use crate::config::{PeerSortColumn, TorrentSortColumn};
use crate::tui::formatters::format_speed;
use ratatui::prelude::Constraint;

const SPEED_COLUMN_MIN_WIDTH: u16 = 11;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnId {
    Status,
    Name,
    DownSpeed,
    UpSpeed,
}

pub struct ColumnDefinition {
    pub id: ColumnId,
    pub header: &'static str,
    pub min_width: u16,
    pub priority: u8,
    pub default_constraint: Constraint,
    pub sort_enum: Option<TorrentSortColumn>,
}

pub fn get_torrent_columns() -> Vec<ColumnDefinition> {
    vec![
        ColumnDefinition {
            id: ColumnId::Status,
            header: "Done",
            min_width: 7,
            priority: 2,
            default_constraint: Constraint::Length(7),
            sort_enum: Some(TorrentSortColumn::Progress),
        },
        ColumnDefinition {
            id: ColumnId::Name,
            header: "Name",
            min_width: 15,
            priority: 0,
            default_constraint: Constraint::Fill(1),
            sort_enum: Some(TorrentSortColumn::Name),
        },
        ColumnDefinition {
            id: ColumnId::UpSpeed,
            header: "UL",
            min_width: SPEED_COLUMN_MIN_WIDTH,
            priority: 1,
            default_constraint: Constraint::Length(SPEED_COLUMN_MIN_WIDTH),
            sort_enum: Some(TorrentSortColumn::Up),
        },
        ColumnDefinition {
            id: ColumnId::DownSpeed,
            header: "DL",
            min_width: SPEED_COLUMN_MIN_WIDTH,
            priority: 1,
            default_constraint: Constraint::Length(SPEED_COLUMN_MIN_WIDTH),
            sort_enum: Some(TorrentSortColumn::Down),
        },
    ]
}

pub fn torrent_has_download_activity(app_state: &AppState) -> bool {
    app_state
        .torrents
        .values()
        .any(|t| t.smoothed_download_speed_bps > 0)
}

pub fn torrent_has_upload_activity(app_state: &AppState) -> bool {
    app_state
        .torrents
        .values()
        .any(|t| t.smoothed_upload_speed_bps > 0)
}

/// Restored seeds look incomplete until their check finishes, so while startup is rolling
/// them in only other torrents count; real download progress still shows.
pub fn has_incomplete_torrents(app_state: &AppState) -> bool {
    app_state.torrents.iter().any(|(info_hash, t)| {
        torrent_is_effectively_incomplete(&t.latest_state)
            && !app_state.startup_expected_seeds.contains(info_hash)
    })
}

pub fn has_unavailable_torrents(app_state: &AppState) -> bool {
    app_state
        .torrents
        .values()
        .any(|t| !t.latest_state.data_available)
}

pub fn active_torrent_column_indices(app_state: &AppState) -> Vec<usize> {
    let has_dl_activity = torrent_has_download_activity(app_state);
    let has_ul_activity = torrent_has_upload_activity(app_state);
    let has_incomplete = has_incomplete_torrents(app_state);
    let has_unavailable = has_unavailable_torrents(app_state);

    get_torrent_columns()
        .iter()
        .enumerate()
        .filter_map(|(idx, col)| {
            let is_active = match col.id {
                ColumnId::DownSpeed => has_dl_activity,
                ColumnId::UpSpeed => has_ul_activity,
                ColumnId::Status => has_incomplete || has_unavailable,
                ColumnId::Name => true,
            };
            is_active.then_some(idx)
        })
        .collect()
}

fn torrent_speed_column_width(app_state: &AppState, column_id: ColumnId) -> u16 {
    let max_label_width = app_state
        .torrents
        .values()
        .map(|torrent| match column_id {
            ColumnId::DownSpeed => torrent.smoothed_download_speed_bps,
            ColumnId::UpSpeed => torrent.smoothed_upload_speed_bps,
            ColumnId::Status | ColumnId::Name => 0,
        })
        .map(format_speed)
        .map(|label| label.len() as u16)
        .max()
        .unwrap_or(0);

    max_label_width.max(SPEED_COLUMN_MIN_WIDTH)
}

pub fn compute_visible_torrent_columns(
    app_state: &AppState,
    available_width: u16,
) -> (Vec<Constraint>, Vec<usize>) {
    let all_cols = get_torrent_columns();
    let active_indices = active_torrent_column_indices(app_state);

    let smart_cols: Vec<SmartCol> = active_indices
        .iter()
        .map(|&idx| {
            let c = &all_cols[idx];
            let speed_width = match c.id {
                ColumnId::DownSpeed | ColumnId::UpSpeed => {
                    Some(torrent_speed_column_width(app_state, c.id))
                }
                ColumnId::Status | ColumnId::Name => None,
            };
            SmartCol {
                min_width: speed_width.unwrap_or(c.min_width),
                priority: c.priority,
                constraint: speed_width
                    .map(Constraint::Length)
                    .unwrap_or(c.default_constraint),
            }
        })
        .collect();

    let (constraints, visible_active_indices) =
        compute_smart_table_layout(&smart_cols, available_width, 1);
    let visible_real_indices: Vec<usize> = visible_active_indices
        .into_iter()
        .filter_map(|idx| active_indices.get(idx).copied())
        .collect();

    (constraints, visible_real_indices)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PeerColumnId {
    Flags,
    Address,
    Client,
    Action,
    Progress,
    DownSpeed,
    UpSpeed,
}

pub struct PeerColumnDefinition {
    pub id: PeerColumnId,
    pub header: &'static str,
    pub min_width: u16,
    pub priority: u8,
    pub default_constraint: Constraint,
    pub sort_enum: Option<PeerSortColumn>,
}

pub fn get_peer_columns() -> Vec<PeerColumnDefinition> {
    vec![
        PeerColumnDefinition {
            id: PeerColumnId::Flags,
            header: "Flag",
            min_width: 4,
            priority: 1,
            default_constraint: Constraint::Length(4),
            sort_enum: Some(PeerSortColumn::Flags),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Progress,
            header: "Status",
            min_width: 6,
            priority: 2,
            default_constraint: Constraint::Length(6),
            sort_enum: Some(PeerSortColumn::Completed),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Address,
            header: "Address",
            min_width: 25,
            priority: 0,
            default_constraint: Constraint::Fill(2),
            sort_enum: Some(PeerSortColumn::Address),
        },
        PeerColumnDefinition {
            id: PeerColumnId::UpSpeed,
            header: "Upload",
            min_width: SPEED_COLUMN_MIN_WIDTH,
            priority: 1,
            default_constraint: Constraint::Fill(1),
            sort_enum: Some(PeerSortColumn::UL),
        },
        PeerColumnDefinition {
            id: PeerColumnId::DownSpeed,
            header: "Download",
            min_width: SPEED_COLUMN_MIN_WIDTH,
            priority: 1,
            default_constraint: Constraint::Fill(1),
            sort_enum: Some(PeerSortColumn::DL),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Client,
            header: "Client",
            min_width: 12,
            priority: 3,
            default_constraint: Constraint::Fill(1),
            sort_enum: Some(PeerSortColumn::Client),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Action,
            header: "Action",
            min_width: 12,
            priority: 5,
            default_constraint: Constraint::Fill(1),
            sort_enum: Some(PeerSortColumn::Action),
        },
    ]
}

pub fn peer_has_download_activity(app_state: &AppState) -> bool {
    app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash))
        .is_some_and(|torrent| {
            torrent
                .latest_state
                .peers
                .iter()
                .any(|peer| peer.download_speed_bps > 0)
        })
}

pub fn peer_has_upload_activity(app_state: &AppState) -> bool {
    app_state
        .torrent_list_order
        .get(app_state.ui.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash))
        .is_some_and(|torrent| {
            torrent
                .latest_state
                .peers
                .iter()
                .any(|peer| peer.upload_speed_bps > 0)
        })
}

pub fn active_peer_column_indices(app_state: &AppState) -> Vec<usize> {
    let has_dl_activity = peer_has_download_activity(app_state);
    let has_ul_activity = peer_has_upload_activity(app_state);

    get_peer_columns()
        .iter()
        .enumerate()
        .filter_map(|(idx, col)| {
            let is_active = match col.id {
                PeerColumnId::DownSpeed => has_dl_activity,
                PeerColumnId::UpSpeed => has_ul_activity,
                PeerColumnId::Flags
                | PeerColumnId::Address
                | PeerColumnId::Client
                | PeerColumnId::Action
                | PeerColumnId::Progress => true,
            };
            is_active.then_some(idx)
        })
        .collect()
}

pub fn compute_visible_peer_columns(
    app_state: &AppState,
    available_width: u16,
) -> (Vec<Constraint>, Vec<usize>) {
    let all_cols = get_peer_columns();
    let active_indices = active_peer_column_indices(app_state);

    let smart_peer_cols: Vec<SmartCol> = active_indices
        .iter()
        .map(|&idx| {
            let c = &all_cols[idx];
            SmartCol {
                min_width: c.min_width,
                priority: c.priority,
                constraint: c.default_constraint,
            }
        })
        .collect();

    let (constraints, visible_active_indices) =
        compute_smart_table_layout(&smart_peer_cols, available_width, 1);
    let visible_real_indices: Vec<usize> = visible_active_indices
        .into_iter()
        .filter_map(|idx| active_indices.get(idx).copied())
        .collect();

    (constraints, visible_real_indices)
}

#[derive(Clone, Debug)]
pub struct SmartCol {
    pub min_width: u16,
    pub priority: u8,
    pub constraint: Constraint,
}

pub fn compute_smart_table_layout(
    columns: &[SmartCol],
    available_width: u16,
    horizontal_padding: u16,
) -> (Vec<Constraint>, Vec<usize>) {
    let mut indexed_cols: Vec<(usize, &SmartCol)> = columns.iter().enumerate().collect();

    indexed_cols.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then(a.0.cmp(&b.0)));

    let mut active_indices = Vec::new();
    let mut current_used_width = 0;

    let expansion_reserve = if available_width < 80 {
        15
    } else if available_width < 140 {
        25
    } else {
        0
    };

    for (idx, col) in indexed_cols {
        let spacing_cost = if active_indices.is_empty() {
            0
        } else {
            horizontal_padding
        };

        if col.priority == 0 {
            active_indices.push(idx);
            current_used_width += col.min_width + spacing_cost;
        } else {
            let projected_width = current_used_width + col.min_width + spacing_cost;
            let effective_budget = available_width.saturating_sub(expansion_reserve);

            if projected_width <= effective_budget {
                active_indices.push(idx);
                current_used_width = projected_width;
            }
        }
    }

    active_indices.sort();

    let final_constraints = active_indices
        .iter()
        .map(|&i| columns[i].constraint)
        .collect();

    (final_constraints, active_indices)
}

#[cfg(test)]
mod tests {
    use super::{
        compute_visible_peer_columns, compute_visible_torrent_columns, get_peer_columns,
        get_torrent_columns, ColumnId, PeerColumnId,
    };
    use crate::app::{AppState, PeerInfo, TorrentDisplayState, TorrentMetrics};
    use crate::tui::formatters::format_speed;
    use ratatui::layout::Constraint;

    fn peer_test_app_state() -> AppState {
        let mut app_state = AppState::default();
        let torrent = TorrentDisplayState {
            latest_state: TorrentMetrics {
                data_available: true,
                is_complete: true,
                number_of_pieces_total: 1,
                number_of_pieces_completed: 1,
                peers: vec![
                    PeerInfo {
                        address: "127.0.0.1:6881".to_string(),
                        ..Default::default()
                    },
                    PeerInfo {
                        address: "127.0.0.1:6882".to_string(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let info_hash = b"hash_a".to_vec();
        app_state.torrents.insert(info_hash.clone(), torrent);
        app_state.torrent_list_order = vec![info_hash];
        app_state
    }

    #[test]
    fn done_column_ignores_restored_seeds_still_being_checked_during_startup() {
        let mut app_state = peer_test_app_state();
        let info_hash = app_state.torrent_list_order[0].clone();
        if let Some(torrent) = app_state.torrents.get_mut(&info_hash) {
            torrent.latest_state.number_of_pieces_completed = 0;
        }
        let done_visible = |app_state: &AppState| {
            let (_constraints, visible) = compute_visible_torrent_columns(app_state, 120);
            visible
                .iter()
                .any(|&idx| get_torrent_columns()[idx].id == ColumnId::Status)
        };

        app_state.startup_expected_seeds.insert(info_hash.clone());
        assert!(!done_visible(&app_state), "unchecked seed stays hidden");

        if let Some(torrent) = app_state.torrents.get_mut(&info_hash) {
            torrent.latest_state.data_available = false;
        }
        assert!(done_visible(&app_state), "file errors still show");

        if let Some(torrent) = app_state.torrents.get_mut(&info_hash) {
            torrent.latest_state.data_available = true;
        }
        app_state.startup_expected_seeds.clear();
        assert!(done_visible(&app_state), "downloads still show");
    }

    #[test]
    fn peer_address_column_reserves_more_width_for_ipv6_addresses() {
        let columns = get_peer_columns();
        let address = columns
            .iter()
            .find(|column| column.id == PeerColumnId::Address)
            .expect("address column");

        assert_eq!(address.min_width, 25);
        assert_eq!(address.default_constraint, Constraint::Fill(2));
    }

    #[test]
    fn speed_columns_fit_three_digit_mbps_labels() {
        assert_eq!(format_speed(999_990_000), "999.99 Mbps");
        assert_eq!(format_speed(999_990_000).len(), 11);

        let torrent_columns = get_torrent_columns();
        for column_id in [ColumnId::UpSpeed, ColumnId::DownSpeed] {
            let column = torrent_columns
                .iter()
                .find(|column| column.id == column_id)
                .expect("torrent speed column");
            assert_eq!(column.min_width, 11);
            assert_eq!(column.default_constraint, Constraint::Length(11));
        }

        let peer_columns = get_peer_columns();
        for column_id in [PeerColumnId::UpSpeed, PeerColumnId::DownSpeed] {
            let column = peer_columns
                .iter()
                .find(|column| column.id == column_id)
                .expect("peer speed column");
            assert_eq!(column.min_width, 11);
        }
    }

    #[test]
    fn torrent_speed_columns_keep_minimum_width_for_scaled_rate_label() {
        let mut app_state = peer_test_app_state();
        let info_hash = app_state.torrent_list_order[0].clone();
        let speed_bps = 1_000_000_000_000;
        if let Some(torrent) = app_state.torrents.get_mut(&info_hash) {
            torrent.smoothed_download_speed_bps = speed_bps;
        }

        let (constraints, visible) = compute_visible_torrent_columns(&app_state, 120);
        let all_columns = get_torrent_columns();
        let dl_pos = visible
            .iter()
            .position(|&idx| all_columns[idx].id == ColumnId::DownSpeed)
            .expect("download column should be visible");

        assert_eq!(format_speed(speed_bps), "1.00 Tbps");
        assert_eq!(
            constraints[dl_pos],
            Constraint::Length(
                all_columns[visible[dl_pos]]
                    .min_width
                    .max(format_speed(speed_bps).len() as u16)
            )
        );
    }

    #[test]
    fn peer_columns_drop_low_priority_fields_before_address_on_medium_widths() {
        let mut app_state = peer_test_app_state();
        if let Some(torrent) = app_state.torrents.get_mut(b"hash_a".as_slice()) {
            torrent.latest_state.peers[0].download_speed_bps = 1;
            torrent.latest_state.peers[0].upload_speed_bps = 1;
        }
        let (_constraints, visible) = compute_visible_peer_columns(&app_state, 90);

        assert!(visible.contains(&2), "address column should stay visible");
        assert!(!visible.contains(&6), "action column should drop first");
    }

    #[test]
    fn peer_columns_hide_dl_and_ul_when_selected_torrent_has_no_activity() {
        let app_state = peer_test_app_state();
        let (_constraints, visible) = compute_visible_peer_columns(&app_state, 120);

        assert!(!visible.contains(&3), "upload column should be hidden");
        assert!(!visible.contains(&4), "download column should be hidden");
    }

    #[test]
    fn peer_columns_show_only_active_direction() {
        let mut app_state = peer_test_app_state();
        if let Some(torrent) = app_state.torrents.get_mut(b"hash_a".as_slice()) {
            torrent.latest_state.peers[0].download_speed_bps = 32;
        }
        let (_constraints, visible) = compute_visible_peer_columns(&app_state, 120);

        assert!(!visible.contains(&3), "upload column should stay hidden");
        assert!(visible.contains(&4), "download column should be visible");
    }

    #[test]
    fn torrent_columns_hide_inactive_speed_columns() {
        let app_state = peer_test_app_state();
        let (_constraints, visible) = compute_visible_torrent_columns(&app_state, 120);

        assert_eq!(visible, vec![1], "only name should be visible when idle");
    }

    #[test]
    fn torrent_columns_show_only_active_speed_direction() {
        let mut app_state = peer_test_app_state();
        if let Some(torrent) = app_state.torrents.get_mut(b"hash_a".as_slice()) {
            torrent.smoothed_download_speed_bps = 32;
        }
        let (_constraints, visible) = compute_visible_torrent_columns(&app_state, 120);

        assert!(!visible.contains(&2), "upload column should stay hidden");
        assert!(visible.contains(&3), "download column should be visible");
    }

    #[test]
    fn torrent_columns_show_done_when_torrent_is_incomplete() {
        let mut app_state = peer_test_app_state();
        if let Some(torrent) = app_state.torrents.get_mut(b"hash_a".as_slice()) {
            torrent.latest_state.is_complete = false;
            torrent.latest_state.number_of_pieces_total = 10;
            torrent.latest_state.number_of_pieces_completed = 5;
        }
        let (_constraints, visible) = compute_visible_torrent_columns(&app_state, 120);

        assert!(visible.contains(&0), "done column should be visible");
        assert!(visible.contains(&1), "name column should stay visible");
    }

    #[test]
    fn torrent_columns_show_done_when_torrent_data_is_unavailable() {
        let mut app_state = peer_test_app_state();
        if let Some(torrent) = app_state.torrents.get_mut(b"hash_a".as_slice()) {
            torrent.latest_state.data_available = false;
        }
        let (_constraints, visible) = compute_visible_torrent_columns(&app_state, 120);

        assert!(
            visible.contains(&0),
            "done column should be visible for file probe issues"
        );
        assert!(visible.contains(&1), "name column should stay visible");
    }
}
