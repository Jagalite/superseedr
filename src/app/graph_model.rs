// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application graph model definitions and transitions.

use super::*;

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub enum GraphDisplayMode {
    #[default]
    Auto,
    OneMinute,
    FiveMinutes,
    TenMinutes,
    ThirtyMinutes,
    OneHour,
    ThreeHours,
    TwelveHours,
    TwentyFourHours,
    SevenDays,
    ThirtyDays,
    OneYear,
}

impl GraphDisplayMode {
    pub fn as_seconds(&self) -> usize {
        match self {
            Self::Auto => 60,
            Self::OneMinute => 60,
            Self::FiveMinutes => 300,
            Self::TenMinutes => 600,
            Self::ThirtyMinutes => 1800,
            Self::OneHour => 3600,
            Self::ThreeHours => 3 * 3600,
            Self::TwelveHours => 12 * 3600,
            Self::TwentyFourHours => 86_400,
            Self::SevenDays => 7 * 86_400,
            Self::ThirtyDays => 30 * 86_400,
            Self::OneYear => 365 * 86_400,
        }
    }

    pub fn to_string(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::OneMinute => "1m",
            Self::FiveMinutes => "5m",
            Self::TenMinutes => "10m",
            Self::ThirtyMinutes => "30m",
            Self::OneHour => "1h",
            Self::ThreeHours => "3h",
            Self::TwelveHours => "12h",
            Self::TwentyFourHours => "24h",
            Self::SevenDays => "7d",
            Self::ThirtyDays => "30d",
            Self::OneYear => "1y",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Self::Auto => Self::OneMinute,
            Self::OneMinute => Self::FiveMinutes,
            Self::FiveMinutes => Self::TenMinutes,
            Self::TenMinutes => Self::ThirtyMinutes,
            Self::ThirtyMinutes => Self::OneHour,
            Self::OneHour => Self::ThreeHours,
            Self::ThreeHours => Self::TwelveHours,
            Self::TwelveHours => Self::TwentyFourHours,
            Self::TwentyFourHours => Self::SevenDays,
            Self::SevenDays => Self::ThirtyDays,
            Self::ThirtyDays => Self::OneYear,
            Self::OneYear => Self::OneYear,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            Self::Auto => Self::Auto,
            Self::OneMinute => Self::Auto,
            Self::FiveMinutes => Self::OneMinute,
            Self::TenMinutes => Self::FiveMinutes,
            Self::ThirtyMinutes => Self::TenMinutes,
            Self::OneHour => Self::ThirtyMinutes,
            Self::ThreeHours => Self::OneHour,
            Self::TwelveHours => Self::ThreeHours,
            Self::TwentyFourHours => Self::TwelveHours,
            Self::SevenDays => Self::TwentyFourHours,
            Self::ThirtyDays => Self::SevenDays,
            Self::OneYear => Self::ThirtyDays,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoGraphWindowState {
    pub effective_mode: GraphDisplayMode,
    pub last_evaluation_unix: u64,
    pub last_change_unix: u64,
}

impl Default for AutoGraphWindowState {
    fn default() -> Self {
        Self {
            effective_mode: GraphDisplayMode::OneMinute,
            last_evaluation_unix: 0,
            last_change_unix: 0,
        }
    }
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub enum ChartPanelView {
    #[default]
    Network,
    Cpu,
    Ram,
    Disk,
    Tuning,
    TorrentOverlay,
    MultiTorrentOverlay,
}

impl ChartPanelView {
    pub fn to_string(self) -> &'static str {
        match self {
            Self::Network => "NET",
            Self::Cpu => "CPU",
            Self::Ram => "RAM",
            Self::Disk => "DISK",
            Self::Tuning => "TUNE",
            Self::TorrentOverlay => "TOR",
            Self::MultiTorrentOverlay => "MULTI",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Network => Self::Cpu,
            Self::Cpu => Self::Ram,
            Self::Ram => Self::Disk,
            Self::Disk => Self::Tuning,
            Self::Tuning => Self::TorrentOverlay,
            Self::TorrentOverlay => Self::MultiTorrentOverlay,
            Self::MultiTorrentOverlay => Self::MultiTorrentOverlay,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Network => Self::Network,
            Self::Cpu => Self::Network,
            Self::Ram => Self::Cpu,
            Self::Disk => Self::Ram,
            Self::Tuning => Self::Disk,
            Self::TorrentOverlay => Self::Tuning,
            Self::MultiTorrentOverlay => Self::TorrentOverlay,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectedHeader {
    Torrent(ColumnId),
    Peer(PeerColumnId),
}
impl Default for SelectedHeader {
    fn default() -> Self {
        SelectedHeader::Torrent(ColumnId::Name)
    }
}

pub(super) fn torrent_sort_header(column: TorrentSortColumn) -> ColumnId {
    match column {
        TorrentSortColumn::Name => ColumnId::Name,
        TorrentSortColumn::Down => ColumnId::DownSpeed,
        TorrentSortColumn::Up => ColumnId::UpSpeed,
        TorrentSortColumn::Progress => ColumnId::Status,
    }
}
