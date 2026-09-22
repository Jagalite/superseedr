// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppMode, AppState, HelpSection, SearchMode};
use crate::config::Settings;
use crate::terminal_event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crate::theme::ThemeContext;
use crate::tui::action_style::{help_key_style, ActionTone};
use crate::tui::formatters::{centered_rect, sanitize_text, truncate_with_ellipsis};
use crate::tui::render::calculate_player_stats;
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::input_panel::draw_prompt_panel;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::{prelude::*, widgets::*};

const HELP_SECTIONS: [HelpSection; 7] = [
    HelpSection::General,
    HelpSection::Torrents,
    HelpSection::Graphs,
    HelpSection::Legends,
    HelpSection::Screens,
    HelpSection::Paths,
    HelpSection::Build,
];

impl HelpSection {
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Torrents => "Torrents",
            Self::Graphs => "Graphs",
            Self::Legends => "Legends",
            Self::Screens => "Screens",
            Self::Paths => "Paths",
            Self::Build => "Build",
        }
    }

    fn next(self) -> Self {
        let idx = HELP_SECTIONS
            .iter()
            .position(|section| *section == self)
            .unwrap_or(0);
        HELP_SECTIONS[(idx + 1) % HELP_SECTIONS.len()]
    }

    fn prev(self) -> Self {
        let idx = HELP_SECTIONS
            .iter()
            .position(|section| *section == self)
            .unwrap_or(0);
        HELP_SECTIONS[(idx + HELP_SECTIONS.len() - 1) % HELP_SECTIONS.len()]
    }

    fn description(self) -> &'static str {
        match self {
            Self::General => "Everyday shortcuts. Use these from the main dashboard.",
            Self::Torrents => "Add transfers and control the selected torrent.",
            Self::Graphs => "Adjust charts, visualizations, and appearance.",
            Self::Legends => "Understand the colors and metrics on the dashboard.",
            Self::Screens => "Shortcuts for management, settings, and other screens.",
            Self::Paths => "Configuration, logs, and watched folders on this host.",
            Self::Build => "Discovery features available in this executable.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HelpItem {
    section: HelpSection,
    subsection: String,
    key: String,
    action: String,
    key_style: HelpKeyStyle,
    action_tone: ActionTone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelpKeyStyle {
    Plain,
    PeerDownloadOpportunity,
    PeerDownloadBlocked,
    PeerUploadOpportunity,
    PeerUploadRestricted,
    DiskRead,
    DiskWrite,
}

impl HelpItem {
    fn new(
        section: HelpSection,
        subsection: impl Into<String>,
        key: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            section,
            subsection: subsection.into(),
            key: key.into(),
            action: action.into(),
            key_style: HelpKeyStyle::Plain,
            action_tone: ActionTone::Info,
        }
    }

    fn with_action_tone(mut self, action_tone: ActionTone) -> Self {
        self.action_tone = action_tone;
        self
    }

    fn with_key_style(mut self, key_style: HelpKeyStyle) -> Self {
        self.key_style = key_style;
        self
    }

    fn matches_query(&self, query: &str, mode: SearchMode, matcher: &SkimMatcherV2) -> bool {
        if query.is_empty() {
            return true;
        }

        let haystack = format!(
            "{} {} {} {}",
            self.section.label(),
            self.subsection,
            self.key,
            self.action
        );
        match mode {
            SearchMode::Fuzzy => matcher
                .fuzzy_match(&haystack.to_lowercase(), &query.to_lowercase())
                .is_some(),
            SearchMode::Regex => regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .ok()
                .is_some_and(|re| re.is_match(&haystack)),
        }
    }
}

fn display_path_or_disabled(path: Option<std::path::PathBuf>) -> String {
    path.map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "Disabled".to_string())
}

fn build_help_footer_entries(
    settings: &Settings,
    app_state: &AppState,
) -> Vec<(&'static str, String)> {
    let log_path_str = app_state
        .runtime_paths
        .log_files_path
        .clone()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown location".to_string());

    let mut entries = if app_state.runtime_paths.shared_mode {
        vec![
            (
                "Settings",
                app_state
                    .runtime_paths
                    .settings_path
                    .clone()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unknown location".to_string()),
            ),
            ("Log Files", log_path_str),
            (
                "Host Watch",
                display_path_or_disabled(app_state.runtime_paths.resolved_watch_path(settings)),
            ),
            (
                "Shared Inbox",
                app_state
                    .runtime_paths
                    .shared_inbox_path
                    .clone()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unknown location".to_string()),
            ),
        ]
    } else {
        let settings_path_str = app_state
            .runtime_paths
            .settings_path
            .clone()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "Unknown location".to_string());
        let watch_path_str = app_state
            .runtime_paths
            .resolved_watch_path(settings)
            .map(|watch_path| watch_path.to_string_lossy().to_string())
            .unwrap_or_else(|| "Disabled".to_string());
        vec![
            ("Settings", settings_path_str),
            ("Log Files", log_path_str),
            ("Watch Dir", watch_path_str),
        ]
    };

    if let Some(cluster_role) = app_state.cluster_role_label.as_ref() {
        entries.push(("Cluster", cluster_role.clone()));
    }
    if let Some(runtime_label) = app_state.cluster_runtime_label.as_ref() {
        entries.push(("Runtime", runtime_label.clone()));
    }

    entries
}

