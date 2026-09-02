// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppMode, AppState, HelpSection, SearchMode};
use crate::config::{
    is_shared_config_mode, local_settings_path, resolve_host_watch_path, runtime_log_dir,
    shared_inbox_path, shared_settings_path, Settings,
};
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
            Self::General => {
                "Move through Superseedr, search the manual, and reach every global workspace."
            }
            Self::Torrents => {
                "Add, pause, sort, and remove transfers with deliberate keyboard control."
            }
            Self::Graphs => "Tune live telemetry, time scale, refresh cadence, and presentation.",
            Self::Legends => {
                "Decode peer state, disk activity, DHT telemetry, and session progression."
            }
            Self::Screens => "Use the context-aware commands available in each focused workspace.",
            Self::Paths => "Inspect the resolved locations for this host and configuration mode.",
            Self::Build => "See the discovery capabilities compiled into this exact executable.",
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
    let log_path_str = runtime_log_dir()
        .map(|path| path.join("app*.log"))
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown location".to_string());

    let mut entries = if is_shared_config_mode() {
        vec![
            (
                "Settings",
                shared_settings_path()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unknown location".to_string()),
            ),
            ("Log Files", log_path_str),
            (
                "Host Watch",
                display_path_or_disabled(resolve_host_watch_path(settings)),
            ),
            (
                "Shared Inbox",
                shared_inbox_path()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unknown location".to_string()),
            ),
        ]
    } else {
        let settings_path_str = local_settings_path()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "Unknown location".to_string());
        let watch_path_str = crate::config::get_watch_path()
            .map(|(system_watch, _)| system_watch.to_string_lossy().to_string())
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
        "Esc / m / q",
        "Close the field manual and return to the dashboard",
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
        HelpSection::General,
        "Global Routes",
        "Q (shift+q)",
        "Quit the application",
        ActionTone::Destructive
    );
    action_item!(
        HelpSection::General,
        "Global Routes",
        "c",
        "Open Config",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Global Routes",
        "r",
        "Open RSS",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Global Routes",
        "J",
        "Open the event journal",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Global Routes",
        "M",
        "Open torrent management",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Global Routes",
        "P",
        "Open peer management",
        ActionTone::Open
    );
    action_item!(
        HelpSection::General,
        "Global Routes",
        "z",
        "Toggle Zen / Power Saving mode",
        ActionTone::Toggle
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

    if is_shared_config_mode() {
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
        if cfg!(feature = "dht") {
            "Included in this build"
        } else {
            "Not included in this private build"
        }
    );
    item!(
        HelpSection::Build,
        "Feature Set",
        "PEX",
        if cfg!(feature = "pex") {
            "Included in this build"
        } else {
            "Not included in this private build"
        }
    );
    item!(
        HelpSection::Build,
        "Feature Set",
        "Private mode",
        if cfg!(all(feature = "dht", feature = "pex")) {
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
        HelpAction::ScrollUp => {
            let max_scroll = max_help_scroll_offset(settings, app_state);
            app_state.ui.help.scroll_offset = app_state
                .ui
                .help
                .scroll_offset
                .min(max_scroll)
                .saturating_sub(1);
            HelpReduceResult {
                consumed: true,
                effects: Vec::new(),
            }
        }
        HelpAction::ScrollDown => {
            let max_scroll = max_help_scroll_offset(settings, app_state);
            app_state.ui.help.scroll_offset = app_state
                .ui
                .help
                .scroll_offset
                .min(max_scroll)
                .saturating_add(1)
                .min(max_scroll);
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

    f.render_widget(Clear, layout.popup);

    if let Some(search_area) = layout.search {
        draw_help_search_panel(f, search_area, app_state, items.len(), ctx);
    }
    draw_help_tabs(f, layout.tabs, app_state, ctx);

    let active_color = help_section_color(app_state.ui.help.active_section, ctx);
    let panel_title = Line::from(Span::styled(
        " ◆ ",
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
                Span::styled("◆ ", ctx.apply(Style::default().fg(color).bold())),
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
        Paragraph::new(Line::from(vec![
            Span::styled("◆ ", ctx.apply(Style::default().fg(color).bold())),
            Span::styled(
                title,
                ctx.apply(Style::default().fg(ctx.theme.semantic.text).bold()),
            ),
        ])),
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
                SearchMode::Fuzzy => {
                    "Fuzzy matching across every help section; Tab changes mode and Esc clears."
                }
                SearchMode::Regex => {
                    "Regex matching across every help section; Tab changes mode and Esc clears."
                }
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
    let display_rows = help_display_rows(&items, search_view);
    clamped_scroll_offset(
        usize::MAX,
        display_rows.len(),
        help_visible_count_for_state(app_state),
    )
}

fn help_marker_key_cell(
    marker: &'static str,
    marker_color: Color,
    label: &str,
    ctx: &ThemeContext,
) -> Cell<'static> {
    Cell::from(Line::from(vec![
        Span::styled(marker, ctx.apply(Style::default().fg(marker_color).bold())),
        Span::styled(
            format!(" {label}"),
            ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
        ),
    ]))
}

fn help_item_key_cell(item: &HelpItem, key_width: u16, ctx: &ThemeContext) -> Cell<'static> {
    match item.key_style {
        HelpKeyStyle::Plain => Cell::from(Span::styled(
            truncate_with_ellipsis(&format!("[{}]", item.key), key_width as usize),
            help_key_style(ctx, item.action_tone).bold(),
        )),
        HelpKeyStyle::PeerDownloadOpportunity => {
            help_marker_key_cell("■", ctx.accent_sapphire(), &item.key, ctx)
        }
        HelpKeyStyle::PeerDownloadBlocked => {
            help_marker_key_cell("■", ctx.accent_maroon(), &item.key, ctx)
        }
        HelpKeyStyle::PeerUploadOpportunity => {
            help_marker_key_cell("■", ctx.accent_teal(), &item.key, ctx)
        }
        HelpKeyStyle::PeerUploadRestricted => {
            help_marker_key_cell("■", ctx.accent_peach(), &item.key, ctx)
        }
        HelpKeyStyle::DiskRead => help_marker_key_cell("↑", ctx.state_success(), &item.key, ctx),
        HelpKeyStyle::DiskWrite => help_marker_key_cell("↓", ctx.accent_sky(), &item.key, ctx),
    }
}

fn draw_help_table(
    f: &mut Frame,
    area: Rect,
    app_state: &AppState,
    items: &[HelpItem],
    ctx: &ThemeContext,
) {
    if area.height == 0 {
        return;
    }

    let search_view = app_state.ui.help.is_searching || !app_state.ui.help.search_query.is_empty();
    let display_rows = help_display_rows(items, search_view);
    let visible_count = help_table_capacity(area);
    let scroll = clamped_scroll_offset(
        app_state.ui.help.scroll_offset,
        display_rows.len(),
        visible_count,
    );
    let visible_rows = display_rows
        .iter()
        .skip(scroll)
        .take(visible_count)
        .collect::<Vec<_>>();
    let key_width = if area.width >= 58 {
        28
    } else if area.width >= 42 {
        22
    } else {
        (area.width / 2).clamp(12, 20)
    };
    let column_spacing = u16::from(area.width >= 48);
    let action_width = area
        .width
        .saturating_sub(key_width)
        .saturating_sub(column_spacing)
        .saturating_sub(2) as usize;

    let rows = if visible_rows.is_empty() {
        vec![Row::new(vec![
            Cell::from(Span::styled(
                "-",
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            )),
            Cell::from(Span::styled(
                if app_state.ui.help.search_query.is_empty() {
                    "No help entries in this view"
                } else {
                    "No help entries match the search"
                },
                ctx.apply(Style::default().fg(ctx.state_warning())),
            )),
        ])]
    } else {
        visible_rows
            .into_iter()
            .map(|row| match row {
                HelpDisplayRow::Spacer => Row::new(vec![Cell::from(""), Cell::from("")]),
                HelpDisplayRow::Heading { section, title } => Row::new(vec![
                    Cell::from(Span::styled(
                        truncate_with_ellipsis(
                            &format!("◆ {}", title.to_uppercase()),
                            key_width as usize,
                        ),
                        ctx.apply(
                            Style::default()
                                .fg(help_section_color(*section, ctx))
                                .bold(),
                        ),
                    )),
                    Cell::from(""),
                ]),
                HelpDisplayRow::Item(item) => Row::new(vec![
                    help_item_key_cell(item, key_width, ctx),
                    Cell::from(Span::styled(
                        format!("  {}", truncate_with_ellipsis(&item.action, action_width)),
                        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
                    )),
                ]),
            })
            .collect()
    };

    let table = Table::new(rows, [Constraint::Length(key_width), Constraint::Min(1)])
        .column_spacing(column_spacing);

    f.render_widget(table, area);
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
            ("Tab", "", ActionTone::Mode),
            ("Enter", "", ActionTone::Confirm),
            ("Esc", "", ActionTone::Cancel),
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
            ("Esc", "", ActionTone::Cancel),
            ("Tab", "", ActionTone::Mode),
            ("/", "", ActionTone::Search),
        ]
    } else if compact {
        &[
            ("Esc", "close", ActionTone::Cancel),
            ("Tab", "section", ActionTone::Mode),
            ("/", "search", ActionTone::Search),
        ]
    } else {
        &[
            ("Esc/m/q", "close", ActionTone::Cancel),
            ("Tab", "section", ActionTone::Mode),
            ("/", "search", ActionTone::Search),
            ("↑/↓", "scroll", ActionTone::Navigate),
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
            format!("[{key}]"),
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
    use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
    use crate::terminal_event::{KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_help_screen(width: u16, height: u16, mut app_state: AppState) -> String {
        app_state.mode = AppMode::Help;
        app_state.screen_area = Rect::new(0, 0, width, height);
        let settings = Settings::default();
        let dht_status = DhtStatus::default();
        let dht_wave_telemetry = DhtWaveTelemetry::default();
        let theme = ThemeContext::new(app_state.theme, 0.0);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let screen = ScreenContext::new(
                    &app_state,
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
    fn help_layout_uses_top_tabs_and_external_footer() {
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
    fn wide_help_render_keeps_classic_chrome_and_simplified_content() {
        let rendered = render_help_screen(120, 36, AppState::default());

        assert!(!rendered.contains("FIELD INDEX"));
        assert!(!rendered.contains("FIELD NOTE"));
        for section in HELP_SECTIONS {
            assert!(rendered.contains(section.label()));
        }
        assert!(rendered.contains("search the manual"));
        assert!(rendered.contains("HELP NAVIGATION"));
        assert!(!rendered.contains("ROWS 1-"));
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
            assert!(!rendered.contains("FIELD INDEX"));
            assert!(rendered.contains("HELP NAVIGATION"));
            assert!(rendered.contains("[Tab] section"));
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
            let display_rows =
                help_display_rows(&help_items_for_view(&settings, &app_state), true).len();
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
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
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
            item.subsection == "Global Routes"
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
