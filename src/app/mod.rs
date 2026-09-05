// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub(crate) mod reducer;
pub use reducer::AppEffect;
pub(crate) use reducer::{
    finalize_manager_metrics_batch, reduce_app_action, remove_torrent_from_state, AppAction,
};
pub(crate) mod torrent_manager_protocol;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use std::collections::VecDeque;

use magnet_url::Magnet;

use fuzzy_matcher::FuzzyMatcher;

use rand::RngExt;

use self::torrent_manager_protocol::DiskIoOperation;

use crate::config::{
    FeedSyncError, PeerSortColumn, RssFilterMode, RssHistoryEntry, Settings, SortDirection,
    TorrentMetadataEntry, TorrentMetadataFileEntry, TorrentSettings, TorrentSortColumn,
    UiLayoutMode,
};
use crate::dht_model::{DhtStatus, DhtWaveTelemetry};
use crate::peer_manager::{PeerManagerView, PeerPolicy};
use crate::persistence::activity_history::{
    ActivityHistoryPersistedState, ActivityHistoryRollupState,
};
use crate::persistence::event_journal::{
    append_event_journal_entry, ControlOrigin, EventCategory, EventDetails, EventJournalEntry,
    EventJournalState, EventScope, EventType, IngestKind, IngestOrigin,
};
use crate::persistence::network_history::{
    NetworkHistoryPersistedState, NetworkHistoryRollupState,
};
use crate::persistence::rss::RssPersistedState;

use crate::token_bucket::{rate_limit_bps_to_bucket_bytes_per_sec, TokenBucket};

use crate::tui::events::PasteBurst;
use crate::tui::layout::common::{ColumnId, PeerColumnId};
use crate::tui::layout::normal::{
    calculate_layout, LayoutContext, DEFAULT_SIDEBAR_PERCENT, PEER_STREAM_MIN_HEIGHT,
    PEER_STREAM_MIN_WIDTH,
};
use crate::tui::render::compute_effects_activity_speed_multiplier;
use crate::tui::render::draw;
use crate::tui::screens::browser::{
    build_filesystem_filter, calculate_list_height, focused_pane, preview_content_for_selection,
};
use crate::tui::tree;
use crate::tui::tree::RawNode;
use crate::tui::tree::TreeProjection;
use crate::tui::tree::TreeViewState;

#[cfg(test)]
pub use crate::tui::state::ConfigNetworkInterfaceInventory;
pub(crate) use crate::tui::state::AWAITING_MAGNET_METADATA_LABEL;
pub use crate::tui::state::{
    AppMode, BrowserPane, ConfigEditState, ConfigItem, ConfigPane, ConfigUiState,
    DownloadSelectionTarget, FileBrowserMode, FilePriority, RssPreviewItem, TorrentControlState,
    TorrentPreviewPayload,
};

use crate::resource::ResourceType;
use crate::theme::Theme;

use self::torrent_manager_protocol::data_availability_from_file_probe_result;
use self::torrent_manager_protocol::FileActivityUpdate;
use self::torrent_manager_protocol::ManagerCommand;
use self::torrent_manager_protocol::ManagerEvent;
use self::torrent_manager_protocol::TorrentFileProbeStatus;
use crate::integrations::control::{ControlFilePriorityOverride, ControlRequest};
use crate::networking::PeerTransportKind;
use crate::networking::{NetworkInterfaceInfo, NetworkScopeId};
use crate::torrent_identity::info_hash_from_torrent_source;

#[cfg(test)]
thread_local! {
    static TEST_PERSISTENCE_WRITER_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn test_persistence_writer_enabled() -> bool {
    TEST_PERSISTENCE_WRITER_ENABLED.get()
}

#[cfg(test)]
fn set_test_persistence_writer_enabled(enabled: bool) {
    TEST_PERSISTENCE_WRITER_ENABLED.set(enabled);
}

use std::collections::{HashMap, HashSet};
use tokio::sync::watch;

use std::sync::Arc;
use web_time::{Instant, SystemTime, UNIX_EPOCH};

use sha1::Digest;
use sha2::Sha256;

use serde::{Deserialize, Serialize};
use std::time::Duration;

use ratatui::prelude::Rect;

use tracing::{event as tracing_event, Level};

mod limits;
pub use limits::*;
mod version_model;
use version_model::*;
mod file_preview;
pub use file_preview::*;
mod display_rate;
pub use display_rate::*;
mod resource_model;
pub use resource_model::*;
mod graph_model;
pub use graph_model::*;
mod commands;
pub use commands::*;
mod rss_model;
pub use rss_model::*;
mod torrent_model;
pub use torrent_model::*;
mod visualization_model;
pub use visualization_model::*;
mod browser_model;
pub use browser_model::*;
mod panels_model;
pub use panels_model::*;
mod model;
pub use model::*;
mod throttle;
use throttle::*;
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::App;

use crate::tui::animation::{advance_dht_wave_state, dht_wave_targets};
mod torrent_helpers;
pub use torrent_helpers::*;
mod presentation;
pub(crate) use presentation::*;

mod checkpoint;
pub use checkpoint::*;

mod settings_policy;
pub use settings_policy::*;

mod lifecycle;
pub use lifecycle::*;

mod manager_lifetime;
#[cfg(target_arch = "wasm32")]
pub(crate) use manager_lifetime::ManagerSource;
pub(crate) use manager_lifetime::{ManagerLifetime, ManagerObservation};

mod bootstrap;
pub use bootstrap::*;

mod ingest_policy;
pub(crate) use ingest_policy::*;