fn build_help_items(settings: &Settings, app_state: &AppState) -> Vec<HelpItem> {
    let mut items = Vec::new();
    macro_rules! item {
        ($section:expr, $subsection:expr, $key:expr, $action:expr $(,)?) => {
            items.push(HelpItem::new($section, $subsection, $key, $action));
        };
    }
    macro_rules! action_item {
        ($section:expr, $subsection:expr, $key:expr, $action:expr, $action_tone:expr $(,)?) => {
            items.push(
                HelpItem::new($section, $subsection, $key, $action).with_action_tone($action_tone),
            );
        };
    }
    macro_rules! styled_item {
        ($section:expr, $subsection:expr, $key:expr, $action:expr, $key_style:expr $(,)?) => {
            items.push(
                HelpItem::new($section, $subsection, $key, $action).with_key_style($key_style),
            );
        };
    }

    action_item!(
        HelpSection::General,
        "Dashboard shortcuts",
        "C",
        "Open Config",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Dashboard shortcuts",
        "R",
        "Open RSS",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Dashboard shortcuts",
        "J",
        "Open the event journal",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Dashboard shortcuts",
        "M",
        "Open torrent management",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Dashboard shortcuts",
        "P",
        "Open peer management",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Dashboard shortcuts",
        "Z",
        "Toggle Zen / Power Saving mode",
        ActionTone::Toggle
    );

    action_item!(
        HelpSection::General,
        "Dashboard shortcuts",
        "Q (shift+q)",
        "Quit the application",
        ActionTone::Destructive
    );
    action_item!(
        HelpSection::General,
        "Help Navigation",
        "Tab / Shift+Tab / h / l",
        "Move between help sections",
        ActionTone::Mode
    );
    action_item!(
        HelpSection::General,
        "Help Navigation",
        "Up / Down / k / j",
        "Scroll the visible help rows",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::General,
        "Help Navigation",
        "PgUp / PgDn / Home / End",
        "Scroll by a page or jump to the start or end; Home and End apply while browsing",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::General,
        "Help Navigation",
        "Esc / m / q",
        "Close help and return to the dashboard",
        ActionTone::Cancel
    );
    action_item!(
        HelpSection::General,
        "Search",
        "/",
        "Open search in the current searchable view; in Help, search all help contents",
        ActionTone::Search
    );
    action_item!(
        HelpSection::General,
        "Search",
        "Tab",
        "Toggle fuzzy or regex matching where the active search supports modes",
        ActionTone::Mode
    );
    action_item!(
        HelpSection::General,
        "Search",
        "Enter / Esc",
        "Keep the current results or clear the search and return to this section",
        ActionTone::Confirm
    );
    action_item!(
        HelpSection::General,
        "Search",
        "Ctrl+u",
        "Clear the current query without leaving the search panel",
        ActionTone::Clear
    );
    action_item!(
        HelpSection::Torrents,
        "Adding Torrents",
        "a",
        "Choose a .torrent file",
        ActionTone::Add
    );
    action_item!(
        HelpSection::Torrents,
        "Adding Torrents",
        "Paste",
        "Paste a magnet link or torrent file path",
        ActionTone::Paste
    );
    action_item!(
        HelpSection::Torrents,
        "Adding Torrents",
        "CLI",
        "Run superseedr add from another terminal",
        ActionTone::Add
    );
    action_item!(
        HelpSection::Torrents,
        "Torrent Actions",
        "p",
        "Pause or resume the selected torrent",
        ActionTone::Queue
    );
    action_item!(
        HelpSection::Torrents,
        "Torrent Actions",
        "d / D",
        "Remove the selected torrent; D also removes files after confirmation",
        ActionTone::Destructive
    );
    action_item!(
        HelpSection::Torrents,
        "Table Control",
        "h / l / Left / Right",
        "Move between table header columns",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Torrents,
        "Table Control",
        "s",
        "Sort by the focused table column",
        ActionTone::Sort
    );
    action_item!(
        HelpSection::Torrents,
        "Table Control",
        "S",
        "Clear manual sorting and resume automatic sorting",
        ActionTone::Clear
    );

    action_item!(
        HelpSection::Graphs,
        "Chart Panels",
        "t / T",
        "Switch graph time scale forward or backward",
        ActionTone::Rate
    );
    action_item!(
        HelpSection::Graphs,
        "Chart Panels",
        "g / G",
        "Switch chart panel view forward or backward",
        ActionTone::Mode
    );
    action_item!(
        HelpSection::Graphs,
        "Chart Panels",
        "[ / ]",
        "Change UI refresh rate",
        ActionTone::Rate
    );
    action_item!(
        HelpSection::Graphs,
        "Chart Panels",
        "< / >",
        "Cycle UI theme",
        ActionTone::Theme
    );
    action_item!(
        HelpSection::Graphs,
        "Visualization Focus",
        "v",
        "Enter or leave visualization focus mode",
        ActionTone::Mode
    );
    action_item!(
        HelpSection::Graphs,
        "Visualization Focus",
        "Tab / Shift+Tab",
        "Move focus between panels with multiple visualization renderers",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Graphs,
        "Visualization Focus",
        "Left / Right",
        "Cycle the focused panel visualization",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Graphs,
        "Visualization Focus",
        "u",
        "Restore the focused panel's default visualization",
        ActionTone::Clear
    );
    action_item!(
        HelpSection::Graphs,
        "Layout",
        "x",
        "Anonymize torrent names",
        ActionTone::Toggle
    );

    item!(
        HelpSection::Legends,
        "DHT Wave",
        "DHT panel",
        "Power multiplier, active queries, and unique peers found in the last 10s"
    );
    styled_item!(
        HelpSection::Legends,
        "Peer Flags",
        "Blue",
        "You are interested (DL potential)",
        HelpKeyStyle::PeerDownloadOpportunity
    );
    styled_item!(
        HelpSection::Legends,
        "Peer Flags",
        "Red",
        "Peer is choking you (DL block)",
        HelpKeyStyle::PeerDownloadBlocked
    );
    styled_item!(
        HelpSection::Legends,
        "Peer Flags",
        "Teal",
        "Peer is interested (UL opportunity)",
        HelpKeyStyle::PeerUploadOpportunity
    );
    styled_item!(
        HelpSection::Legends,
        "Peer Flags",
        "Peach",
        "You are choking peer (UL restriction)",
        HelpKeyStyle::PeerUploadRestricted
    );
    styled_item!(
        HelpSection::Legends,
        "Disk Metrics",
        "Read",
        "Data read from disk",
        HelpKeyStyle::DiskRead
    );
    styled_item!(
        HelpSection::Legends,
        "Disk Metrics",
        "Write",
        "Data written to disk",
        HelpKeyStyle::DiskWrite
    );
    item!(
        HelpSection::Legends,
        "Disk Metrics",
        "Seek",
        "Avg. distance between I/O ops; lower is better"
    );
    item!(
        HelpSection::Legends,
        "Disk Metrics",
        "Latency",
        "Time to complete one I/O op; lower is better"
    );
    item!(
        HelpSection::Legends,
        "Disk Metrics",
        "IOPS",
        "I/O Operations Per Second; total workload"
    );
    item!(
        HelpSection::Legends,
        "Self Tuning",
        "Self-Tune",
        "Tuning state and countdown to the next adjustment cycle"
    );
    item!(
        HelpSection::Legends,
        "Self Tuning",
        "Resource rows",
        "Current limits for peers, reads, writes, and reserve capacity"
    );

    action_item!(
        HelpSection::Screens,
        "RSS",
        "Tab / h",
        "Move RSS focus or swap Explorer with History",
        ActionTone::Mode
    );
    action_item!(
        HelpSection::Screens,
        "RSS",
        "s",
        "Sync feeds now",
        ActionTone::Rate
    );
    action_item!(
        HelpSection::Screens,
        "RSS",
        "a / D / Space",
        "Add, delete, or toggle the focused RSS item",
        ActionTone::Toggle
    );
    action_item!(
        HelpSection::Screens,
        "RSS",
        "Enter",
        "Confirm add or search input",
        ActionTone::Confirm
    );
    action_item!(
        HelpSection::Screens,
        "RSS",
        "Y",
        "Download the selected Explorer item if it has not been downloaded",
        ActionTone::Confirm
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "Up / Down / k / j",
        "Move selection through visible torrents",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "Page Up / Page Down / Home / End",
        "Move by a page or jump to the first or last visible torrent",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "h / l / Left / Right",
        "Move between table columns",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "s",
        "Sort by the focused column",
        ActionTone::Sort
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "x",
        "Toggle anonymized torrent names",
        ActionTone::Toggle
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "Space / A",
        "Select the current torrent or all visible torrents",
        ActionTone::Select
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "f",
        "Open files for the highlighted torrent",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "p",
        "Queue pause or resume for the current target set",
        ActionTone::Queue
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "d / D",
        "Queue remove, or purge files with D, for the current target set",
        ActionTone::Destructive
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "Y / Enter",
        "Review queued commands with Y; submit from review with Enter",
        ActionTone::Confirm
    );
    action_item!(
        HelpSection::Screens,
        "Torrent Management",
        "u",
        "Clear the current selection and its draft commands",
        ActionTone::Clear
    );
    action_item!(
        HelpSection::Screens,
        "Peer Management",
        "Up / Down / k / j",
        "Move selection through tracked and restricted peers",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "Peer Management",
        "Tab / Shift+Tab",
        "Cycle All, Active, Recent, and Restricted filters",
        ActionTone::Mode
    );
    action_item!(
        HelpSection::Screens,
        "Peer Management",
        "h / l / Left / Right",
        "Move between table columns; s sorts the focused column",
        ActionTone::Sort
    );
    action_item!(
        HelpSection::Screens,
        "Peer Management",
        "/",
        "Search peer addresses, endpoints, torrents, states, and restriction reasons",
        ActionTone::Search
    );
    action_item!(
        HelpSection::Screens,
        "Peer Management",
        "Enter",
        "Open or close full peer details on compact layouts",
        ActionTone::Open
    );
    action_item!(
        HelpSection::Screens,
        "Peer Management",
        "x",
        "Toggle privacy masking for peer and torrent identities",
        ActionTone::Toggle
    );
    action_item!(
        HelpSection::Screens,
        "Journal",
        "Tab / Shift+Tab",
        "Cycle event journal filters",
        ActionTone::Mode
    );
    action_item!(
        HelpSection::Screens,
        "Journal",
        "Page Up / Page Down",
        "Move through journal activities by one visible page",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "Journal",
        "/",
        "Search the current journal filter; use Tab to toggle fuzzy or regex matching",
        ActionTone::Search
    );
    action_item!(
        HelpSection::Screens,
        "Journal",
        "Y",
        "Replay selected archived torrent, magnet, or path source",
        ActionTone::Replay
    );
    action_item!(
        HelpSection::Screens,
        "Config",
        "Space",
        "Shift or open the selected control",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "Config",
        "h / l",
        "Move backward or forward through choices",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "Config",
        "r",
        "Open confirmation before resetting the focused setting",
        ActionTone::Clear
    );
    action_item!(
        HelpSection::Screens,
        "Config",
        "Esc / q",
        "Close Config immediately",
        ActionTone::Cancel
    );
    action_item!(
        HelpSection::Screens,
        "Config · Editing",
        "Enter / Esc",
        "Apply the edited value or cancel the current edit",
        ActionTone::Edit
    );
    action_item!(
        HelpSection::Screens,
        "Config · Path Picker",
        "Y",
        "Apply the confirmed path and return to Config",
        ActionTone::Confirm
    );
    action_item!(
        HelpSection::Screens,
        "Config · Compact Details",
        "Esc",
        "Return to the settings list",
        ActionTone::Navigate
    );
    action_item!(
        HelpSection::Screens,
        "File Browser",
        "Y",
        "Confirm the current file-browser action",
        ActionTone::Confirm
    );
    action_item!(
        HelpSection::Screens,
        "Delete Confirm",
        "Y",
        "Confirm delete",
        ActionTone::Destructive
    );

    let (lvl, progress) = calculate_player_stats(app_state);
    item!(
        HelpSection::Legends,
        "Session Level",
        "Progress",
        format!(
            "Level {lvl} with {:.0}% progress to next level",
            progress * 100.0
        )
    );

    for (label, value) in build_help_footer_entries(settings, app_state) {
        item!(
            HelpSection::Paths,
            "Runtime Paths",
            label,
            if value.is_empty() {
                "Unknown location".to_string()
            } else {
                value
            },
        );
    }

    if app_state.runtime_paths.shared_mode {
        item!(
            HelpSection::Paths,
            "Shared Mode",
            "Shared mode",
            "Settings and inbox paths come from the shared configuration root",
        );
    }

    item!(
        HelpSection::Build,
        "Feature Set",
        "DHT",
        if cfg!(target_arch = "wasm32") {
            "Simulated only; no discovery service"
        } else if cfg!(feature = "dht") {
            "Included in this build"
        } else {
            "Not included in this private build"
        }
    );
    item!(
        HelpSection::Build,
        "Feature Set",
        "PEX",
        if cfg!(target_arch = "wasm32") {
            "Simulated only; no discovery service"
        } else if cfg!(feature = "pex") {
            "Included in this build"
        } else {
            "Not included in this private build"
        }
    );
    item!(
        HelpSection::Build,
        "Feature Set",
        "Private mode",
        if cfg!(target_arch = "wasm32") {
            "Browser simulation; no tracker or peer networking"
        } else if cfg!(all(feature = "dht", feature = "pex")) {
            "Normal public-tracker feature set"
        } else {
            "Private-tracker feature set with public discovery disabled"
        }
    );

    items
}

fn help_items_for_view(settings: &Settings, app_state: &AppState) -> Vec<HelpItem> {
    filter_help_items_for_view(build_help_items(settings, app_state), app_state)
}

fn filter_help_items_for_view(all_items: Vec<HelpItem>, app_state: &AppState) -> Vec<HelpItem> {
    let query = app_state.ui.help.search_query.trim();
    let search_view = app_state.ui.help.is_searching || !query.is_empty();

    if search_view {
        if query.is_empty() {
            return all_items;
        }
        let matcher = SkimMatcherV2::default();
        return all_items
            .into_iter()
            .filter(|item| item.matches_query(query, app_state.ui.help.search_mode, &matcher))
            .collect();
    }

    all_items
        .into_iter()
        .filter(|item| item.section == app_state.ui.help.active_section)
        .collect()
}

enum HelpDisplayRow<'a> {
    Spacer,
    Heading { section: HelpSection, title: String },
    Item(&'a HelpItem),
}

fn help_display_rows(items: &[HelpItem], search_view: bool) -> Vec<HelpDisplayRow<'_>> {
    let mut rows = Vec::new();
    let mut index = 0;

    while index < items.len() {
        let item = &items[index];
        let heading = if search_view {
            format!("{} / {}", item.section.label(), item.subsection)
        } else {
            item.subsection.clone()
        };

        let item_count = items[index..]
            .iter()
            .take_while(|candidate| {
                if search_view {
                    candidate.section == item.section && candidate.subsection == item.subsection
                } else {
                    candidate.subsection == item.subsection
                }
            })
            .count();

        if !rows.is_empty() {
            rows.push(HelpDisplayRow::Spacer);
        }
        rows.push(HelpDisplayRow::Heading {
            section: item.section,
            title: heading,
        });
        rows.extend(
            items[index..index + item_count]
                .iter()
                .map(HelpDisplayRow::Item),
        );
        index += item_count;
    }

    rows
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelpAction {
    Close,
    SectionNext,
    SectionPrev,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    First,
    Last,
    SearchStart,
    SearchInsert(char),
    SearchBackspace,
    SearchClear,
    SearchCommit,
    SearchCancel,
    ToggleSearchMode,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HelpEffect {
    ToNormal,
}

#[derive(Default)]
pub struct HelpReduceResult {
    pub consumed: bool,
    pub effects: Vec<HelpEffect>,
}

fn map_key_to_help_action(key: KeyEvent, search_panel_active: bool) -> Option<HelpAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::PageUp => return Some(HelpAction::PageUp),
        KeyCode::PageDown => return Some(HelpAction::PageDown),
        KeyCode::Home if !search_panel_active => return Some(HelpAction::First),
        KeyCode::End if !search_panel_active => return Some(HelpAction::Last),
        _ => {}
    }

    if search_panel_active && matches!(key.code, KeyCode::Tab) {
        return Some(HelpAction::ToggleSearchMode);
    }

    if search_panel_active {
        return match key.code {
            KeyCode::Esc => Some(HelpAction::SearchCancel),
            KeyCode::Enter => Some(HelpAction::SearchCommit),
            KeyCode::Backspace => Some(HelpAction::SearchBackspace),
            KeyCode::Char('u') if has_ctrl => Some(HelpAction::SearchClear),
            KeyCode::Up => Some(HelpAction::ScrollUp),
            KeyCode::Down => Some(HelpAction::ScrollDown),
            KeyCode::Char(c) if !has_ctrl && !has_alt => Some(HelpAction::SearchInsert(c)),
            _ => None,
        };
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('m') | KeyCode::Char('q') => Some(HelpAction::Close),
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => Some(HelpAction::SectionNext),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => Some(HelpAction::SectionPrev),
        KeyCode::Up | KeyCode::Char('k') => Some(HelpAction::ScrollUp),
        KeyCode::Down | KeyCode::Char('j') => Some(HelpAction::ScrollDown),
        KeyCode::Char('/') => Some(HelpAction::SearchStart),
        _ => None,
    }
}

pub fn reduce_help_action(
    app_state: &mut AppState,
    settings: &Settings,
    action: HelpAction,
) -> HelpReduceResult {
    match action {
        HelpAction::Close => HelpReduceResult {
            consumed: true,
            effects: vec![HelpEffect::ToNormal],
        },
        HelpAction::SectionNext => {
            app_state.ui.help.active_section = app_state.ui.help.active_section.next();
            app_state.ui.help.scroll_offset = 0;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::SectionPrev => {
            app_state.ui.help.active_section = app_state.ui.help.active_section.prev();
            app_state.ui.help.scroll_offset = 0;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::ScrollUp
        | HelpAction::ScrollDown
        | HelpAction::PageUp
        | HelpAction::PageDown
        | HelpAction::First
        | HelpAction::Last => {
            let max_scroll = max_help_scroll_offset(settings, app_state);
            let current = app_state.ui.help.scroll_offset.min(max_scroll);
            let page = help_visible_count_for_state(app_state)
                .saturating_sub(1)
                .max(1);
            app_state.ui.help.scroll_offset = match action {
                HelpAction::ScrollUp => current.saturating_sub(1),
                HelpAction::ScrollDown => current.saturating_add(1).min(max_scroll),
                HelpAction::PageUp => current.saturating_sub(page),
                HelpAction::PageDown => current.saturating_add(page).min(max_scroll),
                HelpAction::First => 0,
                HelpAction::Last => max_scroll,
                _ => unreachable!(),
            };
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::SearchStart => {
            app_state.ui.help.is_searching = true;
            app_state.ui.help.search_mode = SearchMode::Regex;
            app_state.ui.help.scroll_offset = 0;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::SearchInsert(c) => {
            app_state.ui.help.search_query.push(c);
            app_state.ui.help.scroll_offset = 0;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::SearchBackspace => {
            app_state.ui.help.search_query.pop();
            app_state.ui.help.scroll_offset = 0;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::SearchClear => {
            app_state.ui.help.is_searching = true;
            app_state.ui.help.search_query.clear();
            app_state.ui.help.scroll_offset = 0;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::SearchCommit => {
            app_state.ui.help.is_searching = false;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::SearchCancel => {
            app_state.ui.help.is_searching = false;
            app_state.ui.help.search_query.clear();
            app_state.ui.help.scroll_offset = 0;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::ToggleSearchMode => {
            app_state.ui.help.search_mode = match app_state.ui.help.search_mode {
                SearchMode::Fuzzy => SearchMode::Regex,
                SearchMode::Regex => SearchMode::Fuzzy,
            };
            app_state.ui.help.scroll_offset = 0;
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
    }
}

pub fn execute_help_effects(app_state: &mut AppState, effects: Vec<HelpEffect>) {
    for effect in effects {
        match effect {
            HelpEffect::ToNormal => app_state.mode = AppMode::Normal,
        }
    }
}

#[cfg(test)]
fn handle_event(event: CrosstermEvent, app_state: &mut AppState) {
    handle_event_with_settings(event, app_state, &Settings::default());
}

pub fn handle_event_with_settings(
    event: CrosstermEvent,
    app_state: &mut AppState,
    settings: &Settings,
) {
    if !matches!(app_state.mode, AppMode::Help) {
        return;
    }

    if let CrosstermEvent::Key(key) = event {
        let search_panel_active =
            app_state.ui.help.is_searching || !app_state.ui.help.search_query.is_empty();
        if let Some(action) = map_key_to_help_action(key, search_panel_active) {
            let reduced = reduce_help_action(app_state, settings, action);
            if reduced.consumed {
                app_state.ui.needs_redraw = true;
                execute_help_effects(app_state, reduced.effects);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelpDensity {
    Compact,
    Standard,
    Spacious,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HelpLayout {
    popup: Rect,
    search: Option<Rect>,
    tabs: Rect,
    panel: Rect,
    hero: Rect,
    warning: Option<Rect>,
    table: Rect,
    controls: Rect,
    density: HelpDensity,
}

fn help_density(panel: Rect) -> HelpDensity {
    if panel.width < 68 || panel.height < 18 {
        HelpDensity::Compact
    } else if panel.width >= 108 && panel.height >= 30 {
        HelpDensity::Spacious
    } else {
        HelpDensity::Standard
    }
}

fn help_panel_inner(panel: Rect, density: HelpDensity) -> Rect {
    let vertical_padding = u16::from(matches!(density, HelpDensity::Spacious));
    Block::default()
        .borders(Borders::ALL)
        .padding(Padding::new(1, 1, vertical_padding, vertical_padding))
        .inner(panel)
}

fn warning_height(warning_text: &str, width: u16) -> u16 {
    let usable_width = width.saturating_sub(2).max(1) as usize;
    let display_width = warning_text.chars().count();
    let lines = display_width.div_ceil(usable_width) as u16;
    lines.saturating_add(1).clamp(2, 3)
}

fn calculate_help_layout(
    frame_area: Rect,
    search_panel_active: bool,
    warning_text: Option<&str>,
) -> HelpLayout {
    let popup = centered_rect(92, 96, frame_area);
    let (search, help_area) = if search_panel_active && popup.height >= 6 {
        let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(popup);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, popup)
    };

    let chrome = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(help_area);
    let panel = chrome[1];
    let density = help_density(panel);
    let inner = help_panel_inner(panel, density);
    let mut hero_height = match density {
        HelpDensity::Compact => 1,
        HelpDensity::Standard | HelpDensity::Spacious => 3,
    };
    let warning_row_height = warning_text.map(|text| warning_height(text, inner.width));
    let mut table_min_height = 1;
    if matches!(density, HelpDensity::Compact) {
        if let Some(warning_row_height) = warning_row_height {
            hero_height = hero_height.min(
                inner
                    .height
                    .saturating_sub(warning_row_height)
                    .saturating_sub(1),
            );
            table_min_height =
                u16::from(inner.height > warning_row_height.saturating_add(hero_height));
        }
    }
    let mut content_constraints = vec![Constraint::Length(hero_height)];
    if let Some(warning_row_height) = warning_row_height {
        content_constraints.push(Constraint::Length(warning_row_height));
    }
    content_constraints.push(Constraint::Min(table_min_height));
    let content_rows = Layout::vertical(content_constraints).split(inner);
    let table_index = 1 + usize::from(warning_text.is_some());

    HelpLayout {
        popup,
        search,
        tabs: chrome[0],
        panel,
        hero: content_rows[0],
        warning: warning_text.map(|_| content_rows[1]),
        table: content_rows[table_index],
        controls: chrome[2],
        density,
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.ui;
    let settings = screen.settings;
    let ctx = screen.theme;
    let items = help_items_for_view(settings, app_state);
    let search_panel_active =
        app_state.ui.help.is_searching || !app_state.ui.help.search_query.is_empty();
    let layout = calculate_help_layout(
        f.area(),
        search_panel_active,
        app_state.system_warning.as_deref(),
    );

    crate::tui::render::clear(f, layout.popup);

    if let Some(search_area) = layout.search {
        draw_help_search_panel(f, search_area, app_state, items.len(), ctx);
    }
    draw_help_tabs(f, layout.tabs, app_state, ctx);

    let active_color = help_section_color(app_state.ui.help.active_section, ctx);
    let panel_title = Line::from(Span::styled(
        " Help ",
        ctx.apply(Style::default().fg(active_color).bold()),
    ));
    let vertical_padding = u16::from(matches!(layout.density, HelpDensity::Spacious));
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
        .title_top(panel_title)
        .padding(Padding::new(1, 1, vertical_padding, vertical_padding));
    f.render_widget(outer_block, layout.panel);

    draw_help_hero(
        f,
        layout.hero,
        app_state,
        items.len(),
        search_panel_active,
        layout.density,
        ctx,
    );

    if let (Some(warning_area), Some(warning_text)) = (layout.warning, &app_state.system_warning) {
        draw_warning(f, warning_area, warning_text, ctx);
    }

    if layout.table.height > 0 && layout.table.width > 0 {
        draw_help_table(f, layout.table, app_state, &items, ctx);
    }
    draw_help_controls(f, layout.controls, app_state, ctx);
}

fn draw_warning(f: &mut Frame, area: Rect, warning_text: &str, ctx: &ThemeContext) {
    if area.height == 0 {
        return;
    }

    let warning = Paragraph::new(warning_text)
        .wrap(Wrap { trim: true })
        .style(ctx.apply(Style::default().fg(ctx.state_warning())));
    f.render_widget(warning, area);
}

fn draw_help_hero(
    f: &mut Frame,
    area: Rect,
    app_state: &AppState,
    visible_count: usize,
    search_view: bool,
    density: HelpDensity,
    ctx: &ThemeContext,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let section = app_state.ui.help.active_section;
    let color = help_section_color(section, ctx);
    let title = if search_view {
        "Search results"
    } else {
        section.label()
    };
    let count_label = if search_view {
        format!("{visible_count} matches")
    } else {
        format!("{visible_count} entries")
    };

    if matches!(density, HelpDensity::Compact) {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    title,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text).bold()),
                ),
                Span::styled(
                    format!("  ·  {count_label}"),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ])),
            area,
        );
        return;
    }

    let hero_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)));
    let inner = hero_block.inner(area);
    f.render_widget(hero_block, area);
    if inner.height == 0 {
        return;
    }

    let columns = Layout::horizontal([Constraint::Min(1), Constraint::Length(14)])
        .split(Rect::new(inner.x, inner.y, inner.width, 1));
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            title,
            ctx.apply(Style::default().fg(ctx.theme.semantic.text).bold()),
        )])),
        columns[0],
    );
    f.render_widget(
        Paragraph::new(count_label)
            .alignment(Alignment::Right)
            .style(ctx.apply(Style::default().fg(color).bold())),
        columns[1],
    );

    if inner.height > 1 {
        let description = if search_view {
            match app_state.ui.help.search_mode {
                SearchMode::Fuzzy => "Searching all sections · Tab changes mode · Esc clears",
                SearchMode::Regex => "Searching all sections · Tab changes mode · Esc clears",
            }
        } else {
            section.description()
        };
        f.render_widget(
            Paragraph::new(truncate_with_ellipsis(description, inner.width as usize))
                .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }
}

fn draw_help_tabs(f: &mut Frame, area: Rect, app_state: &AppState, ctx: &ThemeContext) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let active = app_state.ui.help.active_section;
    let gap_width = if area.width >= help_tabs_width(3) {
        Some(3)
    } else if area.width >= help_tabs_width(1) {
        Some(1)
    } else {
        None
    };

    let spans = if let Some(gap_width) = gap_width {
        let mut spans = Vec::new();
        for (idx, section) in HELP_SECTIONS.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::styled(
                    " ".repeat(gap_width),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ));
            }
            let style = if *section == active {
                ctx.apply(
                    Style::default()
                        .fg(help_section_color(*section, ctx))
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                )
            } else {
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
            };
            spans.push(Span::styled(section.label(), style));
        }
        spans
    } else if area.width >= 32 {
        let prev = active.prev();
        let next = active.next();
        vec![
            Span::styled(
                format!("‹ {}  ", prev.label()),
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
            Span::styled(
                active.label(),
                ctx.apply(
                    Style::default()
                        .fg(help_section_color(active, ctx))
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ),
            ),
            Span::styled(
                format!("  {} ›", next.label()),
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
        ]
    } else {
        vec![Span::styled(
            format!("‹ {} ›", active.label()),
            ctx.apply(
                Style::default()
                    .fg(help_section_color(active, ctx))
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
        )]
    };

    f.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

fn help_tabs_width(gap_width: u16) -> u16 {
    HELP_SECTIONS
        .iter()
        .map(|section| section.label().chars().count() as u16)
        .sum::<u16>()
        + gap_width * HELP_SECTIONS.len().saturating_sub(1) as u16
}

fn draw_help_search_panel(
    f: &mut Frame,
    area: Rect,
    app_state: &AppState,
    visible_count: usize,
    ctx: &ThemeContext,
) {
    if area.height == 0 {
        return;
    }

    let mut trailing_spans = help_search_mode_spans(app_state, ctx);
    trailing_spans.push(Span::styled(
        format!("  {visible_count} matches"),
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
    ));
    draw_prompt_panel(
        f,
        area,
        " Help Search ".to_string(),
        sanitize_text(&app_state.ui.help.search_query),
        trailing_spans,
        ctx,
    );
}

fn help_search_mode_spans(app_state: &AppState, ctx: &ThemeContext) -> Vec<Span<'static>> {
    let (fuzzy_style, regex_style) = match app_state.ui.help.search_mode {
        SearchMode::Fuzzy => (
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ),
        SearchMode::Regex => (
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ),
    };
    vec![
        Span::raw("  "),
        Span::styled("Fuzzy", fuzzy_style),
        Span::raw(" / "),
        Span::styled("Regex", regex_style),
    ]
}

fn help_section_color(section: HelpSection, ctx: &ThemeContext) -> Color {
    match section {
        HelpSection::General => ctx.state_selected(),
        HelpSection::Torrents => ctx.state_success(),
        HelpSection::Graphs => ctx.accent_teal(),
        HelpSection::Legends => ctx.accent_peach(),
        HelpSection::Screens => ctx.accent_sapphire(),
        HelpSection::Paths => ctx.state_info(),
        HelpSection::Build => ctx.state_warning(),
    }
}

fn help_table_capacity(area: Rect) -> usize {
    area.height.max(1) as usize
}

fn clamped_scroll_offset(scroll_offset: usize, len: usize, visible_count: usize) -> usize {
    if len <= visible_count {
        return 0;
    }
    scroll_offset.min(len.saturating_sub(visible_count))
}

fn help_table_area_for_state(app_state: &AppState) -> Rect {
    if app_state.screen_area.width == 0 || app_state.screen_area.height == 0 {
        return Rect::new(0, 0, 1, 1);
    }

    let search_panel_active =
        app_state.ui.help.is_searching || !app_state.ui.help.search_query.is_empty();
    calculate_help_layout(
        app_state.screen_area,
        search_panel_active,
        app_state.system_warning.as_deref(),
    )
    .table
}

fn help_visible_count_for_state(app_state: &AppState) -> usize {
    help_table_capacity(help_table_area_for_state(app_state))
}

fn max_help_scroll_offset(settings: &Settings, app_state: &AppState) -> usize {
    let items = help_items_for_view(settings, app_state);
    let search_view = app_state.ui.help.is_searching || !app_state.ui.help.search_query.is_empty();
    let height = help_content_height(
        &items,
        search_view,
        help_table_area_for_state(app_state).width.saturating_sub(1),
    );
    clamped_scroll_offset(usize::MAX, height, help_visible_count_for_state(app_state))
}

fn help_marker_key_line(
    marker: &'static str,
    marker_color: Color,
    label: &str,
    ctx: &ThemeContext,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(marker, ctx.apply(Style::default().fg(marker_color).bold())),
        Span::styled(
            format!(" {label}"),
            ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
        ),
    ])
}

fn help_key_label(item: &HelpItem) -> String {
    let key = sanitize_text(&item.key);
    match item.key_style {
        HelpKeyStyle::Plain => key,
        HelpKeyStyle::DiskRead => format!("↑ {key}"),
        HelpKeyStyle::DiskWrite => format!("↓ {key}"),
        _ => format!("■ {key}"),
    }
}

fn help_item_key_line(item: &HelpItem, ctx: &ThemeContext) -> Line<'static> {
    match item.key_style {
        HelpKeyStyle::Plain => Line::from(Span::styled(
            help_key_label(item),
            help_key_style(ctx, item.action_tone).bold(),
        )),
        HelpKeyStyle::PeerDownloadOpportunity => {
            help_marker_key_line("■", ctx.accent_sapphire(), &item.key, ctx)
        }
        HelpKeyStyle::PeerDownloadBlocked => {
            help_marker_key_line("■", ctx.accent_maroon(), &item.key, ctx)
        }
        HelpKeyStyle::PeerUploadOpportunity => {
            help_marker_key_line("■", ctx.accent_teal(), &item.key, ctx)
        }
        HelpKeyStyle::PeerUploadRestricted => {
            help_marker_key_line("■", ctx.accent_peach(), &item.key, ctx)
        }
        HelpKeyStyle::DiskRead => help_marker_key_line("↑", ctx.state_success(), &item.key, ctx),
        HelpKeyStyle::DiskWrite => help_marker_key_line("↓", ctx.accent_sky(), &item.key, ctx),
    }
}

// Measurement and drawing both use Paragraph's wrapper, including Unicode widths.
fn wrapped_height(text: &str, width: u16) -> usize {
    Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(width.max(1))
        .max(1)
}

fn help_key_width(width: u16) -> Option<u16> {
    (width >= 64).then_some(28)
}

fn help_row_height(row: &HelpDisplayRow<'_>, width: u16) -> usize {
    match row {
        HelpDisplayRow::Spacer => 1,
        HelpDisplayRow::Heading { title, .. } => wrapped_height(title, width),
        HelpDisplayRow::Item(item) => {
            let key = help_key_label(item);
            let action = sanitize_text(&item.action);
            if let Some(key_width) = help_key_width(width) {
                wrapped_height(&key, key_width)
                    .max(wrapped_height(&action, width.saturating_sub(key_width + 2)))
            } else {
                wrapped_height(&key, width) + wrapped_height(&action, width.saturating_sub(2))
            }
        }
    }
}

fn help_content_height(items: &[HelpItem], search_view: bool, width: u16) -> usize {
    help_display_rows(items, search_view)
        .iter()
        .map(|row| help_row_height(row, width))
        .sum()
}

// A paragraph can begin above the viewport: scroll inside it instead of dropping
// its remaining lines. This keeps long descriptions and paths fully reachable.
fn draw_help_paragraph(f: &mut Frame, area: Rect, start: usize, scroll: usize, line: Line<'_>) {
    let height = Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width);
    let hidden = scroll.saturating_sub(start);
    let y = start.saturating_sub(scroll);
    if hidden >= height || y >= usize::from(area.height) || area.width == 0 {
        return;
    }
    let visible = (height - hidden).min(usize::from(area.height) - y) as u16;
    f.render_widget(
        Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .scroll((hidden as u16, 0)),
        Rect::new(area.x, area.y + y as u16, area.width, visible),
    );
}

fn draw_help_table(
    f: &mut Frame,
    area: Rect,
    app_state: &AppState,
    items: &[HelpItem],
    ctx: &ThemeContext,
) {
    if area.is_empty() {
        return;
    }
    let muted = ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0));
    if items.is_empty() {
        let invalid_regex = app_state.ui.help.search_mode == SearchMode::Regex
            && regex::Regex::new(app_state.ui.help.search_query.trim()).is_err();
        let message = if invalid_regex {
            "Invalid regular expression. Edit the query or press Tab for fuzzy search."
        } else {
            "No matching commands. Try a shorter query, or press Esc to browse sections."
        };
        f.render_widget(
            Paragraph::new(message)
                .wrap(Wrap { trim: false })
                .style(muted),
            area,
        );
        return;
    }
    // Leave a dedicated gutter for the scroll indicator.
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    let search_view = app_state.ui.help.is_searching || !app_state.ui.help.search_query.is_empty();
    let rows = help_display_rows(items, search_view);
    let total = help_content_height(items, search_view, content.width);
    let scroll = clamped_scroll_offset(
        app_state.ui.help.scroll_offset,
        total,
        help_table_capacity(area),
    );
    let mut start = 0;
    for row in rows {
        let height = help_row_height(&row, content.width);
        match &row {
            HelpDisplayRow::Spacer => {}
            HelpDisplayRow::Heading { section, title } => {
                draw_help_paragraph(
                    f,
                    content,
                    start,
                    scroll,
                    Line::styled(
                        title.clone(),
                        ctx.apply(
                            Style::default()
                                .fg(help_section_color(*section, ctx))
                                .bold(),
                        ),
                    ),
                );
            }
            HelpDisplayRow::Item(item) => {
                let key = help_item_key_line(item, ctx);
                let action = Line::styled(
                    sanitize_text(&item.action),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                );
                if let Some(key_width) = help_key_width(content.width) {
                    draw_help_paragraph(
                        f,
                        Rect::new(content.x, content.y, key_width, content.height),
                        start,
                        scroll,
                        key,
                    );
                    draw_help_paragraph(
                        f,
                        Rect::new(
                            content.x + key_width + 2,
                            content.y,
                            content.width - key_width - 2,
                            content.height,
                        ),
                        start,
                        scroll,
                        action,
                    );
                } else {
                    let key_height = wrapped_height(&help_key_label(item), content.width);
                    draw_help_paragraph(f, content, start, scroll, key);
                    draw_help_paragraph(
                        f,
                        Rect::new(
                            content.x + 2,
                            content.y,
                            content.width.saturating_sub(2),
                            content.height,
                        ),
                        start + key_height,
                        scroll,
                        action,
                    );
                }
            }
        }
        start += height;
    }
    if total > usize::from(area.height) {
        let mut state = ScrollbarState::new(total.saturating_sub(usize::from(area.height)) + 1)
            .position(scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface1)))
                .thumb_style(muted),
            area,
            &mut state,
        );
    }
}

fn draw_help_controls(f: &mut Frame, area: Rect, app_state: &AppState, ctx: &ThemeContext) {
    if area.height == 0 {
        return;
    }

    let search_panel_active =
        app_state.ui.help.is_searching || !app_state.ui.help.search_query.is_empty();
    let tight = area.width < 48;
    let compact = area.width < 68;
    let entries: &[(&str, &str, ActionTone)] = if search_panel_active && tight {
        &[
            ("Tab", "mode", ActionTone::Mode),
            ("Esc", "clear", ActionTone::Cancel),
        ]
    } else if search_panel_active && compact {
        &[
            ("Tab", "mode", ActionTone::Mode),
            ("Enter", "keep", ActionTone::Confirm),
            ("Esc", "clear", ActionTone::Cancel),
        ]
    } else if search_panel_active {
        &[
            ("type", "query", ActionTone::Edit),
            ("Tab", "mode", ActionTone::Mode),
            ("Enter", "keep", ActionTone::Confirm),
            ("Esc", "clear", ActionTone::Cancel),
            ("↑/↓", "scroll", ActionTone::Navigate),
        ]
    } else if tight {
        &[
            ("Esc", "close", ActionTone::Cancel),
            ("Tab", "next", ActionTone::Mode),
            ("/", "find", ActionTone::Search),
        ]
    } else if compact {
        &[
            ("Esc", "close", ActionTone::Cancel),
            ("Tab", "section", ActionTone::Mode),
            ("/", "search", ActionTone::Search),
        ]
    } else if area.width < 94 {
        &[
            ("Esc", "close", ActionTone::Cancel),
            ("Tab", "section", ActionTone::Mode),
            ("/", "search", ActionTone::Search),
            ("↑/↓", "scroll", ActionTone::Navigate),
        ]
    } else {
        &[
            ("Esc", "close", ActionTone::Cancel),
            ("Tab", "section", ActionTone::Mode),
            ("/", "search", ActionTone::Search),
            ("↑/↓", "scroll", ActionTone::Navigate),
            ("PgUp/Dn", "page", ActionTone::Navigate),
        ]
    };

    let mut spans = Vec::new();
    for (idx, (key, label, tone)) in entries.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(
                " | ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ));
        }
        spans.push(Span::styled(
            *key,
            crate::tui::action_style::footer_key_style(ctx, *tone),
        ));
        if !label.is_empty() {
            spans.push(Span::styled(
                format!(" {label}"),
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ));
        }
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::service::{DhtStatus, DhtWaveTelemetry};
    use crate::terminal_event::{KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_help_screen(width: u16, height: u16, mut app_state: AppState) -> String {
        app_state.mode = AppMode::Help;
        app_state.screen_area = Rect::new(0, 0, width, height);
        render_help_state(width, height, &app_state)
    }

    fn render_help_state(width: u16, height: u16, app_state: &AppState) -> String {
        let settings = Settings::default();
        let dht_status = DhtStatus::default();
        let dht_wave_telemetry = DhtWaveTelemetry::default();
        let theme = ThemeContext::new(app_state.theme, 0.0);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let screen = ScreenContext::new(
                    app_state,
                    &dht_status,
                    &dht_wave_telemetry,
                    &settings,
                    &theme,
                );
                draw(frame, &screen);
            })
            .expect("draw help screen");

        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn help_layout_keeps_navigation_and_footer_outside_content() {
        for area in [
            Rect::new(0, 0, 120, 36),
            Rect::new(0, 0, 80, 48),
            Rect::new(0, 0, 60, 36),
            Rect::new(0, 0, 40, 10),
        ] {
            let layout = calculate_help_layout(area, false, None);
            assert!(layout.tabs.bottom() <= layout.panel.y);
            assert!(layout.panel.bottom() <= layout.controls.y);
            assert!(layout.controls.bottom() <= layout.popup.bottom());
            assert!(layout.table.height > 0);
            let inner = help_panel_inner(layout.panel, layout.density);
            assert_eq!(inner.x, layout.panel.x + 2);
            assert_eq!(inner.right(), layout.panel.right() - 2);
        }
    }

    #[test]
    fn classic_help_tabs_fit_common_vertical_widths() {
        assert_eq!(help_tabs_width(3), 63);
        assert_eq!(help_tabs_width(1), 51);

        let eighty_columns = calculate_help_layout(Rect::new(0, 0, 80, 48), false, None);
        assert!(eighty_columns.tabs.width >= help_tabs_width(3));

        let sixty_columns = calculate_help_layout(Rect::new(0, 0, 60, 36), false, None);
        assert!(sixty_columns.tabs.width >= help_tabs_width(1));
    }

    #[test]
    fn help_layout_keeps_search_warning_and_table_disjoint() {
        for area in [Rect::new(0, 0, 120, 36), Rect::new(0, 0, 80, 48)] {
            let layout =
                calculate_help_layout(area, true, Some("Network discovery is still warming up"));

            let search = layout.search.expect("visible search panel");
            let warning = layout.warning.expect("visible warning row");
            assert!(search.bottom() <= layout.tabs.y);
            assert!(layout.tabs.bottom() <= layout.panel.y);
            assert!(layout.hero.bottom() <= warning.y);
            assert!(warning.bottom() <= layout.table.y);
            assert!(
                layout.table.bottom() <= help_panel_inner(layout.panel, layout.density).bottom()
            );
            assert!(layout.panel.bottom() <= layout.controls.y);
        }
    }

    #[test]
    fn compact_search_prioritizes_warning_over_optional_rows() {
        let warning_text = "Open file limit is low";

        for (height, expected_table_height) in [(10, 1), (9, 0)] {
            let layout =
                calculate_help_layout(Rect::new(0, 0, 40, height), true, Some(warning_text));
            let inner = help_panel_inner(layout.panel, layout.density);
            let warning = layout.warning.expect("visible warning row");

            assert_eq!(layout.density, HelpDensity::Compact);
            assert_eq!(layout.hero.height, 0);
            assert_eq!(warning.height, warning_height(warning_text, inner.width));
            assert_eq!(layout.table.height, expected_table_height);
            assert!(warning.bottom() <= layout.table.y);
            assert!(layout.table.bottom() <= inner.bottom());
        }
    }

    #[test]
    fn compact_search_render_preserves_system_warning() {
        for height in [10, 9] {
            let mut app_state = AppState {
                system_warning: Some("Open file limit is low".to_string()),
                ..Default::default()
            };
            app_state.ui.help.is_searching = true;

            let rendered = render_help_screen(40, height, app_state);

            assert!(
                rendered.contains("Open file limit is low"),
                "missing warning at 40x{height}:\n{rendered}"
            );
        }
    }

    #[test]
    fn help_section_descriptions_are_complete() {
        for section in HELP_SECTIONS {
            assert!(!section.description().is_empty());
        }
    }

    #[test]
    fn wide_help_render_shows_top_tabs_and_dashboard_shortcuts() {
        let rendered = render_help_screen(120, 36, AppState::default());

        let tab_line = rendered
            .lines()
            .find(|line| line.contains("Torrents"))
            .expect("top tabs");
        for section in HELP_SECTIONS {
            assert!(tab_line.contains(section.label()));
        }
        for section in HELP_SECTIONS {
            assert!(rendered.contains(section.label()));
        }
        assert!(rendered.contains("Everyday shortcuts"));
        assert!(rendered.contains("Dashboard shortcuts"));
        assert!(rendered.contains("Open Config"));
        assert!(!rendered.contains("[C]"));
    }

    #[test]
    fn vertical_help_renders_all_tabs_and_external_footer() {
        for (width, height) in [(80, 48), (60, 36)] {
            let rendered = render_help_screen(width, height, AppState::default());
            for section in HELP_SECTIONS {
                assert!(
                    rendered.contains(section.label()),
                    "missing {} at {width}x{height}:\n{rendered}",
                    section.label()
                );
            }
            assert!(rendered.contains("Dashboard shortcuts"));
            assert!(rendered.contains("Tab section"));
        }
    }

    #[test]
    fn compact_and_tight_help_renders_keep_core_controls_visible() {
        for (width, height) in [(80, 24), (60, 18), (40, 10)] {
            let rendered = render_help_screen(width, height, AppState::default());
            assert!(
                rendered.contains("General"),
                "missing active section at {width}x{height}:\n{rendered}"
            );
        }
    }

    #[test]
    fn vertical_help_scroll_clamps_to_planned_table_height() {
        let settings = Settings::default();
        for (width, height) in [(80, 48), (60, 36)] {
            let mut app_state = AppState {
                mode: AppMode::Help,
                screen_area: Rect::new(0, 0, width, height),
                ..Default::default()
            };
            app_state.ui.help.is_searching = true;
            let display_rows = help_content_height(
                &help_items_for_view(&settings, &app_state),
                true,
                help_table_area_for_state(&app_state)
                    .width
                    .saturating_sub(1),
            );
            let expected_max =
                display_rows.saturating_sub(help_visible_count_for_state(&app_state));

            for _ in 0..display_rows + 8 {
                reduce_help_action(&mut app_state, &settings, HelpAction::ScrollDown);
            }

            assert_eq!(app_state.ui.help.scroll_offset, expected_max);
        }
    }

    #[test]
    fn search_render_keeps_prompt_results_and_global_scope_visible() {
        let mut app_state = AppState::default();
        app_state.ui.help.is_searching = true;
        app_state.ui.help.search_query = "queue".to_string();

        let rendered = render_help_screen(120, 36, app_state);

        assert!(rendered.contains("Help Search"));
        assert!(rendered.contains("Search results"));
        assert!(rendered.contains("matches"));

        let mut tight_state = AppState::default();
        tight_state.ui.help.is_searching = true;
        tight_state.ui.help.search_query = "path".to_string();
        let tight_rendered = render_help_screen(40, 10, tight_state);
        assert!(tight_rendered.contains("Help Search"));
    }

    #[test]
    fn help_can_render_below_its_minimum_useful_size() {
        for (width, height) in [(1, 1), (8, 4), (24, 8), (40, 10)] {
            let mut state = AppState::default();
            render_help_state(width, height, &state);
            state.ui.help.is_searching = true;
            state.system_warning = Some("Service notice".to_string());
            render_help_state(width, height, &state);
        }
    }

    #[test]
    fn top_tabs_keep_the_full_panel_width_at_every_size() {
        for area in [
            Rect::new(0, 0, 120, 36),
            Rect::new(0, 0, 80, 36),
            Rect::new(0, 0, 120, 18),
        ] {
            let layout = calculate_help_layout(area, false, None);
            assert_eq!(layout.tabs.height, 1);
            assert!(layout.tabs.bottom() <= layout.panel.y);
            assert_eq!(layout.panel.x, layout.popup.x);
            assert_eq!(layout.panel.width, layout.popup.width);
        }
    }

    #[test]
    fn paging_and_end_reach_wrapped_content_after_resize() {
        let settings = Settings::default();
        let mut state = AppState {
            mode: AppMode::Help,
            screen_area: Rect::new(0, 0, 80, 24),
            ..Default::default()
        };
        state.ui.help.active_section = HelpSection::Screens;
        handle_event_with_settings(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            &mut state,
            &settings,
        );
        assert_eq!(
            state.ui.help.scroll_offset,
            help_visible_count_for_state(&state) - 1
        );
        handle_event_with_settings(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            &mut state,
            &settings,
        );
        assert_eq!(state.ui.help.scroll_offset, 0);
        for (width, height) in [(40, 12), (80, 24), (120, 36)] {
            state.screen_area = Rect::new(0, 0, width, height);
            handle_event_with_settings(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
                &mut state,
                &settings,
            );
            assert_eq!(
                state.ui.help.scroll_offset,
                max_help_scroll_offset(&settings, &state)
            );
            let rendered = render_help_state(width, height, &state);
            assert!(
                rendered.contains("Confirm delete"),
                "last action must be reachable:\n{rendered}"
            );
        }
        handle_event_with_settings(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            &mut state,
            &settings,
        );
        assert_eq!(state.ui.help.scroll_offset, 0);
    }

    #[test]
    fn long_path_is_readable_across_scrolled_pages() {
        let settings = Settings::default();
        let mut state = AppState {
            mode: AppMode::Help,
            screen_area: Rect::new(0, 0, 40, 12),
            ..Default::default()
        };
        state.ui.help.active_section = HelpSection::Paths;
        state.runtime_paths.settings_path = Some(std::path::PathBuf::from(
            "/archive/研究/quiet-valley/observations/seasonal-records/configuration/unique-tail.toml"));
        let mut all_visible = String::new();
        for offset in 0..=max_help_scroll_offset(&settings, &state) {
            state.ui.help.scroll_offset = offset;
            all_visible.push_str(&render_help_state(40, 12, &state));
        }
        // TestBackend retains the blank continuation cell after each wide glyph.
        assert!(all_visible.contains('研') && all_visible.contains('究'));
        assert!(all_visible.contains("unique-tail.toml"));
        assert!(!all_visible.contains('…'));
    }

    #[test]
    fn search_errors_offer_a_recovery_action() {
        let mut state = AppState::default();
        state.ui.help.is_searching = true;
        state.ui.help.search_mode = SearchMode::Regex;
        state.ui.help.search_query = "[".to_string();
        let rendered = render_help_state(80, 24, &state);
        assert!(rendered.contains("Invalid regular expression"));
        state.ui.help.search_query = "unmatched-reference-4821".to_string();
        let rendered = render_help_screen(80, 24, state);
        assert!(rendered.contains("No matching commands"));
    }

    #[test]
    fn help_esc_returns_to_normal() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn help_m_press_returns_to_normal() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn help_ignores_non_close_key() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Help));
    }

    #[test]
    fn help_handler_ignores_when_not_in_help_mode() {
        let mut app_state = AppState {
            mode: AppMode::Normal,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn help_tab_cycles_sections_and_resets_scroll() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };
        app_state.ui.help.scroll_offset = 12;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert_eq!(app_state.ui.help.active_section, HelpSection::Torrents);
        assert_eq!(app_state.ui.help.scroll_offset, 0);
        assert!(matches!(app_state.mode, AppMode::Help));
    }

    #[test]
    fn help_arrow_keys_scroll() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            &mut app_state,
        );
        assert_eq!(app_state.ui.help.scroll_offset, 1);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            &mut app_state,
        );
        assert_eq!(app_state.ui.help.scroll_offset, 0);
    }

    #[test]
    fn layout_keeps_search_above_the_content_panel_below_the_header() {
        let layout = calculate_help_layout(Rect::new(0, 0, 120, 40), true, Some("Service notice"));
        let search = layout.search.expect("search region");
        let warning = layout.warning.expect("warning region");

        assert!(search.bottom() <= layout.tabs.y);
        assert!(layout.tabs.bottom() <= layout.panel.y);
        assert!(layout.hero.bottom() <= warning.y);
        assert!(warning.bottom() <= layout.table.y);
        assert!(layout.table.bottom() <= help_panel_inner(layout.panel, layout.density).bottom());
        assert!(layout.panel.bottom() <= layout.controls.y);
    }

    #[test]
    fn scroll_capacity_uses_the_rendered_content_region_with_search() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            screen_area: Rect::new(0, 0, 120, 30),
            system_warning: Some("Service notice".to_string()),
            ..Default::default()
        };
        app_state.ui.help.is_searching = true;
        let expected = calculate_help_layout(
            app_state.screen_area,
            true,
            app_state.system_warning.as_deref(),
        )
        .table;

        assert_eq!(help_table_area_for_state(&app_state), expected);
        assert_eq!(
            help_visible_count_for_state(&app_state),
            expected.height as usize
        );
    }

    #[test]
    fn help_down_scroll_clamps_at_visible_bottom() {
        let settings = Settings::default();
        let mut app_state = AppState {
            mode: AppMode::Help,
            screen_area: Rect::new(0, 0, 120, 14),
            ..Default::default()
        };
        let max_scroll = max_help_scroll_offset(&settings, &app_state);
        assert!(max_scroll > 0);

        for _ in 0..max_scroll + 8 {
            handle_event_with_settings(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                &mut app_state,
                &settings,
            );
        }
        assert_eq!(app_state.ui.help.scroll_offset, max_scroll);

        handle_event_with_settings(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            &mut app_state,
            &settings,
        );
        assert_eq!(app_state.ui.help.scroll_offset, max_scroll - 1);
    }

    #[test]
    fn help_display_rows_add_space_between_subsections() {
        let items = vec![
            HelpItem::new(HelpSection::General, "One", "a", "First action"),
            HelpItem::new(HelpSection::General, "Two", "b", "Second action"),
        ];

        let rows = help_display_rows(&items, false);

        assert!(matches!(rows[0], HelpDisplayRow::Heading { .. }));
        assert!(matches!(rows[1], HelpDisplayRow::Item(_)));
        assert!(matches!(rows[2], HelpDisplayRow::Spacer));
        assert!(matches!(rows[3], HelpDisplayRow::Heading { .. }));
        assert!(matches!(rows[4], HelpDisplayRow::Item(_)));
    }

    #[test]
    fn help_legend_items_keep_visual_marker_styles() {
        let settings = Settings::default();
        let app_state = AppState::default();

        let items = build_help_items(&settings, &app_state);

        let blue_peer = items
            .iter()
            .find(|item| item.subsection == "Peer Flags" && item.key == "Blue")
            .expect("blue peer flag help item");
        assert_eq!(blue_peer.key_style, HelpKeyStyle::PeerDownloadOpportunity);

        let read_metric = items
            .iter()
            .find(|item| item.subsection == "Disk Metrics" && item.key == "Read")
            .expect("read disk metric help item");
        assert_eq!(read_metric.key_style, HelpKeyStyle::DiskRead);
    }

    #[test]
    fn help_includes_peer_management_route_and_screen_controls() {
        let items = build_help_items(&Settings::default(), &AppState::default());

        assert!(items.iter().any(|item| {
            item.subsection == "Dashboard shortcuts"
                && item.key == "P"
                && item.action == "Open peer management"
        }));
        assert!(items.iter().any(|item| {
            item.subsection == "Peer Management"
                && item.key == "Tab / Shift+Tab"
                && item.action.contains("Restricted")
        }));
        assert!(items.iter().any(|item| {
            item.subsection == "Peer Management"
                && item.key == "Enter"
                && item.action.contains("details")
        }));
    }

    #[test]
    fn help_search_filters_across_all_sections() {
        let settings = Settings::default();
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };
        app_state.ui.help.active_section = HelpSection::General;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
            &mut app_state,
        );
        for ch in "rss".chars() {
            handle_event(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                &mut app_state,
            );
        }

        let items = help_items_for_view(&settings, &app_state);

        assert!(app_state.ui.help.is_searching);
        assert_eq!(app_state.ui.help.search_query, "rss");
        assert!(items
            .iter()
            .any(|item| item.section == HelpSection::Screens));
        assert!(items.iter().any(|item| item.action.contains("RSS")));
    }

    #[test]
    fn help_search_tab_toggles_fuzzy_and_regex() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };
        app_state.ui.help.is_searching = true;
        assert_eq!(app_state.ui.help.search_mode, SearchMode::Regex);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert_eq!(app_state.ui.help.search_mode, SearchMode::Fuzzy);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert_eq!(app_state.ui.help.search_mode, SearchMode::Regex);
    }

    #[test]
    fn help_search_start_defaults_to_regex() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };
        app_state.ui.help.search_mode = SearchMode::Fuzzy;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(app_state.ui.help.is_searching);
        assert_eq!(app_state.ui.help.search_mode, SearchMode::Regex);
    }

    #[test]
    fn help_regex_search_filters_all_sections() {
        let settings = Settings::default();
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };
        app_state.ui.help.is_searching = true;
        app_state.ui.help.search_mode = SearchMode::Regex;
        app_state.ui.help.search_query = "Torrent Management Y / Enter Review queued".to_string();

        let items = help_items_for_view(&settings, &app_state);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subsection, "Torrent Management");
        assert_eq!(items[0].key, "Y / Enter");
    }

    #[test]
    fn help_invalid_regex_matches_no_rows() {
        let settings = Settings::default();
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };
        app_state.ui.help.is_searching = true;
        app_state.ui.help.search_mode = SearchMode::Regex;
        app_state.ui.help.search_query = "[".to_string();

        let items = help_items_for_view(&settings, &app_state);

        assert!(items.is_empty());
    }

    #[test]
    fn help_esc_clears_search_before_closing() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };
        app_state.ui.help.is_searching = true;
        app_state.ui.help.search_query = "path".to_string();

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Help));
        assert!(!app_state.ui.help.is_searching);
        assert!(app_state.ui.help.search_query.is_empty());
    }

    #[test]
    fn help_ctrl_u_reopens_committed_search_panel() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };
        app_state.ui.help.is_searching = false;
        app_state.ui.help.search_query = "queue".to_string();
        app_state.ui.help.scroll_offset = 4;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            &mut app_state,
        );

        assert!(app_state.ui.help.is_searching);
        assert!(app_state.ui.help.search_query.is_empty());
        assert_eq!(app_state.ui.help.scroll_offset, 0);
        assert!(matches!(app_state.mode, AppMode::Help));
    }

    #[test]
    fn help_footer_includes_cluster_entries_when_present() {
        let settings = Settings::default();
        let app_state = AppState {
            cluster_role_label: Some("Leader".to_string()),
            cluster_runtime_label: Some("Reader".to_string()),
            ..Default::default()
        };

        let entries = build_help_footer_entries(&settings, &app_state);

        assert!(entries.contains(&("Cluster", "Leader".to_string())));
        assert!(entries.contains(&("Runtime", "Reader".to_string())));
    }

    #[test]
    fn help_footer_omits_cluster_entries_when_absent() {
        let settings = Settings::default();
        let app_state = AppState::default();

        let entries = build_help_footer_entries(&settings, &app_state);

        assert!(!entries.iter().any(|(label, _)| *label == "Cluster"));
        assert!(!entries.iter().any(|(label, _)| *label == "Runtime"));
    }
}
