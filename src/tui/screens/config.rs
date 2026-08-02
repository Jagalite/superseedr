// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppCommand, AppMode, ConfigEditState, ConfigItem, ConfigPane, FileBrowserMode};
use crate::config::Settings;
use crate::networking::runtime::NetworkRuntimePhase;
use crate::networking::{
    available_network_interfaces, DnsPolicy, NetworkBindingMode, NetworkInterfaceInfo,
};
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::spawn_app_command_sender;
use crate::tui::formatters::{
    format_limit_bps, format_speed, path_to_string, truncate_with_ellipsis,
};
use crate::tui::layout::config::{calculate_config_layout, ConfigLayoutKind};
use crate::tui::screen_context::ScreenContext;
use directories::UserDirs;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Frame, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use tokio::sync::{broadcast, mpsc};

const UNLIMITED_RATE_LIMIT_BPS: u64 = crate::config::UNLIMITED_RATE_LIMIT_BPS;

#[derive(Clone, Debug, PartialEq)]
pub enum ConfigAction {
    Exit,
    ToggleAnonymize,
    ShiftSelected,
    SetSelectedBool(bool),
    MoveUp,
    MoveDown,
    RequestReset,
    ResetSelected,
    IncreaseSelected,
    DecreaseSelected,
    EditInsert(char),
    EditBackspace,
    EditDelete,
    EditMoveLeft,
    EditMoveRight,
    EditMoveHome,
    EditMoveEnd,
    EditCancel,
    EditCommit,
}

pub enum ConfigEffect {
    AppCommand(Box<AppCommand>),
    ApplySettings,
}

pub struct ConfigHandleContext<'a> {
    pub mode: &'a mut AppMode,
    pub anonymize: &'a mut bool,
    pub settings_edit: &'a mut Box<Settings>,
    pub applied_settings: &'a Settings,
    pub selected_index: &'a mut usize,
    pub items: &'a mut [ConfigItem],
    pub active_pane: &'a mut ConfigPane,
    pub editing: &'a mut Option<ConfigEditState>,
    pub reset_confirmation: &'a mut Option<ConfigItem>,
    pub shared_follower: bool,
    pub compact: bool,
    pub app_command_tx: &'a mpsc::Sender<AppCommand>,
    pub shutdown_tx: &'a broadcast::Sender<()>,
    pub file_browser_generation: &'a mut u64,
}

#[derive(Default)]
pub struct ConfigReduceResult {
    pub consumed: bool,
    pub effects: Vec<ConfigEffect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigCategory {
    Paths,
    Downloads,
    Network,
    Ui,
}

impl ConfigCategory {
    fn label(self) -> &'static str {
        match self {
            Self::Paths => "Paths",
            Self::Downloads => "Downloads",
            Self::Network => "Network",
            Self::Ui => "UI",
        }
    }

    fn all() -> &'static [Self] {
        &[Self::Network, Self::Paths, Self::Downloads, Self::Ui]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigControlKind {
    Bool,
    Enum,
    Number,
    RateLimit,
    Path,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigScope {
    Host,
    Shared,
}

#[derive(Clone, Copy, Debug)]
struct ConfigSettingDescriptor {
    item: ConfigItem,
    category: ConfigCategory,
    label: &'static str,
    control: ConfigControlKind,
    scope: ConfigScope,
}

fn config_setting_descriptors() -> &'static [ConfigSettingDescriptor] {
    &[
        ConfigSettingDescriptor {
            item: ConfigItem::ClientPort,
            category: ConfigCategory::Network,
            label: "Listen Port",
            control: ConfigControlKind::Number,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::NetworkBindingMode,
            category: ConfigCategory::Network,
            label: "Network Routing",
            control: ConfigControlKind::Enum,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::NetworkInterface,
            category: ConfigCategory::Network,
            label: "Interface",
            control: ConfigControlKind::Enum,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::NetworkIpv4Enabled,
            category: ConfigCategory::Network,
            label: "IPv4",
            control: ConfigControlKind::Bool,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::NetworkIpv6Enabled,
            category: ConfigCategory::Network,
            label: "IPv6",
            control: ConfigControlKind::Bool,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::NetworkIpv4Address,
            category: ConfigCategory::Network,
            label: "Exact IPv4 Address",
            control: ConfigControlKind::Text,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::NetworkIpv6Address,
            category: ConfigCategory::Network,
            label: "Exact IPv6 Address",
            control: ConfigControlKind::Text,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::NetworkDnsPolicy,
            category: ConfigCategory::Network,
            label: "DNS Policy",
            control: ConfigControlKind::Enum,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::NetworkDnsServers,
            category: ConfigCategory::Network,
            label: "Bound DNS Servers",
            control: ConfigControlKind::Text,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::DefaultDownloadFolder,
            category: ConfigCategory::Paths,
            label: "Default Download Folder",
            control: ConfigControlKind::Path,
            scope: ConfigScope::Shared,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::WatchFolder,
            category: ConfigCategory::Paths,
            label: "Torrent Watch Folder",
            control: ConfigControlKind::Path,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::UiLayoutMode,
            category: ConfigCategory::Ui,
            label: "Layout",
            control: ConfigControlKind::Enum,
            scope: ConfigScope::Shared,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::AlwaysShowAddLocationPrompt,
            category: ConfigCategory::Downloads,
            label: "Confirm Add Priority And Location",
            control: ConfigControlKind::Bool,
            scope: ConfigScope::Host,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::GlobalDownloadLimit,
            category: ConfigCategory::Downloads,
            label: "Global Download Limit",
            control: ConfigControlKind::RateLimit,
            scope: ConfigScope::Shared,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::GlobalUploadLimit,
            category: ConfigCategory::Downloads,
            label: "Global Upload Limit",
            control: ConfigControlKind::RateLimit,
            scope: ConfigScope::Shared,
        },
    ]
}

fn descriptor_for_item(item: ConfigItem) -> &'static ConfigSettingDescriptor {
    config_setting_descriptors()
        .iter()
        .find(|descriptor| descriptor.item == item)
        .expect("all config items must have descriptors")
}

fn config_category_for_item(item: ConfigItem) -> ConfigCategory {
    descriptor_for_item(item).category
}

fn selected_item(items: &[ConfigItem], selected_index: usize) -> ConfigItem {
    items
        .get(selected_index)
        .copied()
        .unwrap_or(ConfigItem::ClientPort)
}

fn enabled_label(enabled: bool) -> String {
    if enabled { "Enabled" } else { "Disabled" }.to_string()
}

fn network_binding_mode_label(mode: NetworkBindingMode) -> String {
    match mode {
        NetworkBindingMode::Any => "Normal (automatic)",
        NetworkBindingMode::Interface => "Bind to interface",
        NetworkBindingMode::LocalAddress => "Use local address",
    }
    .to_string()
}

fn next_network_binding_mode(mode: NetworkBindingMode) -> NetworkBindingMode {
    match mode {
        NetworkBindingMode::Any => NetworkBindingMode::Interface,
        NetworkBindingMode::Interface => NetworkBindingMode::LocalAddress,
        NetworkBindingMode::LocalAddress => NetworkBindingMode::Any,
    }
}

fn previous_network_binding_mode(mode: NetworkBindingMode) -> NetworkBindingMode {
    match mode {
        NetworkBindingMode::Any => NetworkBindingMode::LocalAddress,
        NetworkBindingMode::Interface => NetworkBindingMode::Any,
        NetworkBindingMode::LocalAddress => NetworkBindingMode::Interface,
    }
}

fn set_network_binding_mode(settings: &mut Settings, mode: NetworkBindingMode) {
    settings.network_binding.mode = mode;
    match mode {
        NetworkBindingMode::Any => {
            settings.network_binding.dns_policy = DnsPolicy::System;
        }
        NetworkBindingMode::LocalAddress => {
            settings.network_binding.interface = None;
        }
        NetworkBindingMode::Interface => {}
    }
}

fn selectable_network_interfaces() -> Vec<NetworkInterfaceInfo> {
    available_network_interfaces()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| {
            interface.is_up
                && !interface.is_loopback
                && (!interface.ipv4_addresses.is_empty() || !interface.ipv6_addresses.is_empty())
        })
        .collect()
}

fn cycled_network_interface_name(
    current: Option<&str>,
    interfaces: &[NetworkInterfaceInfo],
    forward: bool,
) -> Option<String> {
    if interfaces.is_empty() {
        return None;
    }
    let current_index = current.and_then(|current| {
        interfaces
            .iter()
            .position(|interface| interface.name == current)
    });
    let next_index = match (current_index, forward) {
        (Some(index), true) => (index + 1) % interfaces.len(),
        (Some(index), false) => index.checked_sub(1).unwrap_or(interfaces.len() - 1),
        (None, true) => 0,
        (None, false) => interfaces.len() - 1,
    };
    Some(interfaces[next_index].name.clone())
}

fn cycle_network_interface(settings: &mut Settings, forward: bool) -> bool {
    let interfaces = selectable_network_interfaces();
    let next = cycled_network_interface_name(
        settings.network_binding.interface.as_deref(),
        &interfaces,
        forward,
    );
    let changed = next.is_some() && settings.network_binding.interface != next;
    if changed {
        settings.network_binding.interface = next;
    }
    changed
}

fn dns_policy_label(policy: DnsPolicy) -> String {
    match policy {
        DnsPolicy::System => "System DNS",
        DnsPolicy::Bound => "Bound DNS",
    }
    .to_string()
}

fn next_dns_policy(policy: DnsPolicy) -> DnsPolicy {
    match policy {
        DnsPolicy::System => DnsPolicy::Bound,
        DnsPolicy::Bound => DnsPolicy::System,
    }
}

fn value_for_item(item: ConfigItem, settings: &Settings) -> String {
    match item {
        ConfigItem::ClientPort if settings.randomize_client_port => {
            format!("Random ({})", settings.client_port)
        }
        ConfigItem::ClientPort => settings.client_port.to_string(),
        ConfigItem::NetworkBindingMode => network_binding_mode_label(settings.network_binding.mode),
        ConfigItem::NetworkInterface => settings
            .network_binding
            .interface
            .clone()
            .unwrap_or_default(),
        ConfigItem::NetworkIpv4Enabled => enabled_label(settings.network_binding.enable_ipv4),
        ConfigItem::NetworkIpv6Enabled => enabled_label(settings.network_binding.enable_ipv6),
        ConfigItem::NetworkIpv4Address => settings
            .network_binding
            .ipv4_address
            .map(|address| address.to_string())
            .unwrap_or_default(),
        ConfigItem::NetworkIpv6Address => settings
            .network_binding
            .ipv6_address
            .map(|address| address.to_string())
            .unwrap_or_default(),
        ConfigItem::NetworkDnsPolicy => dns_policy_label(settings.network_binding.dns_policy),
        ConfigItem::NetworkDnsServers => settings
            .network_binding
            .dns_servers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        ConfigItem::DefaultDownloadFolder => {
            path_to_string(settings.default_download_folder.as_deref())
        }
        ConfigItem::WatchFolder => path_to_string(settings.watch_folder.as_deref()),
        ConfigItem::AlwaysShowAddLocationPrompt => {
            if settings.always_show_add_location_prompt {
                "Enabled".to_string()
            } else {
                "Disabled".to_string()
            }
        }
        ConfigItem::UiLayoutMode => settings.ui_layout_mode.label().to_string(),
        ConfigItem::GlobalDownloadLimit => format_limit_bps(settings.global_download_limit_bps),
        ConfigItem::GlobalUploadLimit => format_limit_bps(settings.global_upload_limit_bps),
    }
}

fn config_item_contains_private_value(item: ConfigItem) -> bool {
    matches!(
        item,
        ConfigItem::ClientPort
            | ConfigItem::NetworkInterface
            | ConfigItem::NetworkIpv4Address
            | ConfigItem::NetworkIpv6Address
            | ConfigItem::NetworkDnsServers
            | ConfigItem::DefaultDownloadFolder
            | ConfigItem::WatchFolder
    )
}

fn anonymize_config_value(item: ConfigItem, value: String, anonymize: bool) -> String {
    if anonymize
        && config_item_contains_private_value(item)
        && !value.is_empty()
        && value != "Not Set"
    {
        "Anonymized".to_string()
    } else {
        value
    }
}

fn config_item_is_dirty(item: ConfigItem, draft: &Settings, applied: &Settings) -> bool {
    match item {
        ConfigItem::ClientPort => {
            draft.client_port != applied.client_port
                || draft.randomize_client_port != applied.randomize_client_port
        }
        ConfigItem::NetworkBindingMode => {
            draft.network_binding.mode != applied.network_binding.mode
        }
        ConfigItem::NetworkInterface => {
            draft.network_binding.interface != applied.network_binding.interface
        }
        ConfigItem::NetworkIpv4Enabled => {
            draft.network_binding.enable_ipv4 != applied.network_binding.enable_ipv4
        }
        ConfigItem::NetworkIpv6Enabled => {
            draft.network_binding.enable_ipv6 != applied.network_binding.enable_ipv6
        }
        ConfigItem::NetworkIpv4Address => {
            draft.network_binding.ipv4_address != applied.network_binding.ipv4_address
        }
        ConfigItem::NetworkIpv6Address => {
            draft.network_binding.ipv6_address != applied.network_binding.ipv6_address
        }
        ConfigItem::NetworkDnsPolicy => {
            draft.network_binding.dns_policy != applied.network_binding.dns_policy
        }
        ConfigItem::NetworkDnsServers => {
            draft.network_binding.dns_servers != applied.network_binding.dns_servers
        }
        ConfigItem::DefaultDownloadFolder => {
            draft.default_download_folder != applied.default_download_folder
        }
        ConfigItem::WatchFolder => draft.watch_folder != applied.watch_folder,
        ConfigItem::AlwaysShowAddLocationPrompt => {
            draft.always_show_add_location_prompt != applied.always_show_add_location_prompt
        }
        ConfigItem::UiLayoutMode => draft.ui_layout_mode != applied.ui_layout_mode,
        ConfigItem::GlobalDownloadLimit => {
            draft.global_download_limit_bps != applied.global_download_limit_bps
        }
        ConfigItem::GlobalUploadLimit => {
            draft.global_upload_limit_bps != applied.global_upload_limit_bps
        }
    }
}

fn config_item_is_locked(item: ConfigItem, shared_follower: bool) -> bool {
    let descriptor = descriptor_for_item(item);
    (shared_follower && descriptor.scope == ConfigScope::Shared) || shared_path_is_manual(item)
}

fn config_scope_label(scope: ConfigScope) -> &'static str {
    if crate::config::is_shared_config_mode() {
        match scope {
            ConfigScope::Host => "HOST",
            ConfigScope::Shared => "SHARED",
        }
    } else {
        "LOCAL"
    }
}

pub(crate) fn merge_config_item_into_current(
    draft: &Settings,
    current: &Settings,
    item: ConfigItem,
    shared_follower: bool,
) -> Settings {
    let mut update = current.clone();
    if config_item_is_locked(item, shared_follower) {
        return update;
    }
    match item {
        ConfigItem::ClientPort => {
            update.client_port = draft.client_port;
            update.randomize_client_port = draft.randomize_client_port;
        }
        ConfigItem::NetworkBindingMode => {
            update.network_binding.mode = draft.network_binding.mode;
            match draft.network_binding.mode {
                NetworkBindingMode::Any => {
                    update.network_binding.dns_policy = DnsPolicy::System;
                }
                NetworkBindingMode::LocalAddress => {
                    update.network_binding.interface = None;
                }
                NetworkBindingMode::Interface => {}
            }
        }
        ConfigItem::NetworkInterface => {
            update.network_binding.interface = draft.network_binding.interface.clone();
        }
        ConfigItem::NetworkIpv4Enabled => {
            update.network_binding.enable_ipv4 = draft.network_binding.enable_ipv4;
        }
        ConfigItem::NetworkIpv6Enabled => {
            update.network_binding.enable_ipv6 = draft.network_binding.enable_ipv6;
        }
        ConfigItem::NetworkIpv4Address => {
            update.network_binding.ipv4_address = draft.network_binding.ipv4_address;
        }
        ConfigItem::NetworkIpv6Address => {
            update.network_binding.ipv6_address = draft.network_binding.ipv6_address;
        }
        ConfigItem::NetworkDnsPolicy => {
            update.network_binding.dns_policy = draft.network_binding.dns_policy;
        }
        ConfigItem::NetworkDnsServers => {
            update.network_binding.dns_servers = draft.network_binding.dns_servers.clone();
        }
        ConfigItem::DefaultDownloadFolder => {
            update.default_download_folder = draft.default_download_folder.clone();
        }
        ConfigItem::WatchFolder => update.watch_folder = draft.watch_folder.clone(),
        ConfigItem::UiLayoutMode => update.ui_layout_mode = draft.ui_layout_mode,
        ConfigItem::AlwaysShowAddLocationPrompt => {
            update.always_show_add_location_prompt = draft.always_show_add_location_prompt;
        }
        ConfigItem::GlobalDownloadLimit => {
            update.global_download_limit_bps = draft.global_download_limit_bps;
        }
        ConfigItem::GlobalUploadLimit => {
            update.global_upload_limit_bps = draft.global_upload_limit_bps;
        }
    }
    update
}

fn shared_path_is_manual(item: ConfigItem) -> bool {
    crate::config::is_shared_config_mode() && item == ConfigItem::DefaultDownloadFolder
}

fn parse_rate_limit_input(input: &str) -> Option<u64> {
    let normalized = input.trim().to_ascii_lowercase();
    if matches!(normalized.as_str(), "unlimited" | "none" | "off") {
        return Some(UNLIMITED_RATE_LIMIT_BPS);
    }

    let compact = normalized.replace(' ', "");
    let number_end = compact
        .char_indices()
        .find_map(|(index, character)| {
            (!character.is_ascii_digit() && character != '.').then_some(index)
        })
        .unwrap_or(compact.len());
    let number = compact.get(..number_end)?.parse::<f64>().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    if number == 0.0 {
        return Some(UNLIMITED_RATE_LIMIT_BPS);
    }

    let multiplier = match compact.get(number_end..)? {
        "" | "bps" | "bit/s" | "bits/s" => 1.0,
        "k" | "kbps" | "kbit/s" | "kbits/s" => 1_000.0,
        "m" | "mbps" | "mbit/s" | "mbits/s" => 1_000_000.0,
        "g" | "gbps" | "gbit/s" | "gbits/s" => 1_000_000_000.0,
        "t" | "tbps" | "tbit/s" | "tbits/s" => 1_000_000_000_000.0,
        "p" | "pbps" | "pbit/s" | "pbits/s" => 1_000_000_000_000_000.0,
        "e" | "ebps" | "ebit/s" | "ebits/s" => 1_000_000_000_000_000_000.0,
        _ => return None,
    };
    let value = number * multiplier;
    if value > u64::MAX as f64 {
        return None;
    }
    let value = value.round() as u64;
    Some(if crate::config::is_unlimited_rate_limit_bps(value) {
        UNLIMITED_RATE_LIMIT_BPS
    } else {
        value
    })
}

fn edit_character_allowed(item: ConfigItem, character: char) -> bool {
    match item {
        ConfigItem::ClientPort => {
            character.is_ascii_digit() || "random".contains(character.to_ascii_lowercase())
        }
        ConfigItem::NetworkInterface => !character.is_control() && !character.is_whitespace(),
        ConfigItem::NetworkIpv4Address => character.is_ascii_digit() || character == '.',
        ConfigItem::NetworkIpv6Address => {
            character.is_ascii_hexdigit() || matches!(character, ':' | '.')
        }
        ConfigItem::NetworkDnsServers => {
            character.is_ascii_hexdigit()
                || character.is_ascii_whitespace()
                || matches!(character, '.' | ':' | ',' | '[' | ']')
        }
        ConfigItem::GlobalDownloadLimit | ConfigItem::GlobalUploadLimit => {
            character.is_ascii_alphanumeric() || matches!(character, '.' | ' ' | '/')
        }
        _ => false,
    }
}

fn insert_editor_character(editor: &mut ConfigEditState, character: char) {
    if editor.select_all {
        editor.buffer.clear();
        editor.cursor = 0;
        editor.select_all = false;
    }
    editor.buffer.insert(editor.cursor, character);
    editor.cursor += character.len_utf8();
}

fn map_key_to_config_action(
    key_code: KeyCode,
    editing: &Option<ConfigEditState>,
) -> Option<ConfigAction> {
    if editing.is_some() {
        let item = editing.as_ref().map(|editor| editor.item);
        return match key_code {
            KeyCode::Char(c) if item.is_some_and(|item| edit_character_allowed(item, c)) => {
                Some(ConfigAction::EditInsert(c))
            }
            KeyCode::Backspace => Some(ConfigAction::EditBackspace),
            KeyCode::Delete => Some(ConfigAction::EditDelete),
            KeyCode::Left => Some(ConfigAction::EditMoveLeft),
            KeyCode::Right => Some(ConfigAction::EditMoveRight),
            KeyCode::Home => Some(ConfigAction::EditMoveHome),
            KeyCode::End => Some(ConfigAction::EditMoveEnd),
            KeyCode::Esc => Some(ConfigAction::EditCancel),
            KeyCode::Enter => Some(ConfigAction::EditCommit),
            _ => None,
        };
    }

    match key_code {
        KeyCode::Esc | KeyCode::Char('q' | 'Q') => Some(ConfigAction::Exit),
        KeyCode::Char('x') => Some(ConfigAction::ToggleAnonymize),
        KeyCode::Char(' ') => Some(ConfigAction::ShiftSelected),
        KeyCode::Char('t') => Some(ConfigAction::SetSelectedBool(true)),
        KeyCode::Char('f') => Some(ConfigAction::SetSelectedBool(false)),
        KeyCode::Up | KeyCode::Char('k') => Some(ConfigAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(ConfigAction::MoveDown),
        KeyCode::Char('r') => Some(ConfigAction::RequestReset),
        KeyCode::Right | KeyCode::Char('l') => Some(ConfigAction::IncreaseSelected),
        KeyCode::Left | KeyCode::Char('h') => Some(ConfigAction::DecreaseSelected),
        _ => None,
    }
}

fn path_browser_effect(
    settings_edit: &Settings,
    selected_index: usize,
    items: &[ConfigItem],
    selected_item: ConfigItem,
) -> Option<ConfigEffect> {
    if !matches!(
        selected_item,
        ConfigItem::DefaultDownloadFolder | ConfigItem::WatchFolder
    ) || shared_path_is_manual(selected_item)
    {
        return None;
    }

    let initial_path = if selected_item == ConfigItem::WatchFolder {
        settings_edit.watch_folder.clone()
    } else {
        settings_edit.default_download_folder.clone()
    }
    .unwrap_or_else(|| {
        UserDirs::new()
            .and_then(|user_dirs| user_dirs.download_dir().map(|path| path.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    });

    Some(ConfigEffect::AppCommand(Box::new(
        AppCommand::FetchFileTree {
            browser_generation: 0,
            path: initial_path,
            browser_mode: FileBrowserMode::ConfigPathSelection {
                target_item: selected_item,
                current_settings: Box::new(settings_edit.clone()),
                selected_index,
                items: items.to_vec(),
            },
            preserve_browser_mode: false,
            highlight_path: None,
        },
    )))
}

pub fn reduce_config_action(
    action: ConfigAction,
    settings_edit: &mut Box<Settings>,
    selected_index: &mut usize,
    items: &mut [ConfigItem],
    editing: &mut Option<ConfigEditState>,
) -> ConfigReduceResult {
    let mut result = ConfigReduceResult::default();
    match action {
        ConfigAction::Exit => {
            result.consumed = true;
        }
        ConfigAction::ToggleAnonymize => {
            result.consumed = true;
        }
        ConfigAction::ShiftSelected => {
            result.consumed = true;
            match items[*selected_index] {
                ConfigItem::DefaultDownloadFolder | ConfigItem::WatchFolder => {
                    let selected_item = items[*selected_index];
                    if let Some(effect) =
                        path_browser_effect(settings_edit, *selected_index, items, selected_item)
                    {
                        result.effects.push(effect);
                    }
                }
                ConfigItem::ClientPort => {
                    let buffer = if settings_edit.randomize_client_port {
                        "RANDOM".to_string()
                    } else {
                        settings_edit.client_port.to_string()
                    };
                    *editing = Some(ConfigEditState {
                        cursor: buffer.len(),
                        buffer,
                        item: ConfigItem::ClientPort,
                        select_all: true,
                    });
                }
                ConfigItem::NetworkIpv4Address
                | ConfigItem::NetworkIpv6Address
                | ConfigItem::NetworkDnsServers => {
                    let item = items[*selected_index];
                    let buffer = value_for_item(item, settings_edit);
                    *editing = Some(ConfigEditState {
                        cursor: buffer.len(),
                        buffer,
                        item,
                        select_all: true,
                    });
                }
                ConfigItem::NetworkBindingMode => {
                    let next_mode = next_network_binding_mode(settings_edit.network_binding.mode);
                    set_network_binding_mode(settings_edit, next_mode);
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkInterface => {
                    if cycle_network_interface(settings_edit, true) {
                        result.effects.push(ConfigEffect::ApplySettings);
                    }
                }
                ConfigItem::NetworkDnsPolicy => {
                    settings_edit.network_binding.dns_policy =
                        next_dns_policy(settings_edit.network_binding.dns_policy);
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkIpv4Enabled => {
                    settings_edit.network_binding.enable_ipv4 =
                        !settings_edit.network_binding.enable_ipv4;
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkIpv6Enabled => {
                    settings_edit.network_binding.enable_ipv6 =
                        !settings_edit.network_binding.enable_ipv6;
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::AlwaysShowAddLocationPrompt => {
                    settings_edit.always_show_add_location_prompt =
                        !settings_edit.always_show_add_location_prompt;
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::UiLayoutMode => {
                    settings_edit.ui_layout_mode = settings_edit.ui_layout_mode.next();
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::GlobalDownloadLimit | ConfigItem::GlobalUploadLimit => {
                    let item = items[*selected_index];
                    let buffer = if item == ConfigItem::GlobalDownloadLimit {
                        format_limit_bps(settings_edit.global_download_limit_bps)
                    } else {
                        format_limit_bps(settings_edit.global_upload_limit_bps)
                    };
                    *editing = Some(ConfigEditState {
                        cursor: buffer.len(),
                        buffer,
                        item,
                        select_all: true,
                    });
                }
            }
        }
        ConfigAction::SetSelectedBool(value) => {
            result.consumed = true;
            match items[*selected_index] {
                ConfigItem::AlwaysShowAddLocationPrompt
                    if settings_edit.always_show_add_location_prompt != value =>
                {
                    settings_edit.always_show_add_location_prompt = value;
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkIpv4Enabled
                    if settings_edit.network_binding.enable_ipv4 != value =>
                {
                    settings_edit.network_binding.enable_ipv4 = value;
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkIpv6Enabled
                    if settings_edit.network_binding.enable_ipv6 != value =>
                {
                    settings_edit.network_binding.enable_ipv6 = value;
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                _ => {}
            }
        }
        ConfigAction::MoveUp => {
            result.consumed = true;
            *selected_index = previous_visible_setting_index(items, settings_edit, *selected_index);
        }
        ConfigAction::MoveDown => {
            result.consumed = true;
            *selected_index = next_visible_setting_index(items, settings_edit, *selected_index);
        }
        ConfigAction::RequestReset => {
            result.consumed = true;
        }
        ConfigAction::ResetSelected => {
            result.consumed = true;
            let default_settings = Settings::default();
            let selected_item = items[*selected_index];
            let changed = config_item_is_dirty(selected_item, settings_edit, &default_settings);
            let mut can_apply = true;
            match selected_item {
                ConfigItem::ClientPort => {
                    settings_edit.client_port = default_settings.client_port;
                    settings_edit.randomize_client_port = false;
                }
                ConfigItem::NetworkBindingMode => {
                    set_network_binding_mode(settings_edit, default_settings.network_binding.mode);
                }
                ConfigItem::NetworkInterface => {
                    settings_edit.network_binding.interface =
                        default_settings.network_binding.interface;
                }
                ConfigItem::NetworkIpv4Enabled => {
                    settings_edit.network_binding.enable_ipv4 =
                        default_settings.network_binding.enable_ipv4;
                }
                ConfigItem::NetworkIpv6Enabled => {
                    settings_edit.network_binding.enable_ipv6 =
                        default_settings.network_binding.enable_ipv6;
                }
                ConfigItem::NetworkIpv4Address => {
                    settings_edit.network_binding.ipv4_address =
                        default_settings.network_binding.ipv4_address;
                }
                ConfigItem::NetworkIpv6Address => {
                    settings_edit.network_binding.ipv6_address =
                        default_settings.network_binding.ipv6_address;
                }
                ConfigItem::NetworkDnsPolicy => {
                    settings_edit.network_binding.dns_policy =
                        default_settings.network_binding.dns_policy;
                }
                ConfigItem::NetworkDnsServers => {
                    settings_edit.network_binding.dns_servers =
                        default_settings.network_binding.dns_servers;
                }
                ConfigItem::DefaultDownloadFolder => {
                    if !shared_path_is_manual(selected_item) {
                        settings_edit.default_download_folder =
                            default_settings.default_download_folder;
                    } else {
                        can_apply = false;
                    }
                }
                ConfigItem::WatchFolder => {
                    settings_edit.watch_folder = default_settings.watch_folder;
                }
                ConfigItem::AlwaysShowAddLocationPrompt => {
                    settings_edit.always_show_add_location_prompt =
                        default_settings.always_show_add_location_prompt;
                }
                ConfigItem::UiLayoutMode => {
                    settings_edit.ui_layout_mode = default_settings.ui_layout_mode;
                }
                ConfigItem::GlobalDownloadLimit => {
                    settings_edit.global_download_limit_bps =
                        default_settings.global_download_limit_bps;
                }
                ConfigItem::GlobalUploadLimit => {
                    settings_edit.global_upload_limit_bps =
                        default_settings.global_upload_limit_bps;
                }
            }
            if changed && can_apply {
                result.effects.push(ConfigEffect::ApplySettings);
            }
        }
        ConfigAction::IncreaseSelected => {
            result.consumed = true;
            match items[*selected_index] {
                ConfigItem::UiLayoutMode => {
                    settings_edit.ui_layout_mode = settings_edit.ui_layout_mode.next();
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkInterface if cycle_network_interface(settings_edit, true) => {
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkBindingMode => {
                    let next_mode = next_network_binding_mode(settings_edit.network_binding.mode);
                    set_network_binding_mode(settings_edit, next_mode);
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkDnsPolicy => {
                    settings_edit.network_binding.dns_policy =
                        next_dns_policy(settings_edit.network_binding.dns_policy);
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                _ => {}
            }
        }
        ConfigAction::DecreaseSelected => {
            result.consumed = true;
            match items[*selected_index] {
                ConfigItem::UiLayoutMode => {
                    settings_edit.ui_layout_mode = settings_edit.ui_layout_mode.previous();
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkBindingMode => {
                    let previous_mode =
                        previous_network_binding_mode(settings_edit.network_binding.mode);
                    set_network_binding_mode(settings_edit, previous_mode);
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkInterface if cycle_network_interface(settings_edit, false) => {
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::NetworkDnsPolicy => {
                    settings_edit.network_binding.dns_policy =
                        next_dns_policy(settings_edit.network_binding.dns_policy);
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                _ => {}
            }
        }
        ConfigAction::EditInsert(c) => {
            result.consumed = true;
            if let Some(editor) = editing {
                insert_editor_character(editor, c);
            }
        }
        ConfigAction::EditBackspace => {
            result.consumed = true;
            if let Some(editor) = editing {
                if editor.select_all {
                    editor.buffer.clear();
                    editor.cursor = 0;
                    editor.select_all = false;
                } else if editor.cursor > 0 {
                    let previous = editor.buffer[..editor.cursor]
                        .char_indices()
                        .next_back()
                        .map(|(index, _)| index)
                        .unwrap_or(0);
                    editor.buffer.drain(previous..editor.cursor);
                    editor.cursor = previous;
                }
            }
        }
        ConfigAction::EditDelete => {
            result.consumed = true;
            if let Some(editor) = editing {
                if editor.select_all {
                    editor.buffer.clear();
                    editor.cursor = 0;
                    editor.select_all = false;
                } else if editor.cursor < editor.buffer.len() {
                    let next = editor.buffer[editor.cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(offset, _)| editor.cursor + offset)
                        .unwrap_or(editor.buffer.len());
                    editor.buffer.drain(editor.cursor..next);
                }
            }
        }
        ConfigAction::EditMoveLeft => {
            result.consumed = true;
            if let Some(editor) = editing {
                if editor.select_all {
                    editor.cursor = 0;
                    editor.select_all = false;
                } else if editor.cursor > 0 {
                    editor.cursor = editor.buffer[..editor.cursor]
                        .char_indices()
                        .next_back()
                        .map(|(index, _)| index)
                        .unwrap_or(0);
                }
            }
        }
        ConfigAction::EditMoveRight => {
            result.consumed = true;
            if let Some(editor) = editing {
                if editor.select_all {
                    editor.cursor = editor.buffer.len();
                    editor.select_all = false;
                } else if editor.cursor < editor.buffer.len() {
                    editor.cursor = editor.buffer[editor.cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(offset, _)| editor.cursor + offset)
                        .unwrap_or(editor.buffer.len());
                }
            }
        }
        ConfigAction::EditMoveHome => {
            result.consumed = true;
            if let Some(editor) = editing {
                editor.cursor = 0;
                editor.select_all = false;
            }
        }
        ConfigAction::EditMoveEnd => {
            result.consumed = true;
            if let Some(editor) = editing {
                editor.cursor = editor.buffer.len();
                editor.select_all = false;
            }
        }
        ConfigAction::EditCancel => {
            result.consumed = true;
            *editing = None;
        }
        ConfigAction::EditCommit => {
            result.consumed = true;
            if let Some(editor) = editing {
                let mut committed = false;
                let mut changed = false;
                match editor.item {
                    ConfigItem::ClientPort => {
                        if is_random_port_input(&editor.buffer) {
                            changed = !settings_edit.randomize_client_port;
                            settings_edit.randomize_client_port = true;
                            committed = true;
                        } else if let Ok(new_port) = editor.buffer.parse::<u16>() {
                            if new_port > 0 {
                                changed = settings_edit.client_port != new_port
                                    || settings_edit.randomize_client_port;
                                settings_edit.client_port = new_port;
                                settings_edit.randomize_client_port = false;
                                committed = true;
                            }
                        }
                    }
                    ConfigItem::NetworkInterface => {
                        let interface = editor.buffer.trim();
                        let new_interface = (!interface.is_empty()).then(|| interface.to_string());
                        changed = settings_edit.network_binding.interface != new_interface;
                        settings_edit.network_binding.interface = new_interface;
                        committed = true;
                    }
                    ConfigItem::NetworkIpv4Address => {
                        let input = editor.buffer.trim();
                        if input.is_empty() {
                            changed = settings_edit.network_binding.ipv4_address.is_some();
                            settings_edit.network_binding.ipv4_address = None;
                            committed = true;
                        } else if let Ok(address) = input.parse() {
                            changed = settings_edit.network_binding.ipv4_address != Some(address);
                            settings_edit.network_binding.ipv4_address = Some(address);
                            committed = true;
                        }
                    }
                    ConfigItem::NetworkIpv6Address => {
                        let input = editor.buffer.trim();
                        if input.is_empty() {
                            changed = settings_edit.network_binding.ipv6_address.is_some();
                            settings_edit.network_binding.ipv6_address = None;
                            committed = true;
                        } else if let Ok(address) = input.parse() {
                            changed = settings_edit.network_binding.ipv6_address != Some(address);
                            settings_edit.network_binding.ipv6_address = Some(address);
                            committed = true;
                        }
                    }
                    ConfigItem::NetworkDnsServers => {
                        let parsed = editor
                            .buffer
                            .split(|character: char| character == ',' || character.is_whitespace())
                            .filter(|value| !value.is_empty())
                            .map(str::parse)
                            .collect::<Result<Vec<_>, _>>();
                        if let Ok(servers) = parsed {
                            changed = settings_edit.network_binding.dns_servers != servers;
                            settings_edit.network_binding.dns_servers = servers;
                            committed = true;
                        }
                    }
                    ConfigItem::GlobalDownloadLimit => {
                        if let Some(new_rate) = parse_rate_limit_input(&editor.buffer) {
                            changed = settings_edit.global_download_limit_bps != new_rate;
                            settings_edit.global_download_limit_bps = new_rate;
                            committed = true;
                        }
                    }
                    ConfigItem::GlobalUploadLimit => {
                        if let Some(new_rate) = parse_rate_limit_input(&editor.buffer) {
                            changed = settings_edit.global_upload_limit_bps != new_rate;
                            settings_edit.global_upload_limit_bps = new_rate;
                            committed = true;
                        }
                    }
                    _ => {
                        committed = true;
                    }
                }
                if committed {
                    if changed {
                        result.effects.push(ConfigEffect::ApplySettings);
                    }
                    *editing = None;
                }
            }
        }
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ConfigListRow {
    Category(ConfigCategory),
    Setting {
        global_index: usize,
        item: ConfigItem,
    },
}

fn config_item_is_visible(item: ConfigItem, settings: &Settings) -> bool {
    let binding = &settings.network_binding;
    match item {
        ConfigItem::NetworkInterface => binding.mode == NetworkBindingMode::Interface,
        ConfigItem::NetworkIpv4Enabled
        | ConfigItem::NetworkIpv6Enabled
        | ConfigItem::NetworkIpv4Address
        | ConfigItem::NetworkIpv6Address
        | ConfigItem::NetworkDnsPolicy => binding.mode != NetworkBindingMode::Any,
        ConfigItem::NetworkDnsServers => {
            binding.mode != NetworkBindingMode::Any && binding.dns_policy == DnsPolicy::Bound
        }
        _ => true,
    }
}

fn config_item_is_network_binding_child(item: ConfigItem) -> bool {
    matches!(
        item,
        ConfigItem::NetworkInterface
            | ConfigItem::NetworkIpv4Enabled
            | ConfigItem::NetworkIpv6Enabled
            | ConfigItem::NetworkIpv4Address
            | ConfigItem::NetworkIpv6Address
            | ConfigItem::NetworkDnsPolicy
            | ConfigItem::NetworkDnsServers
    )
}

fn config_list_rows(items: &[ConfigItem], settings: &Settings) -> Vec<ConfigListRow> {
    let mut rows = Vec::new();
    for category in ConfigCategory::all() {
        let category_items = items
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, item)| {
                config_category_for_item(*item) == *category
                    && config_item_is_visible(*item, settings)
            })
            .collect::<Vec<_>>();
        if category_items.is_empty() {
            continue;
        }
        rows.push(ConfigListRow::Category(*category));
        rows.extend(
            category_items
                .into_iter()
                .map(|(global_index, item)| ConfigListRow::Setting { global_index, item }),
        );
    }
    rows
}

fn visible_setting_indices(items: &[ConfigItem], settings: &Settings) -> Vec<usize> {
    config_list_rows(items, settings)
        .into_iter()
        .filter_map(|row| match row {
            ConfigListRow::Setting { global_index, .. } => Some(global_index),
            ConfigListRow::Category(_) => None,
        })
        .collect()
}

fn normalized_visible_setting_index(
    items: &[ConfigItem],
    settings: &Settings,
    selected_index: usize,
) -> usize {
    let visible_indices = visible_setting_indices(items, settings);
    if visible_indices.contains(&selected_index) {
        selected_index
    } else {
        visible_indices
            .iter()
            .copied()
            .rfind(|index| *index < selected_index)
            .or_else(|| visible_indices.first().copied())
            .unwrap_or(0)
    }
}

fn next_visible_setting_index(
    items: &[ConfigItem],
    settings: &Settings,
    selected_index: usize,
) -> usize {
    let visible_indices = visible_setting_indices(items, settings);
    visible_indices
        .iter()
        .position(|index| *index == selected_index)
        .and_then(|position| visible_indices.get(position + 1).copied())
        .unwrap_or(selected_index)
}

fn previous_visible_setting_index(
    items: &[ConfigItem],
    settings: &Settings,
    selected_index: usize,
) -> usize {
    let visible_indices = visible_setting_indices(items, settings);
    visible_indices
        .iter()
        .position(|index| *index == selected_index)
        .and_then(|position| position.checked_sub(1))
        .and_then(|position| visible_indices.get(position).copied())
        .unwrap_or(selected_index)
}

struct ConfigRenderContext<'a, 'b> {
    screen: &'a ScreenContext<'b>,
    settings: &'a Settings,
    editing: &'a Option<ConfigEditState>,
    layout_kind: ConfigLayoutKind,
    terminal_area: Rect,
    shared_follower: bool,
}

fn config_pane_padding(horizontal: u16, area: Rect, pad_top: bool) -> Padding {
    let vertical = u16::from(area.height >= 15);
    Padding::new(
        horizontal,
        horizontal,
        if pad_top { vertical } else { 0 },
        vertical,
    )
}

pub struct ConfigDrawState<'a> {
    pub settings: &'a Settings,
    pub selected_index: usize,
    pub items: &'a [ConfigItem],
    pub active_pane: ConfigPane,
    pub editing: &'a Option<ConfigEditState>,
    pub reset_confirmation: &'a Option<ConfigItem>,
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>, state: ConfigDrawState<'_>) {
    let ConfigDrawState {
        settings,
        selected_index,
        items,
        active_pane,
        editing,
        reset_confirmation,
    } = state;
    let plan = calculate_config_layout(f.area(), settings.ui_layout_mode);
    f.render_widget(Clear, f.area());

    let selected_index = normalized_visible_setting_index(items, settings, selected_index);
    let active_item = selected_item(items, selected_index);
    let active_descriptor = descriptor_for_item(active_item);
    let shared_follower = crate::config::is_shared_config_mode()
        && screen.ui.cluster_role_label.as_deref() == Some("Follower");
    let render_ctx = ConfigRenderContext {
        screen,
        settings,
        editing,
        layout_kind: plan.kind,
        terminal_area: f.area(),
        shared_follower,
    };

    if plan.kind == ConfigLayoutKind::Compact {
        match active_pane {
            ConfigPane::Settings => render_settings_pane(
                f,
                &render_ctx,
                items,
                selected_index,
                plan.content_area,
                true,
            ),
            ConfigPane::Details => render_details_pane(
                f,
                &render_ctx,
                active_item,
                active_descriptor,
                plan.content_area,
                true,
            ),
        }
    } else {
        render_settings_pane(f, &render_ctx, items, selected_index, plan.list_pane, true);
        render_details_pane(
            f,
            &render_ctx,
            active_item,
            active_descriptor,
            plan.details_pane,
            false,
        );
    }

    render_config_footer(f, &render_ctx, active_item, active_pane, plan.footer_area);
    if let Some(item) = reset_confirmation {
        render_reset_confirmation_dialog(f, &render_ctx, *item);
    }
}

fn render_settings_pane(
    f: &mut Frame,
    render_ctx: &ConfigRenderContext<'_, '_>,
    items: &[ConfigItem],
    selected_index: usize,
    area: ratatui::layout::Rect,
    focused: bool,
) {
    let ctx = render_ctx.screen.theme;
    let editing = render_ctx.editing;
    let border_style = if focused {
        ctx.apply(Style::default().fg(ctx.state_selected()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.border))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(config_pane_padding(1, area, false))
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows_model = config_list_rows(items, render_ctx.settings);
    let (start, end) = config_list_viewport(&rows_model, selected_index, inner.height as usize);
    let visible_rows = &rows_model[start..end];
    let constraints = vec![Constraint::Length(1); visible_rows.len()];
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (row_model, row_area) in visible_rows.iter().zip(rows.iter()) {
        match row_model {
            ConfigListRow::Category(category) => {
                let line = Line::from(vec![Span::styled(
                    category.label().to_ascii_uppercase(),
                    ctx.apply(Style::default().fg(ctx.accent_sapphire()).bold()),
                )]);
                f.render_widget(Paragraph::new(line), *row_area);
            }
            ConfigListRow::Setting { global_index, item } => {
                let is_highlighted = if let Some(editor) = editing {
                    editor.item == *item
                } else {
                    *global_index == selected_index
                };
                let locked = config_item_is_locked(*item, render_ctx.shared_follower);
                f.render_widget(
                    Paragraph::new(config_setting_row_line(*item, is_highlighted, locked, ctx)),
                    *row_area,
                );
            }
        }
    }
}

fn config_setting_row_line(
    item: ConfigItem,
    is_highlighted: bool,
    locked: bool,
    ctx: &crate::theme::ThemeContext,
) -> Line<'static> {
    let descriptor = descriptor_for_item(item);
    let label_style = if locked {
        ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.text))
    };
    let marker_style = if is_highlighted {
        ctx.apply(Style::default().fg(ctx.state_selected()).bold())
    } else {
        label_style
    };
    let indent = if config_item_is_network_binding_child(item) {
        "  "
    } else {
        ""
    };

    Line::from(vec![
        Span::raw(indent),
        Span::styled(if is_highlighted { "▶" } else { " " }, marker_style),
        Span::raw(" "),
        Span::styled(descriptor.label, label_style),
    ])
}

fn config_list_viewport(
    rows: &[ConfigListRow],
    selected_index: usize,
    visible_height: usize,
) -> (usize, usize) {
    if visible_height == 0 || rows.is_empty() {
        return (0, 0);
    }
    if rows.len() <= visible_height {
        return (0, rows.len());
    }

    let selected_row = rows
        .iter()
        .position(|row| {
            matches!(
                row,
                ConfigListRow::Setting { global_index, .. } if *global_index == selected_index
            )
        })
        .unwrap_or(0);
    let category_start = rows[..=selected_row]
        .iter()
        .rposition(|row| matches!(row, ConfigListRow::Category(_)))
        .unwrap_or(selected_row);
    let start = if selected_row.saturating_sub(category_start) < visible_height {
        category_start.min(rows.len().saturating_sub(visible_height))
    } else {
        selected_row
            .saturating_sub(visible_height / 2)
            .min(rows.len().saturating_sub(visible_height))
    };
    (start, (start + visible_height).min(rows.len()))
}

fn render_details_pane(
    f: &mut Frame,
    render_ctx: &ConfigRenderContext<'_, '_>,
    active_item: ConfigItem,
    active_descriptor: &ConfigSettingDescriptor,
    area: ratatui::layout::Rect,
    focused: bool,
) {
    let ctx = render_ctx.screen.theme;
    let border_style = if focused {
        ctx.apply(Style::default().fg(ctx.state_selected()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.border))
    };
    let locked = config_item_is_locked(active_item, render_ctx.shared_follower);
    let editing_active = render_ctx
        .editing
        .as_ref()
        .is_some_and(|editor| editor.item == active_item);
    let title_label = if editing_active {
        format!(" Edit {} ", active_descriptor.label)
    } else {
        format!(
            " {} · {} ",
            active_descriptor.label,
            active_descriptor.category.label()
        )
    };
    let mut title_spans = vec![Span::styled(
        title_label,
        ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
    )];
    title_spans.push(Span::styled(
        format!("[{}]", config_scope_label(active_descriptor.scope)),
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
    ));
    if locked {
        title_spans.push(Span::styled(
            "  READ ONLY",
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ));
    }
    let block = Block::default()
        .title(Line::from(title_spans))
        .borders(Borders::ALL)
        .padding(config_pane_padding(
            if area.width >= 44 { 2 } else { 1 },
            area,
            true,
        ))
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = if let Some(editor) = render_ctx.editing {
        if editor.item == active_item {
            build_edit_detail_lines(editor, render_ctx, inner.width)
        } else {
            build_setting_detail_lines(active_item, render_ctx, inner.width)
        }
    } else {
        build_setting_detail_lines(active_item, render_ctx, inner.width)
    };
    if let Some(error) = render_ctx.screen.ui.system_error.as_deref() {
        let error = if render_ctx.screen.ui.anonymize_torrent_names {
            "Details hidden while anonymized".to_string()
        } else {
            error.to_string()
        };
        let status_line = Line::from(vec![
            Span::styled(
                "STATUS  ",
                ctx.apply(Style::default().fg(ctx.state_error()).bold()),
            ),
            Span::styled(
                error,
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
            ),
        ]);
        insert_below_detail_divider(&mut lines, status_line);
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.text))),
        inner,
    );
}

fn build_setting_detail_lines(
    item: ConfigItem,
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    match item {
        ConfigItem::ClientPort => build_port_detail_lines(render_ctx, width),
        ConfigItem::NetworkBindingMode
        | ConfigItem::NetworkInterface
        | ConfigItem::NetworkIpv4Enabled
        | ConfigItem::NetworkIpv6Enabled
        | ConfigItem::NetworkIpv4Address
        | ConfigItem::NetworkIpv6Address
        | ConfigItem::NetworkDnsPolicy
        | ConfigItem::NetworkDnsServers => {
            build_network_binding_detail_lines(item, render_ctx, width)
        }
        ConfigItem::DefaultDownloadFolder | ConfigItem::WatchFolder => {
            build_path_detail_lines(item, render_ctx, width)
        }
        ConfigItem::UiLayoutMode => build_layout_detail_lines(render_ctx, width),
        ConfigItem::AlwaysShowAddLocationPrompt => {
            build_confirm_add_detail_lines(render_ctx, width)
        }
        ConfigItem::GlobalDownloadLimit => build_rate_detail_lines(true, render_ctx, width),
        ConfigItem::GlobalUploadLimit => build_rate_detail_lines(false, render_ctx, width),
    }
}

fn build_network_binding_detail_lines(
    item: ConfigItem,
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let anonymize = render_ctx.screen.ui.anonymize_torrent_names;
    let dirty = config_item_is_dirty(item, render_ctx.settings, render_ctx.screen.settings);
    let mut lines = vec![
        detail_row(
            "Configured",
            anonymize_config_value(item, value_for_item(item, render_ctx.settings), anonymize),
            ctx.apply(
                Style::default()
                    .fg(if dirty {
                        ctx.state_warning()
                    } else {
                        ctx.theme.semantic.text
                    })
                    .bold(),
            ),
            ctx,
        ),
        Line::from(""),
        detail_divider(width, ctx),
        info_note_line(network_setting_description(item), ctx),
    ];

    if item == ConfigItem::NetworkInterface {
        lines.push(Line::from(""));
        lines.push(info_section_heading("DETECTED INTERFACES", ctx));
        lines.extend(discovered_interface_detail_lines(
            render_ctx.settings.network_binding.interface.as_deref(),
            width,
            anonymize,
            ctx,
        ));
    }
    lines.push(Line::from(""));
    lines.push(info_section_heading("LIVE NETWORK", ctx));

    let Some(status) = render_ctx.screen.ui.network_runtime_status.as_ref() else {
        lines.push(detail_row(
            "State",
            "Unavailable".to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ctx,
        ));
        return lines;
    };

    let ready = status.phase == NetworkRuntimePhase::Ready;
    lines.push(detail_row(
        "State",
        if ready { "Ready" } else { "Blocked" }.to_string(),
        ctx.apply(Style::default().fg(if ready {
            ctx.state_success()
        } else {
            ctx.state_error()
        })),
        ctx,
    ));
    lines.push(detail_row(
        "Generation",
        status
            .generation_id
            .map(|generation| {
                format!(
                    "{generation} / epoch {}",
                    status.config_epoch.unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "None".to_string()),
        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
        ctx,
    ));
    lines.push(detail_row(
        "Interface",
        if anonymize && status.interface.is_some() {
            "Anonymized".to_string()
        } else {
            status
                .interface
                .as_deref()
                .map(|name| match status.interface_index {
                    Some(index) => format!("{name} (index {index})"),
                    None => name.to_string(),
                })
                .unwrap_or_else(|| "OS selected".to_string())
        },
        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
        ctx,
    ));
    lines.push(detail_row(
        "IPv4",
        runtime_address_summary(
            status.enable_ipv4,
            status
                .selected_ipv4_address
                .map(|address| address.to_string()),
            status
                .interface_ipv4_addresses
                .iter()
                .map(ToString::to_string)
                .collect(),
            anonymize,
        ),
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ctx,
    ));
    lines.push(detail_row(
        "IPv6",
        runtime_address_summary(
            status.enable_ipv6,
            status
                .selected_ipv6_address
                .map(|address| address.to_string()),
            status
                .interface_ipv6_addresses
                .iter()
                .map(ToString::to_string)
                .collect(),
            anonymize,
        ),
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ctx,
    ));
    if let Some(reason) = status.blocked_reason.as_deref() {
        lines.push(detail_row(
            "Reason",
            if anonymize {
                "Details hidden while anonymized".to_string()
            } else {
                reason.to_string()
            },
            ctx.apply(Style::default().fg(ctx.state_error())),
            ctx,
        ));
    }
    if let Some(warning) = status.warning.as_deref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if anonymize {
                "Warning details hidden while anonymized".to_string()
            } else {
                warning.to_string()
            },
            ctx.apply(Style::default().fg(ctx.state_warning())),
        )));
    }
    lines
}

fn discovered_interface_detail_lines(
    configured: Option<&str>,
    width: u16,
    anonymize: bool,
    ctx: &crate::theme::ThemeContext,
) -> Vec<Line<'static>> {
    discovered_interface_lines(
        selectable_network_interfaces(),
        configured,
        width,
        anonymize,
        ctx,
    )
}

fn discovered_interface_lines(
    mut interfaces: Vec<NetworkInterfaceInfo>,
    configured: Option<&str>,
    width: u16,
    anonymize: bool,
    ctx: &crate::theme::ThemeContext,
) -> Vec<Line<'static>> {
    interfaces.sort_by_key(|interface| interface.name.as_str() != configured.unwrap_or_default());
    if interfaces.is_empty() {
        return vec![Line::from(Span::styled(
            "No active non-loopback interfaces were found. A name can still be set in the host config file.",
            ctx.apply(Style::default().fg(ctx.state_warning())),
        ))];
    }

    let total = interfaces.len();
    let mut lines = interfaces
        .into_iter()
        .take(6)
        .enumerate()
        .map(|(position, interface)| {
            let selected = configured == Some(interface.name.as_str());
            let summary = if anonymize {
                format!("Anonymized interface {}", position + 1)
            } else {
                let addresses = interface
                    .ipv4_addresses
                    .iter()
                    .map(ToString::to_string)
                    .chain(interface.ipv6_addresses.iter().map(ToString::to_string))
                    .collect::<Vec<_>>()
                    .join(", ");
                truncate_with_ellipsis(
                    &format!("{}  #{}  {}", interface.name, interface.index, addresses),
                    width.saturating_sub(3) as usize,
                )
            };
            Line::from(vec![
                Span::styled(
                    if selected { "● " } else { "  " },
                    ctx.apply(Style::default().fg(if selected {
                        ctx.state_success()
                    } else {
                        ctx.theme.semantic.overlay0
                    })),
                ),
                Span::styled(
                    summary,
                    ctx.apply(
                        Style::default()
                            .fg(if selected {
                                ctx.theme.semantic.text
                            } else {
                                ctx.theme.semantic.subtext1
                            })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ),
            ])
        })
        .collect::<Vec<_>>();
    if total > 6 {
        lines.push(Line::from(Span::styled(
            format!("  {} more; use Left/Right to cycle all", total - 6),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        )));
    }
    lines
}

fn runtime_address_summary(
    enabled: bool,
    selected: Option<String>,
    interface_addresses: Vec<String>,
    anonymize: bool,
) -> String {
    if !enabled {
        return "Disabled".to_string();
    }
    if anonymize && (selected.is_some() || !interface_addresses.is_empty()) {
        return "Anonymized".to_string();
    }
    selected
        .or_else(|| (!interface_addresses.is_empty()).then(|| interface_addresses.join(", ")))
        .unwrap_or_else(|| "OS selected".to_string())
}

fn network_setting_description(item: ConfigItem) -> &'static str {
    match item {
        ConfigItem::NetworkBindingMode => {
            "Normal lets the operating system route traffic automatically. Binding modes reveal their additional interface, address-family, and DNS controls."
        }
        ConfigItem::NetworkInterface => {
            "Choose an active interface discovered from the operating system. Space or Left/Right cycles the available choices."
        }
        ConfigItem::NetworkIpv4Enabled | ConfigItem::NetworkIpv6Enabled => {
            "Each enabled address family must be available under the selected strict policy."
        }
        ConfigItem::NetworkIpv4Address | ConfigItem::NetworkIpv6Address => {
            "An optional exact local address further constrains source selection."
        }
        ConfigItem::NetworkDnsPolicy => {
            "Bound DNS uses only generation-owned sockets. System DNS remains outside the strict application leak guarantee."
        }
        ConfigItem::NetworkDnsServers => {
            "Enter literal DNS server socket addresses separated by commas, including port 53."
        }
        _ => "Network binding setting.",
    }
}

fn build_port_detail_lines(
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let anonymize = render_ctx.screen.ui.anonymize_torrent_names;
    let draft = render_ctx.settings.client_port;
    let active = render_ctx.screen.settings.client_port;
    let draft_random = render_ctx.settings.randomize_client_port;
    let active_random = render_ctx.screen.settings.randomize_client_port;
    let dirty = draft != active || draft_random != active_random;
    let value_style = ctx.apply(
        Style::default()
            .fg(if dirty {
                ctx.state_warning()
            } else {
                ctx.theme.semantic.text
            })
            .bold(),
    );
    let configured_value = if anonymize {
        "Anonymized".to_string()
    } else if draft_random {
        "Random each start".to_string()
    } else {
        draft.to_string()
    };
    let mut configured = detail_row("Configured", configured_value, value_style, ctx);
    if dirty {
        configured.spans.push(Span::styled(
            "  UPDATING",
            ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
        ));
    }
    let mut lines = vec![
        configured,
        Line::from(""),
        detail_divider(width, ctx),
        info_note_line("Accepts inbound peer handshakes and DHT traffic.", ctx),
        Line::from(""),
        info_section_heading("LIVE STATUS", ctx),
        detail_row(
            "Runtime",
            if anonymize {
                "Anonymized".to_string()
            } else {
                active.to_string()
            },
            ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
            ctx,
        ),
        inbound_matrix_header(ctx),
        inbound_transport_row(
            "TCP",
            render_ctx.screen.ui.inbound_peer_transports.tcp_ipv4_seen,
            render_ctx.screen.ui.inbound_peer_transports.tcp_ipv6_seen,
            ctx,
        ),
        inbound_transport_row(
            "uTP (UDP)",
            render_ctx.screen.ui.inbound_peer_transports.utp_ipv4_seen,
            render_ctx.screen.ui.inbound_peer_transports.utp_ipv6_seen,
            ctx,
        ),
        detail_row(
            "Default",
            Settings::default().client_port.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ctx,
        ),
    ];
    if dirty {
        lines.push(Line::from(Span::styled(
            "The listener is updating to this port.",
            ctx.apply(Style::default().fg(ctx.state_warning())),
        )));
    }
    lines
}

const INBOUND_MATRIX_COLUMN_WIDTH: usize = 12;

fn inbound_matrix_header(ctx: &crate::theme::ThemeContext) -> Line<'static> {
    Line::from(vec![
        detail_label_span("Transport", ctx),
        Span::styled(
            format!("{:<INBOUND_MATRIX_COLUMN_WIDTH$}", "IPv4"),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0).bold()),
        ),
        Span::styled(
            "IPv6",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0).bold()),
        ),
    ])
}

fn inbound_status_cell(observed: bool, ctx: &crate::theme::ThemeContext) -> Span<'static> {
    let (status, style) = if observed {
        (
            "Seen",
            ctx.apply(Style::default().fg(ctx.state_success()).bold()),
        )
    } else {
        (
            "Not yet",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        )
    };
    Span::styled(format!("{status:<INBOUND_MATRIX_COLUMN_WIDTH$}"), style)
}

fn inbound_transport_row(
    transport: &'static str,
    ipv4_seen: bool,
    ipv6_seen: bool,
    ctx: &crate::theme::ThemeContext,
) -> Line<'static> {
    Line::from(vec![
        detail_label_span(transport, ctx),
        inbound_status_cell(ipv4_seen, ctx),
        inbound_status_cell(ipv6_seen, ctx),
    ])
}

fn build_path_detail_lines(
    item: ConfigItem,
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let anonymize = render_ctx.screen.ui.anonymize_torrent_names;
    let draft_settings = if config_item_is_locked(item, render_ctx.shared_follower) {
        render_ctx.screen.settings
    } else {
        render_ctx.settings
    };
    let (draft, active, description) = if item == ConfigItem::WatchFolder {
        (
            draft_settings.watch_folder.as_deref(),
            crate::config::resolve_host_watch_path(render_ctx.screen.settings),
            "Files placed here are discovered and queued for ingest.",
        )
    } else {
        (
            draft_settings.default_download_folder.as_deref(),
            render_ctx.screen.settings.default_download_folder.clone(),
            "New torrents use this location unless an add flow overrides it.",
        )
    };
    let dirty = !config_item_is_locked(item, render_ctx.shared_follower)
        && config_item_is_dirty(item, render_ctx.settings, render_ctx.screen.settings);
    let mut lines = vec![
        detail_row(
            "Configured",
            anonymize_config_value(item, path_to_string(draft), anonymize),
            ctx.apply(
                Style::default()
                    .fg(if dirty {
                        ctx.state_warning()
                    } else {
                        ctx.theme.semantic.text
                    })
                    .bold(),
            ),
            ctx,
        ),
        Line::from(""),
        detail_divider(width, ctx),
        info_note_line(description, ctx),
        Line::from(""),
        info_section_heading("PATH CONTEXT", ctx),
        detail_row(
            "Resolved",
            anonymize_config_value(item, path_to_string(active.as_deref()), anonymize),
            ctx.apply(Style::default().fg(ctx.accent_sapphire()).bold()),
            ctx,
        ),
        detail_row(
            "State",
            if draft.is_some() {
                "Configured"
            } else {
                "Not set"
            }
            .to_string(),
            ctx.apply(Style::default().fg(if draft.is_some() {
                ctx.state_success()
            } else {
                ctx.theme.semantic.subtext0
            })),
            ctx,
        ),
    ];
    if config_item_is_locked(item, render_ctx.shared_follower) {
        let message = if shared_path_is_manual(item) {
            "This shared path is managed in the shared settings file."
        } else {
            "Shared settings are read-only while this node is a follower."
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message,
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        )));
    }
    lines
}

fn build_layout_detail_lines(
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let mode = if config_item_is_locked(ConfigItem::UiLayoutMode, render_ctx.shared_follower) {
        render_ctx.screen.settings.ui_layout_mode
    } else {
        render_ctx.settings.ui_layout_mode
    };
    let kind_label = match render_ctx.layout_kind {
        ConfigLayoutKind::Wide => "Wide",
        ConfigLayoutKind::Stacked => "Stacked",
        ConfigLayoutKind::Compact => "Compact",
    };
    let arrangement = match render_ctx.layout_kind {
        ConfigLayoutKind::Wide => "Side by side",
        ConfigLayoutKind::Stacked => "Settings above details",
        ConfigLayoutKind::Compact => "One panel at a time",
    };
    vec![
        choice_line(
            "Mode",
            &[
                ("Auto", mode == crate::config::UiLayoutMode::Auto),
                (
                    "Horizontal",
                    mode == crate::config::UiLayoutMode::Horizontal,
                ),
                ("Vertical", mode == crate::config::UiLayoutMode::Vertical),
                ("Square", mode == crate::config::UiLayoutMode::Square),
            ],
            ctx,
        ),
        Line::from(""),
        detail_divider(width, ctx),
        info_note_line(
            "Auto follows the terminal shape. Forced modes preserve the preferred arrangement while still protecting minimum usable panel sizes.",
            ctx,
        ),
        Line::from(""),
        info_section_heading("RESOLVED VIEW", ctx),
        detail_row(
            "Resolved",
            kind_label.to_string(),
            ctx.apply(Style::default().fg(ctx.state_success())),
            ctx,
        ),
        detail_row(
            "Viewport",
            format!(
                "{} × {}",
                render_ctx.terminal_area.width, render_ctx.terminal_area.height
            ),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ctx,
        ),
        detail_row(
            "Arrangement",
            arrangement.to_string(),
            ctx.apply(Style::default().fg(ctx.accent_sapphire()).bold()),
            ctx,
        ),
    ]
}

fn build_confirm_add_detail_lines(
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let enabled = render_ctx.settings.always_show_add_location_prompt;
    let flow = if enabled {
        "Add  →  Review location and priorities  →  Start"
    } else {
        "Add  →  Use the configured default  →  Start"
    };
    vec![
        choice_line(
            "Mode",
            &[("Enabled", enabled), ("Disabled", !enabled)],
            ctx,
        ),
        Line::from(""),
        detail_divider(width, ctx),
        info_note_line(
            "When enabled, manual add flows pause for a location and file-priority review before starting.",
            ctx,
        ),
        Line::from(""),
        info_section_heading("ACTIVE FLOW", ctx),
        detail_row(
            "Behavior",
            if enabled {
                "Always review"
            } else {
                "Use fast add paths"
            }
            .to_string(),
            ctx.apply(Style::default().fg(if enabled {
                ctx.state_success()
            } else {
                ctx.theme.semantic.subtext0
            })),
            ctx,
        ),
        Line::from(""),
        Line::from(Span::styled(
            flow,
            ctx.apply(Style::default().fg(ctx.accent_sapphire()).bold()),
        )),
    ]
}

fn build_rate_detail_lines(
    download: bool,
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let item = if download {
        ConfigItem::GlobalDownloadLimit
    } else {
        ConfigItem::GlobalUploadLimit
    };
    let draft_settings = if config_item_is_locked(item, render_ctx.shared_follower) {
        render_ctx.screen.settings
    } else {
        render_ctx.settings
    };
    let draft = if download {
        draft_settings.global_download_limit_bps
    } else {
        draft_settings.global_upload_limit_bps
    };
    let current = if download {
        render_ctx
            .screen
            .ui
            .avg_download_history
            .last()
            .copied()
            .unwrap_or(0)
    } else {
        render_ctx
            .screen
            .ui
            .avg_upload_history
            .last()
            .copied()
            .unwrap_or(0)
    };
    let effective = if download {
        render_ctx.screen.ui.effective_download_limit_bps
    } else {
        render_ctx.screen.settings.global_upload_limit_bps
    };
    let dirty = !config_item_is_locked(item, render_ctx.shared_follower)
        && config_item_is_dirty(item, render_ctx.settings, render_ctx.screen.settings);
    let metric_color = if download {
        ctx.accent_sky()
    } else {
        ctx.accent_teal()
    };
    vec![
        detail_row(
            "Limit",
            format_limit_bps(draft),
            ctx.apply(
                Style::default()
                    .fg(if dirty {
                        ctx.state_warning()
                    } else {
                        ctx.theme.semantic.text
                    })
                    .bold(),
            ),
            ctx,
        ),
        Line::from(""),
        detail_divider(width, ctx),
        info_note_line(
            if download && render_ctx.layout_kind == ConfigLayoutKind::Compact {
                "Caps download traffic; disk protection may lower the live ceiling."
            } else if download {
                "Caps aggregate download traffic. The adaptive disk limiter may temporarily lower the effective ceiling."
            } else {
                "Caps aggregate upload traffic across active torrents."
            },
            ctx,
        ),
        Line::from(""),
        info_section_heading("LIVE TRAFFIC", ctx),
        detail_row(
            "Effective",
            format_limit_bps(effective),
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
            ctx,
        ),
        detail_row(
            "Current",
            format_speed(current),
            ctx.apply(Style::default().fg(metric_color).bold()),
            ctx,
        ),
        rate_gauge_line(current, effective, width, metric_color, ctx),
    ]
}

fn build_edit_detail_lines(
    editor: &ConfigEditState,
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let item = editor.item;
    let buffer = editor.buffer.as_str();
    let anonymize =
        render_ctx.screen.ui.anonymize_torrent_names && config_item_contains_private_value(item);
    let current =
        anonymize_config_value(item, value_for_item(item, render_ctx.settings), anonymize);
    let (status, valid) = match item {
        ConfigItem::ClientPort => {
            let valid =
                is_random_port_input(buffer) || buffer.parse::<u16>().is_ok_and(|port| port > 0);
            (
                if anonymize {
                    "Port value hidden while anonymized.".to_string()
                } else {
                    port_edit_status_message(buffer)
                },
                valid,
            )
        }
        ConfigItem::GlobalDownloadLimit | ConfigItem::GlobalUploadLimit => {
            let parsed = parse_rate_limit_input(buffer);
            (
                parsed
                    .map(|rate| format!("Ready: {}", format_limit_bps(rate)))
                    .unwrap_or_else(|| {
                        "Use a value such as 25 Mbps, raw bits/s, or unlimited.".to_string()
                    }),
                parsed.is_some(),
            )
        }
        ConfigItem::NetworkIpv4Address => {
            let valid = buffer.is_empty() || buffer.parse::<std::net::Ipv4Addr>().is_ok();
            (
                if valid {
                    "Ready to apply IPv4 source address.".to_string()
                } else {
                    "Enter a valid IPv4 address or leave empty.".to_string()
                },
                valid,
            )
        }
        ConfigItem::NetworkIpv6Address => {
            let valid = buffer.is_empty() || buffer.parse::<std::net::Ipv6Addr>().is_ok();
            (
                if valid {
                    "Ready to apply IPv6 source address.".to_string()
                } else {
                    "Enter a valid IPv6 address or leave empty.".to_string()
                },
                valid,
            )
        }
        ConfigItem::NetworkDnsServers => {
            let valid = buffer
                .split(|character: char| character == ',' || character.is_whitespace())
                .filter(|value| !value.is_empty())
                .all(|value| value.parse::<std::net::SocketAddr>().is_ok());
            (
                if valid {
                    "Ready to apply literal DNS server addresses.".to_string()
                } else {
                    "Use literal socket addresses such as 192.0.2.53:53 or [2001:db8::53]:53."
                        .to_string()
                },
                valid,
            )
        }
        ConfigItem::NetworkInterface => (
            if anonymize {
                "Interface value hidden while anonymized.".to_string()
            } else if buffer.trim().is_empty() {
                "Empty removes the configured interface name.".to_string()
            } else {
                format!("Ready to apply interface {}.", buffer.trim())
            },
            true,
        ),
        _ => ("Ready".to_string(), true),
    };
    let mut lines = Vec::new();
    if anonymize {
        lines.push(Line::from(Span::styled(
            "Value hidden while anonymized",
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        )));
    } else {
        lines.extend(edit_field_lines(editor, width, ctx));
    }
    lines.extend([
        Line::from(Span::styled(
            status,
            ctx.apply(Style::default().fg(if buffer.is_empty() {
                ctx.theme.semantic.subtext1
            } else if valid {
                ctx.state_success()
            } else {
                ctx.state_error()
            })),
        )),
        Line::from(""),
        detail_divider(width, ctx),
        info_note_line(edit_help_text(item), ctx),
        Line::from(""),
        info_section_heading("REFERENCE", ctx),
        detail_row(
            "Current",
            current,
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ctx,
        ),
    ]);
    lines
}

fn edit_help_text(item: ConfigItem) -> &'static str {
    match item {
        ConfigItem::ClientPort => {
            "Valid range: 1–65535. Enter validates and updates the runtime listener."
        }
        ConfigItem::GlobalDownloadLimit | ConfigItem::GlobalUploadLimit => {
            "Enter applies the rate in bits per second. Accepted suffixes range from Kbps through Ebps. Zero means unlimited."
        }
        ConfigItem::NetworkInterface => "Use the exact interface name shown by the operating system.",
        ConfigItem::NetworkIpv4Address | ConfigItem::NetworkIpv6Address => {
            "Leave empty for interface-selected addressing, or enter one exact local address."
        }
        ConfigItem::NetworkDnsServers => {
            "Bound DNS accepts only literal IP socket addresses; hostnames are rejected to prevent bootstrap leakage."
        }
        _ => "Enter applies this setting.",
    }
}

fn edit_field_lines(
    editor: &ConfigEditState,
    width: u16,
    ctx: &crate::theme::ThemeContext,
) -> Vec<Line<'static>> {
    if width < 12 {
        return vec![edit_prompt_line(&editor.buffer, ctx)];
    }

    let field_width = width.saturating_sub(2).min(46) as usize;
    let content_width = field_width.saturating_sub(2);
    let horizontal = "─".repeat(field_width);
    let mut content_spans = vec![Span::styled(
        "│ ",
        ctx.apply(Style::default().fg(ctx.state_selected())),
    )];
    let rendered_content_width;

    if editor.select_all {
        let hint = "  type to replace";
        let show_hint = editor.buffer.chars().count() + hint.chars().count() <= content_width;
        let value_width =
            content_width.saturating_sub(if show_hint { hint.chars().count() } else { 0 });
        let visible_value = truncate_with_ellipsis(&editor.buffer, value_width);
        rendered_content_width =
            visible_value.chars().count() + if show_hint { hint.chars().count() } else { 0 };
        content_spans.push(Span::styled(
            visible_value,
            ctx.apply(
                Style::default()
                    .fg(ctx.theme.semantic.surface0)
                    .bg(ctx.state_selected())
                    .bold(),
            ),
        ));
        if show_hint {
            content_spans.push(Span::styled(
                hint,
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ));
        }
    } else {
        let cursor = editor.cursor.min(editor.buffer.len());
        let characters = editor.buffer.chars().collect::<Vec<_>>();
        let cursor_character = editor.buffer[..cursor].chars().count();
        let visible_characters = content_width.saturating_sub(1);
        let mut start = cursor_character.saturating_sub(visible_characters / 2);
        let end = (start + visible_characters).min(characters.len());
        start = end.saturating_sub(visible_characters);
        let before = characters[start..cursor_character]
            .iter()
            .collect::<String>();
        let after = characters[cursor_character..end].iter().collect::<String>();
        rendered_content_width = before.chars().count() + 1 + after.chars().count();
        content_spans.push(Span::styled(
            before,
            ctx.apply(Style::default().fg(ctx.theme.semantic.text).bold()),
        ));
        content_spans.push(Span::styled(
            " ",
            ctx.apply(
                Style::default()
                    .fg(ctx.theme.semantic.surface0)
                    .bg(ctx.state_warning()),
            ),
        ));
        content_spans.push(Span::styled(
            after,
            ctx.apply(Style::default().fg(ctx.theme.semantic.text).bold()),
        ));
    }
    content_spans.push(Span::raw(
        " ".repeat(content_width.saturating_sub(rendered_content_width)),
    ));
    content_spans.push(Span::styled(
        " │",
        ctx.apply(Style::default().fg(ctx.state_selected())),
    ));

    vec![
        Line::from(Span::styled(
            format!("╭{horizontal}╮"),
            ctx.apply(Style::default().fg(ctx.state_selected())),
        )),
        Line::from(content_spans),
        Line::from(Span::styled(
            format!("╰{horizontal}╯"),
            ctx.apply(Style::default().fg(ctx.state_selected())),
        )),
    ]
}

fn edit_prompt_line(buffer: &str, ctx: &crate::theme::ThemeContext) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "> ",
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ),
        Span::styled(
            buffer.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.text).bold()),
        ),
        Span::styled("_", ctx.apply(Style::default().fg(ctx.state_warning()))),
    ])
}

fn detail_label_span(label: &'static str, ctx: &crate::theme::ThemeContext) -> Span<'static> {
    Span::styled(
        format!("{label:<14}"),
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
    )
}

fn detail_row(
    label: &'static str,
    value: String,
    value_style: Style,
    ctx: &crate::theme::ThemeContext,
) -> Line<'static> {
    Line::from(vec![
        detail_label_span(label, ctx),
        Span::styled(value, value_style),
    ])
}

fn info_section_heading(label: &'static str, ctx: &crate::theme::ThemeContext) -> Line<'static> {
    Line::from(Span::styled(
        label,
        ctx.apply(Style::default().fg(ctx.accent_sapphire()).bold()),
    ))
}

fn info_note_line(text: &'static str, ctx: &crate::theme::ThemeContext) -> Line<'static> {
    Line::from(Span::styled(
        text,
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
    ))
}

fn detail_divider(width: u16, ctx: &crate::theme::ThemeContext) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width.saturating_sub(1).min(52) as usize),
        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
    ))
}

fn insert_below_detail_divider(lines: &mut Vec<Line<'static>>, line: Line<'static>) {
    let insert_at = lines
        .iter()
        .position(|candidate| {
            let text = candidate
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            !text.is_empty() && text.chars().all(|character| character == '─')
        })
        .map(|divider| (divider + 2).min(lines.len()))
        .unwrap_or(lines.len());
    let has_following_space = lines
        .get(insert_at)
        .is_some_and(|candidate| candidate.spans.iter().all(|span| span.content.is_empty()));
    lines.insert(insert_at, line);
    if !has_following_space {
        lines.insert(insert_at + 1, Line::from(""));
    }
}

fn choice_line(
    label: &'static str,
    choices: &[(&'static str, bool)],
    ctx: &crate::theme::ThemeContext,
) -> Line<'static> {
    let mut spans = vec![detail_label_span(label, ctx)];
    for (index, (choice, active)) in choices.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                "  ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
            ));
        }
        spans.push(Span::styled(
            *choice,
            if *active {
                ctx.apply(
                    Style::default()
                        .fg(ctx.state_selected())
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                )
            } else {
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
            },
        ));
    }
    Line::from(spans)
}

fn rate_gauge_line(
    current: u64,
    limit: u64,
    width: u16,
    metric_color: ratatui::style::Color,
    ctx: &crate::theme::ThemeContext,
) -> Line<'static> {
    if crate::config::is_unlimited_rate_limit_bps(limit) {
        return Line::from(vec![
            detail_label_span("Usage", ctx),
            Span::styled(
                "No configured ceiling",
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
            ),
        ]);
    }
    let bar_width = width.saturating_sub(16).clamp(8, 28) as usize;
    let ratio = (current as f64 / limit.max(1) as f64).clamp(0.0, 1.0);
    let filled = (ratio * bar_width as f64).round() as usize;
    Line::from(vec![
        detail_label_span("Usage", ctx),
        Span::styled(
            "█".repeat(filled),
            ctx.apply(Style::default().fg(metric_color)),
        ),
        Span::styled(
            "░".repeat(bar_width.saturating_sub(filled)),
            ctx.apply(Style::default().fg(ctx.theme.semantic.surface1)),
        ),
        Span::styled(
            format!("  {:.0}%", ratio * 100.0),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ),
    ])
}

fn port_edit_status_message(buffer: &str) -> String {
    if buffer.is_empty() {
        return "Type a listen port or RANDOM.".to_string();
    }
    if is_random_port_input(buffer) {
        return "Ready to select a new port on each start.".to_string();
    }

    match buffer.parse::<u16>() {
        Ok(0) => "Use RANDOM to select an available port at startup.".to_string(),
        Ok(port) => format!("Ready to apply port {port}."),
        Err(_) => "Use RANDOM or a port from 1-65535.".to_string(),
    }
}

fn is_random_port_input(buffer: &str) -> bool {
    buffer.eq_ignore_ascii_case("RANDOM")
}

fn render_config_footer(
    f: &mut Frame,
    render_ctx: &ConfigRenderContext<'_, '_>,
    active_item: ConfigItem,
    active_pane: ConfigPane,
    area: Rect,
) {
    if area.height == 0 {
        return;
    }
    let locked = config_item_is_locked(active_item, render_ctx.shared_follower);
    let mut actions = Vec::new();
    if render_ctx.editing.is_some() {
        actions.push(("Enter", "apply", ActionTone::Confirm));
        actions.push(("Esc", "cancel", ActionTone::Cancel));
        actions.push(("←/→", "cursor", ActionTone::Navigate));
    } else {
        if !locked {
            match descriptor_for_item(active_item).control {
                ConfigControlKind::Bool => {
                    actions.push(("Space", "toggle", ActionTone::Toggle));
                }
                ConfigControlKind::Enum => {
                    actions.push(("Space", "next", ActionTone::Toggle));
                    actions.push(("←/→", "choice", ActionTone::Navigate));
                }
                ConfigControlKind::Number
                | ConfigControlKind::RateLimit
                | ConfigControlKind::Text => {
                    actions.push(("Space", "edit", ActionTone::Edit));
                }
                ConfigControlKind::Path => {
                    actions.push(("Space", "edit", ActionTone::Edit));
                }
            }
        }
        if render_ctx.layout_kind == ConfigLayoutKind::Compact && active_pane == ConfigPane::Details
        {
            actions.push(("Esc", "settings", ActionTone::Mode));
        } else {
            actions.push(("Esc", "close", ActionTone::Cancel));
        }
        if !locked {
            actions.push(("r", "reset", ActionTone::Clear));
        }
        actions.push(("↑/↓", "setting", ActionTone::Navigate));
    }
    let line = footer_actions_line(&actions, area.width, render_ctx.screen.theme);
    f.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

fn render_reset_confirmation_dialog(
    f: &mut Frame,
    render_ctx: &ConfigRenderContext<'_, '_>,
    item: ConfigItem,
) {
    let ctx = render_ctx.screen.theme;
    let terminal = render_ctx.terminal_area;
    let area = reset_confirmation_area(terminal);
    f.render_widget(Clear, area);

    let vertical_padding = u16::from(area.height >= 9);
    let block = Block::default()
        .title(Line::from(Span::styled(
            " Confirm Reset ",
            ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
        )))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.state_warning())))
        .padding(Padding::new(2, 2, vertical_padding, vertical_padding));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let descriptor = descriptor_for_item(item);
    let default_value = truncate_with_ellipsis(
        &value_for_item(item, &Settings::default()),
        inner.width.saturating_sub(10) as usize,
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                descriptor.label,
                ctx.apply(Style::default().fg(ctx.theme.semantic.text).bold()),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Restore this setting to its default value?",
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
            )),
            Line::from(vec![
                Span::styled(
                    "Default  ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
                ),
                Span::styled(
                    default_value,
                    ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
                ),
            ]),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true }),
        chunks[0],
    );

    let actions = Line::from(vec![
        Span::styled("[Y]", footer_key_style(ctx, ActionTone::Clear)),
        Span::raw(" Reset  "),
        Span::styled("[Esc]", footer_key_style(ctx, ActionTone::Cancel)),
        Span::raw(" Cancel"),
    ]);
    f.render_widget(
        Paragraph::new(actions).alignment(Alignment::Center),
        chunks[1],
    );
}

fn reset_confirmation_area(terminal: Rect) -> Rect {
    let width = terminal.width.saturating_sub(2).clamp(1, 52);
    let height = terminal.height.saturating_sub(2).clamp(1, 9);
    Rect::new(
        terminal.x + terminal.width.saturating_sub(width) / 2,
        terminal.y + terminal.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn footer_actions_line(
    actions: &[(&str, &str, ActionTone)],
    width: u16,
    ctx: &crate::theme::ThemeContext,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (key, label, tone) in actions {
        let separator = if spans.is_empty() { 0 } else { 3 };
        let action_width = key.chars().count() + label.chars().count() + 3;
        if used + separator + action_width > width as usize {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(
                " | ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ));
            used += 3;
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            footer_key_style(ctx, *tone),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
        used += action_width;
    }
    Line::from(spans)
}

fn action_mutates_selected_setting(action: &ConfigAction) -> bool {
    matches!(
        action,
        ConfigAction::ShiftSelected
            | ConfigAction::SetSelectedBool(_)
            | ConfigAction::ResetSelected
            | ConfigAction::IncreaseSelected
            | ConfigAction::DecreaseSelected
    )
}

fn action_supported_for_item(action: &ConfigAction, item: ConfigItem) -> bool {
    let control = descriptor_for_item(item).control;
    match action {
        ConfigAction::ShiftSelected => {
            matches!(
                control,
                ConfigControlKind::Bool
                    | ConfigControlKind::Enum
                    | ConfigControlKind::Number
                    | ConfigControlKind::RateLimit
                    | ConfigControlKind::Path
                    | ConfigControlKind::Text
            )
        }
        ConfigAction::SetSelectedBool(_) => control == ConfigControlKind::Bool,
        ConfigAction::IncreaseSelected | ConfigAction::DecreaseSelected => {
            control == ConfigControlKind::Enum
        }
        _ => true,
    }
}

fn exit_config(mode: &mut AppMode, file_browser_generation: &mut u64) {
    *file_browser_generation = file_browser_generation.wrapping_add(1);
    *mode = AppMode::Normal;
}

pub fn handle_event(event: CrosstermEvent, ctx: ConfigHandleContext<'_>) -> Option<Settings> {
    if let CrosstermEvent::Paste(text) = &event {
        let editor = ctx.editing.as_mut()?;
        let item = editor.item;
        for character in text
            .chars()
            .filter(|character| edit_character_allowed(item, *character))
        {
            insert_editor_character(editor, character);
        }
        return None;
    }

    if let CrosstermEvent::Key(key) = event {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let action = if let Some(confirmed_item) = *ctx.reset_confirmation {
            match key.code {
                KeyCode::Char('Y') => {
                    *ctx.reset_confirmation = None;
                    if let Some(index) = ctx.items.iter().position(|item| *item == confirmed_item) {
                        *ctx.selected_index = index;
                    }
                    Some(ConfigAction::ResetSelected)
                }
                KeyCode::Esc => {
                    *ctx.reset_confirmation = None;
                    return None;
                }
                _ => return None,
            }
        } else {
            map_key_to_config_action(key.code, ctx.editing)
        };
        if let Some(action) = action {
            if action == ConfigAction::Exit {
                if ctx.compact && *ctx.active_pane == ConfigPane::Details {
                    *ctx.active_pane = ConfigPane::Settings;
                } else {
                    exit_config(ctx.mode, ctx.file_browser_generation);
                }
                return None;
            }
            if action == ConfigAction::ToggleAnonymize {
                *ctx.anonymize = !*ctx.anonymize;
                return None;
            }

            *ctx.selected_index =
                normalized_visible_setting_index(ctx.items, ctx.settings_edit, *ctx.selected_index);
            let active_item = selected_item(ctx.items, *ctx.selected_index);
            if !action_supported_for_item(&action, active_item) {
                return None;
            }
            if config_item_is_locked(active_item, ctx.shared_follower)
                && (action_mutates_selected_setting(&action)
                    || action == ConfigAction::RequestReset)
            {
                *ctx.active_pane = ConfigPane::Details;
                return None;
            }

            if action == ConfigAction::RequestReset {
                *ctx.reset_confirmation = Some(active_item);
                if ctx.compact {
                    *ctx.active_pane = ConfigPane::Details;
                }
                return None;
            }

            if ctx.compact
                && *ctx.active_pane == ConfigPane::Settings
                && action_mutates_selected_setting(&action)
            {
                *ctx.active_pane = ConfigPane::Details;
            }

            let reduced = reduce_config_action(
                action,
                ctx.settings_edit,
                ctx.selected_index,
                ctx.items,
                ctx.editing,
            );
            let mut settings_update = None;
            for effect in reduced.effects {
                match effect {
                    ConfigEffect::ApplySettings => {
                        settings_update = Some(merge_config_item_into_current(
                            ctx.settings_edit,
                            ctx.applied_settings,
                            active_item,
                            ctx.shared_follower,
                        ));
                    }
                    ConfigEffect::AppCommand(command) => {
                        let mut command = *command;
                        if let AppCommand::FetchFileTree {
                            browser_generation, ..
                        } = &mut command
                        {
                            *ctx.file_browser_generation =
                                ctx.file_browser_generation.wrapping_add(1);
                            *browser_generation = *ctx.file_browser_generation;
                        }
                        spawn_app_command_sender(
                            ctx.app_command_tx.clone(),
                            ctx.shutdown_tx.subscribe(),
                            command,
                        );
                    }
                }
            }
            return settings_update;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, InboundPeerTransportStatus};
    use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
    use crate::theme::{Theme, ThemeContext, ThemeName};
    use ratatui::{backend::TestBackend, Terminal};
    use strum::IntoEnumIterator;

    fn config_items() -> Vec<ConfigItem> {
        vec![
            ConfigItem::ClientPort,
            ConfigItem::DefaultDownloadFolder,
            ConfigItem::WatchFolder,
            ConfigItem::UiLayoutMode,
            ConfigItem::AlwaysShowAddLocationPrompt,
            ConfigItem::GlobalDownloadLimit,
            ConfigItem::GlobalUploadLimit,
        ]
    }

    fn visible_network_items(settings: &Settings) -> Vec<ConfigItem> {
        let items = ConfigItem::iter().collect::<Vec<_>>();
        config_list_rows(&items, settings)
            .into_iter()
            .filter_map(|row| match row {
                ConfigListRow::Setting { item, .. }
                    if config_category_for_item(item) == ConfigCategory::Network =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect()
    }

    fn discovered_test_interface(name: &str, index: u32) -> NetworkInterfaceInfo {
        NetworkInterfaceInfo {
            name: name.to_string(),
            index,
            is_up: true,
            is_loopback: false,
            ipv4_addresses: vec![std::net::Ipv4Addr::new(192, 0, 2, index as u8)],
            ipv6_addresses: Vec::new(),
        }
    }

    fn test_theme_context() -> ThemeContext {
        ThemeContext::new(Theme::builtin(ThemeName::CatppuccinMocha), 0.0)
    }

    fn plain_lines(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    fn assert_detail_hierarchy(
        lines: &[Line<'_>],
        actionable_labels: &[&str],
        informational_labels: &[&str],
    ) {
        let lines = plain_lines(lines);
        let divider = lines
            .iter()
            .position(|line| !line.is_empty() && line.chars().all(|character| character == '─'))
            .expect("detail panel should contain a divider");
        for label in actionable_labels {
            let index = lines
                .iter()
                .position(|line| line.contains(label))
                .unwrap_or_else(|| panic!("missing actionable label {label}"));
            assert!(index < divider, "{label} should be above the divider");
        }
        for label in informational_labels {
            let index = lines
                .iter()
                .position(|line| line.contains(label))
                .unwrap_or_else(|| panic!("missing informational label {label}"));
            assert!(index > divider, "{label} should be below the divider");
        }
    }

    fn assert_first_below_divider(lines: &[Line<'_>], expected_description: &str) {
        let lines = plain_lines(lines);
        let divider = lines
            .iter()
            .position(|line| !line.is_empty() && line.chars().all(|character| character == '─'))
            .expect("detail panel should contain a divider");
        assert!(
            lines
                .get(divider + 1)
                .is_some_and(|line| line.contains(expected_description)),
            "description should be the first content below the divider"
        );
    }

    fn editor(item: ConfigItem, buffer: &str) -> ConfigEditState {
        ConfigEditState {
            item,
            buffer: buffer.to_string(),
            cursor: buffer.len(),
            select_all: false,
        }
    }

    fn rendered_config(
        width: u16,
        height: u16,
        layout_mode: crate::config::UiLayoutMode,
        active_pane: ConfigPane,
        selected_index: usize,
    ) -> String {
        let settings = Settings {
            ui_layout_mode: layout_mode,
            ..Settings::default()
        };
        let app_state = AppState {
            externally_accessable_port_v4: true,
            externally_accessable_port_v6: true,
            inbound_peer_transports: InboundPeerTransportStatus {
                tcp_ipv4_seen: true,
                tcp_ipv6_seen: false,
                utp_ipv4_seen: false,
                utp_ipv6_seen: true,
            },
            ..AppState::default()
        };
        let dht_status = DhtStatus::default();
        let dht_wave_telemetry = DhtWaveTelemetry::default();
        let theme = test_theme_context();
        let screen = ScreenContext::new(
            &app_state,
            &dht_status,
            &dht_wave_telemetry,
            &settings,
            &theme,
        );
        let items = config_items();
        let editing = None;
        let reset_confirmation = None;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &screen,
                    ConfigDrawState {
                        settings: &settings,
                        selected_index,
                        items: &items,
                        active_pane,
                        editing: &editing,
                        reset_confirmation: &reset_confirmation,
                    },
                );
            })
            .expect("render config");

        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width.max(1) as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn descriptors_cover_existing_config_items_by_category() {
        let described_items = config_setting_descriptors()
            .iter()
            .map(|descriptor| descriptor.item)
            .collect::<Vec<_>>();

        assert_eq!(described_items.len(), ConfigItem::iter().count());
        for item in ConfigItem::iter() {
            assert!(
                described_items.contains(&item),
                "missing config descriptor for {item:?}"
            );
        }

        assert_eq!(
            config_category_for_item(ConfigItem::DefaultDownloadFolder),
            ConfigCategory::Paths
        );
        assert_eq!(
            config_category_for_item(ConfigItem::GlobalDownloadLimit),
            ConfigCategory::Downloads
        );
        assert_eq!(
            config_category_for_item(ConfigItem::ClientPort),
            ConfigCategory::Network
        );
        assert_eq!(
            config_category_for_item(ConfigItem::UiLayoutMode),
            ConfigCategory::Ui
        );

        for item in [
            ConfigItem::ClientPort,
            ConfigItem::WatchFolder,
            ConfigItem::AlwaysShowAddLocationPrompt,
        ] {
            assert_eq!(descriptor_for_item(item).scope, ConfigScope::Host);
        }
        for item in [
            ConfigItem::DefaultDownloadFolder,
            ConfigItem::UiLayoutMode,
            ConfigItem::GlobalDownloadLimit,
            ConfigItem::GlobalUploadLimit,
        ] {
            assert_eq!(descriptor_for_item(item).scope, ConfigScope::Shared);
        }
    }

    #[test]
    fn config_list_rows_group_settings_under_category_headers() {
        let rows = config_list_rows(&config_items(), &Settings::default());

        assert_eq!(rows[0], ConfigListRow::Category(ConfigCategory::Network));
        assert_eq!(
            rows[1],
            ConfigListRow::Setting {
                global_index: 0,
                item: ConfigItem::ClientPort,
            }
        );
        assert!(rows.contains(&ConfigListRow::Category(ConfigCategory::Paths)));
        assert!(rows.contains(&ConfigListRow::Category(ConfigCategory::Downloads)));
        assert!(rows.contains(&ConfigListRow::Category(ConfigCategory::Ui)));
    }

    #[test]
    fn normal_network_routing_hides_binding_only_controls() {
        let settings = Settings::default();

        assert_eq!(
            visible_network_items(&settings),
            vec![ConfigItem::ClientPort, ConfigItem::NetworkBindingMode]
        );

        let items = ConfigItem::iter().collect::<Vec<_>>();
        assert_eq!(normalized_visible_setting_index(&items, &settings, 2), 1);
    }

    #[test]
    fn interface_routing_reveals_applicable_controls_progressively() {
        let mut settings = Settings::default();
        settings.network_binding.mode = NetworkBindingMode::Interface;

        assert_eq!(
            visible_network_items(&settings),
            vec![
                ConfigItem::ClientPort,
                ConfigItem::NetworkBindingMode,
                ConfigItem::NetworkInterface,
                ConfigItem::NetworkIpv4Enabled,
                ConfigItem::NetworkIpv6Enabled,
                ConfigItem::NetworkIpv4Address,
                ConfigItem::NetworkIpv6Address,
                ConfigItem::NetworkDnsPolicy,
            ]
        );

        settings.network_binding.dns_policy = DnsPolicy::Bound;
        assert_eq!(
            visible_network_items(&settings).last(),
            Some(&ConfigItem::NetworkDnsServers)
        );
    }

    #[test]
    fn binding_controls_are_indented_under_network_routing() {
        let theme = test_theme_context();
        let routing = config_setting_row_line(ConfigItem::NetworkBindingMode, true, false, &theme);
        assert_eq!(plain_lines(&[routing]), vec!["▶ Network Routing"]);

        for item in [
            ConfigItem::NetworkInterface,
            ConfigItem::NetworkIpv4Enabled,
            ConfigItem::NetworkIpv6Enabled,
            ConfigItem::NetworkIpv4Address,
            ConfigItem::NetworkIpv6Address,
            ConfigItem::NetworkDnsPolicy,
            ConfigItem::NetworkDnsServers,
        ] {
            let line = config_setting_row_line(item, true, false, &theme);
            assert!(
                plain_lines(&[line])[0].starts_with("  ▶ "),
                "{item:?} should be indented under Network Routing"
            );
        }
    }

    #[test]
    fn local_address_routing_does_not_show_interface_name() {
        let mut settings = Settings::default();
        settings.network_binding.mode = NetworkBindingMode::LocalAddress;

        let visible = visible_network_items(&settings);
        assert!(!visible.contains(&ConfigItem::NetworkInterface));
        assert!(visible.contains(&ConfigItem::NetworkIpv4Address));
        assert!(visible.contains(&ConfigItem::NetworkIpv6Address));
    }

    #[test]
    fn discovered_interface_choices_cycle_without_guessing_a_vpn() {
        let interfaces = vec![
            discovered_test_interface("lan-test0", 1),
            discovered_test_interface("tunnel-test0", 2),
        ];

        assert_eq!(
            cycled_network_interface_name(None, &interfaces, true).as_deref(),
            Some("lan-test0")
        );
        assert_eq!(
            cycled_network_interface_name(Some("lan-test0"), &interfaces, true).as_deref(),
            Some("tunnel-test0")
        );
        assert_eq!(
            cycled_network_interface_name(Some("lan-test0"), &interfaces, false).as_deref(),
            Some("tunnel-test0")
        );
        assert_eq!(
            cycled_network_interface_name(Some("manual-test0"), &interfaces, false).as_deref(),
            Some("tunnel-test0")
        );
    }

    #[test]
    fn anonymized_config_details_hide_network_and_path_values() {
        let mut settings = Settings {
            client_port: 61_234,
            ..Settings::default()
        };
        settings.network_binding.mode = NetworkBindingMode::Interface;
        settings.network_binding.interface = Some("private-test0".to_string());
        settings.network_binding.ipv4_address = Some("192.0.2.44".parse().unwrap());
        settings.network_binding.ipv6_address = Some("2001:db8::44".parse().unwrap());
        settings.network_binding.dns_servers = vec!["192.0.2.53:53".parse().unwrap()];
        settings.watch_folder = Some("/Users/ExampleUser/Downloads/incoming".into());

        let app_state = AppState {
            anonymize_torrent_names: true,
            network_runtime_status: Some(crate::networking::NetworkRuntimeStatus {
                phase: NetworkRuntimePhase::Ready,
                mode: NetworkBindingMode::Interface,
                interface: Some("private-test0".to_string()),
                interface_index: Some(9),
                enable_ipv4: true,
                enable_ipv6: true,
                selected_ipv4_address: Some("192.0.2.44".parse().unwrap()),
                selected_ipv6_address: Some("2001:db8::44".parse().unwrap()),
                interface_ipv4_addresses: vec!["192.0.2.45".parse().unwrap()],
                interface_ipv6_addresses: vec!["2001:db8::45".parse().unwrap()],
                dns_policy: DnsPolicy::Bound,
                dns_servers: vec!["192.0.2.53:53".parse().unwrap()],
                generation_id: Some(7),
                config_epoch: Some(7),
                blocked_reason: None,
                warning: None,
            }),
            ..AppState::default()
        };
        let dht_status = DhtStatus::default();
        let dht_wave_telemetry = DhtWaveTelemetry::default();
        let theme = test_theme_context();
        let screen = ScreenContext::new(
            &app_state,
            &dht_status,
            &dht_wave_telemetry,
            &settings,
            &theme,
        );
        let editing = None;
        let render_ctx = ConfigRenderContext {
            screen: &screen,
            settings: &settings,
            editing: &editing,
            layout_kind: ConfigLayoutKind::Wide,
            terminal_area: Rect::new(0, 0, 120, 30),
            shared_follower: false,
        };

        let mut lines =
            build_network_binding_detail_lines(ConfigItem::NetworkInterface, &render_ctx, 90);
        lines.extend(build_network_binding_detail_lines(
            ConfigItem::NetworkIpv4Address,
            &render_ctx,
            90,
        ));
        lines.extend(build_network_binding_detail_lines(
            ConfigItem::NetworkDnsServers,
            &render_ctx,
            90,
        ));
        lines.extend(build_path_detail_lines(
            ConfigItem::WatchFolder,
            &render_ctx,
            90,
        ));
        lines.extend(build_port_detail_lines(&render_ctx, 90));
        lines.extend(discovered_interface_lines(
            vec![discovered_test_interface("private-test0", 9)],
            Some("private-test0"),
            90,
            true,
            &theme,
        ));
        lines.extend(build_edit_detail_lines(
            &editor(ConfigItem::NetworkInterface, "private-test0"),
            &render_ctx,
            90,
        ));
        lines.extend(build_edit_detail_lines(
            &editor(ConfigItem::ClientPort, "61234"),
            &render_ctx,
            90,
        ));
        let text = plain_lines(&lines).join("\n");

        assert!(text.contains("Anonymized"));
        for private_value in [
            "private-test0",
            "192.0.2.44",
            "192.0.2.45",
            "2001:db8::44",
            "2001:db8::45",
            "192.0.2.53:53",
            "/Users/ExampleUser/Downloads/incoming",
            "61234",
        ] {
            assert!(
                !text.contains(private_value),
                "anonymized config leaked {private_value}"
            );
        }
    }

    #[test]
    fn visible_navigation_follows_grouped_category_order() {
        let items = config_items();
        let settings = Settings::default();

        assert_eq!(next_visible_setting_index(&items, &settings, 0), 1);
        assert_eq!(next_visible_setting_index(&items, &settings, 2), 4);
        assert_eq!(next_visible_setting_index(&items, &settings, 6), 3);
        assert_eq!(next_visible_setting_index(&items, &settings, 3), 3);
        assert_eq!(previous_visible_setting_index(&items, &settings, 3), 6);
        assert_eq!(previous_visible_setting_index(&items, &settings, 4), 2);
        assert_eq!(previous_visible_setting_index(&items, &settings, 0), 0);
    }

    #[test]
    fn wide_render_shows_master_detail_and_primary_footer_action() {
        let rendered = rendered_config(
            120,
            30,
            crate::config::UiLayoutMode::Horizontal,
            ConfigPane::Settings,
            0,
        );

        assert!(rendered.contains("▶ Listen Port"));
        assert!(!rendered.contains("Settings ·"));
        assert!(rendered.contains("Listen Port · Network"));
        assert!(rendered.contains("[Space] edit"));
        assert!(!rendered.contains("[x]"));
    }

    #[test]
    fn detail_panels_keep_controls_above_and_information_below_the_divider() {
        let settings = Settings::default();
        let app_state = AppState {
            externally_accessable_port_v4: true,
            externally_accessable_port_v6: true,
            inbound_peer_transports: InboundPeerTransportStatus {
                tcp_ipv4_seen: true,
                tcp_ipv6_seen: false,
                utp_ipv4_seen: false,
                utp_ipv6_seen: true,
            },
            ..AppState::default()
        };
        let dht_status = DhtStatus::default();
        let dht_wave_telemetry = DhtWaveTelemetry::default();
        let theme = test_theme_context();
        let screen = ScreenContext::new(
            &app_state,
            &dht_status,
            &dht_wave_telemetry,
            &settings,
            &theme,
        );
        let editing = None;
        let render_ctx = ConfigRenderContext {
            screen: &screen,
            settings: &settings,
            editing: &editing,
            layout_kind: ConfigLayoutKind::Wide,
            terminal_area: Rect::new(0, 0, 120, 30),
            shared_follower: false,
        };

        let port_lines = build_port_detail_lines(&render_ctx, 60);
        assert_detail_hierarchy(
            &port_lines,
            &["Configured"],
            &["Runtime", "Transport", "TCP", "uTP (UDP)", "Default"],
        );
        assert_first_below_divider(&port_lines, "Accepts inbound");
        let port_text = plain_lines(&port_lines);
        let matrix_header = port_text
            .iter()
            .find(|line| line.contains("Transport"))
            .expect("matrix should have a transport header");
        let tcp_row = port_text
            .iter()
            .find(|line| line.contains("TCP"))
            .expect("matrix should have a TCP row");
        let utp_row = port_text
            .iter()
            .find(|line| line.contains("uTP (UDP)"))
            .expect("matrix should have a uTP row");
        assert!(matrix_header.contains("IPv4"));
        assert!(matrix_header.contains("IPv6"));
        assert!(tcp_row.contains("Seen"));
        assert!(tcp_row.contains("Not yet"));
        assert!(utp_row.contains("Not yet"));
        assert!(utp_row.contains("Seen"));

        let path_lines = build_path_detail_lines(ConfigItem::WatchFolder, &render_ctx, 60);
        assert_detail_hierarchy(&path_lines, &["Configured"], &["Resolved", "State"]);
        assert_first_below_divider(&path_lines, "Files placed here");

        let layout_lines = build_layout_detail_lines(&render_ctx, 60);
        assert_detail_hierarchy(
            &layout_lines,
            &["Mode"],
            &["Resolved", "Viewport", "Arrangement"],
        );
        assert_first_below_divider(&layout_lines, "Auto follows");
        let layout_text = plain_lines(&layout_lines).join("\n");
        assert!(layout_text.contains("Side by side"));
        assert!(!layout_text.contains("[ Settings ]"));

        let confirm_lines = build_confirm_add_detail_lines(&render_ctx, 60);
        assert_detail_hierarchy(&confirm_lines, &["Mode"], &["Behavior"]);
        assert_first_below_divider(&confirm_lines, "When enabled");

        let rate_lines = build_rate_detail_lines(true, &render_ctx, 60);
        assert_detail_hierarchy(&rate_lines, &["Limit"], &["Effective", "Current", "Usage"]);
        assert_first_below_divider(&rate_lines, "Caps aggregate");

        let editor = editor(ConfigItem::ClientPort, "7123");
        let edit_lines = build_edit_detail_lines(&editor, &render_ctx, 60);
        assert_detail_hierarchy(&edit_lines, &["7123"], &["Current", "Valid range"]);
        assert_first_below_divider(&edit_lines, "Valid range");

        let mut lines = build_port_detail_lines(&render_ctx, 60);
        insert_below_detail_divider(&mut lines, Line::from("STATUS  listener update failed"));
        assert_detail_hierarchy(&lines, &["Configured"], &["STATUS", "Runtime"]);
    }

    #[test]
    fn path_footer_uses_the_shared_space_edit_action() {
        let rendered = rendered_config(
            120,
            30,
            crate::config::UiLayoutMode::Horizontal,
            ConfigPane::Settings,
            2,
        );

        assert!(rendered.contains("[Space] edit"));
        assert!(!rendered.contains("[Space] browse"));
    }

    #[test]
    fn pane_padding_adds_vertical_space_only_when_height_allows_it() {
        let roomy_area = Rect::new(0, 0, 40, 15);
        let compact_area = Rect::new(0, 0, 40, 14);
        let roomy_inner = Block::default()
            .borders(Borders::ALL)
            .padding(config_pane_padding(2, roomy_area, true))
            .inner(roomy_area);
        let compact_inner = Block::default()
            .borders(Borders::ALL)
            .padding(config_pane_padding(2, compact_area, true))
            .inner(compact_area);

        assert_eq!(roomy_inner.y, roomy_area.y + 2);
        assert_eq!(roomy_inner.height, roomy_area.height - 4);
        assert_eq!(compact_inner.y, compact_area.y + 1);
        assert_eq!(compact_inner.height, compact_area.height - 2);

        let navigation_inner = Block::default()
            .borders(Borders::ALL)
            .padding(config_pane_padding(1, roomy_area, false))
            .inner(roomy_area);
        assert_eq!(navigation_inner.y, roomy_area.y + 1);
        assert_eq!(navigation_inner.height, roomy_area.height - 3);
    }

    #[test]
    fn reset_confirmation_uses_a_compact_centered_area() {
        let terminal = Rect::new(3, 5, 120, 30);
        let dialog = reset_confirmation_area(terminal);

        assert_eq!(dialog.width, 52);
        assert_eq!(dialog.height, 9);
        let left = dialog.x - terminal.x;
        let right = terminal.right() - dialog.right();
        let top = dialog.y - terminal.y;
        let bottom = terminal.bottom() - dialog.bottom();
        assert!(left.abs_diff(right) <= 1);
        assert!(top.abs_diff(bottom) <= 1);

        let compact_terminal = Rect::new(0, 0, 30, 7);
        let compact_dialog = reset_confirmation_area(compact_terminal);
        assert_eq!(compact_dialog, Rect::new(1, 1, 28, 5));
    }

    #[test]
    fn forced_vertical_render_keeps_selected_row_and_details_visible() {
        let rendered = rendered_config(
            80,
            40,
            crate::config::UiLayoutMode::Vertical,
            ConfigPane::Settings,
            6,
        );

        assert!(rendered.contains("Global Upload Limit"));
        assert!(rendered.contains("Global Upload Limit · Downloads"));
        assert!(rendered.contains("▶ Global Upload Limit"));
        assert!(!rendered.contains("Settings ·"));
    }

    #[test]
    fn compact_render_uses_one_full_height_pane_at_a_time() {
        let settings_rendered = rendered_config(
            50,
            14,
            crate::config::UiLayoutMode::Vertical,
            ConfigPane::Settings,
            0,
        );
        let details_rendered = rendered_config(
            50,
            14,
            crate::config::UiLayoutMode::Vertical,
            ConfigPane::Details,
            0,
        );

        assert!(settings_rendered.contains("▶ Listen Port"));
        assert!(!settings_rendered.contains("Settings ·"));
        assert!(!settings_rendered.contains("Listen Port · Network"));
        assert!(!settings_rendered.contains(&Settings::default().client_port.to_string()));
        assert!(!settings_rendered.contains("Unlimited"));
        assert!(details_rendered.contains("Listen Port · Network"));
        assert!(!details_rendered.contains("Settings ·"));
        assert!(settings_rendered.contains("[Space] edit"));
        assert!(details_rendered.contains("[Esc] settings"));
    }

    #[test]
    fn tiny_render_is_safe() {
        let rendered = rendered_config(
            10,
            4,
            crate::config::UiLayoutMode::Vertical,
            ConfigPane::Settings,
            0,
        );

        assert!(!rendered.is_empty());
    }

    #[test]
    fn config_list_viewport_keeps_selected_category_and_setting_visible() {
        let rows = config_list_rows(&config_items(), &Settings::default());
        let (start, end) = config_list_viewport(&rows, 6, 4);
        let visible = &rows[start..end];

        assert_eq!(
            visible[0],
            ConfigListRow::Category(ConfigCategory::Downloads)
        );
        assert!(visible.contains(&ConfigListRow::Setting {
            global_index: 6,
            item: ConfigItem::GlobalUploadLimit,
        }));
    }

    #[test]
    fn immediate_apply_merges_only_the_completed_setting() {
        let current = Settings {
            ui_layout_mode: crate::config::UiLayoutMode::Vertical,
            global_download_limit_bps: 25_000_000,
            max_connected_peers: 4_321,
            ..Settings::default()
        };
        let mut draft = current.clone();
        draft.client_port = draft.client_port.saturating_add(1);
        draft.randomize_client_port = true;
        draft.watch_folder = Some(std::path::PathBuf::from("/tmp/superseedr-watch"));
        draft.always_show_add_location_prompt = !draft.always_show_add_location_prompt;
        draft.ui_layout_mode = crate::config::UiLayoutMode::Horizontal;
        draft.global_download_limit_bps = 50_000_000;

        draft.max_connected_peers = 2_000;

        let update = merge_config_item_into_current(&draft, &current, ConfigItem::ClientPort, true);

        assert_eq!(update.client_port, draft.client_port);
        assert!(update.randomize_client_port);
        assert_eq!(update.watch_folder, current.watch_folder);
        assert_eq!(
            update.always_show_add_location_prompt,
            current.always_show_add_location_prompt
        );
        assert_eq!(update.ui_layout_mode, current.ui_layout_mode);
        assert_eq!(
            update.global_download_limit_bps,
            current.global_download_limit_bps
        );
        assert_eq!(update.max_connected_peers, current.max_connected_peers);

        let follower_shared_update =
            merge_config_item_into_current(&draft, &current, ConfigItem::UiLayoutMode, true);
        assert_eq!(
            follower_shared_update.ui_layout_mode,
            current.ui_layout_mode
        );

        let leader_update =
            merge_config_item_into_current(&draft, &current, ConfigItem::UiLayoutMode, false);
        assert_eq!(leader_update.ui_layout_mode, draft.ui_layout_mode);
        assert_eq!(
            leader_update.global_download_limit_bps,
            current.global_download_limit_bps
        );
        assert_eq!(
            leader_update.max_connected_peers,
            current.max_connected_peers
        );

        let path_update =
            merge_config_item_into_current(&draft, &current, ConfigItem::WatchFolder, false);
        assert_eq!(path_update.watch_folder, draft.watch_folder);
        assert_eq!(path_update.ui_layout_mode, current.ui_layout_mode);
    }

    #[test]
    fn reducer_move_down_is_clamped() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = None;

        for _ in 0..10 {
            let _ = reduce_config_action(
                ConfigAction::MoveDown,
                &mut settings,
                &mut idx,
                items.as_mut_slice(),
                &mut editing,
            );
        }

        assert_eq!(idx, 3);
    }

    #[test]
    fn reducer_edit_commit_requests_immediate_download_limit_apply() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 5usize;
        let mut items = config_items();
        let mut editing = Some(editor(ConfigItem::GlobalDownloadLimit, "123"));

        let out = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert_eq!(settings.global_download_limit_bps, 123);
        assert_eq!(editing, None);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
    }

    #[test]
    fn reducer_numeric_port_edit_disables_randomization() {
        let mut settings = Box::new(Settings {
            randomize_client_port: true,
            ..Settings::default()
        });
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = Some(editor(ConfigItem::ClientPort, "7123"));

        let out = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert_eq!(settings.client_port, 7123);
        assert!(!settings.randomize_client_port);
        assert_eq!(editing, None);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
    }

    #[test]
    fn reducer_random_port_edit_enables_randomization() {
        let mut settings = Box::new(Settings::default());
        let original_port = settings.client_port;
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = Some(editor(ConfigItem::ClientPort, "random"));

        let out = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert_eq!(settings.client_port, original_port);
        assert!(settings.randomize_client_port);
        assert_eq!(editing, None);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
    }

    #[test]
    fn reducer_boolean_row_accepts_toggle_true_and_false() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 4usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert!(out.consumed);
        assert!(settings.always_show_add_location_prompt);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));

        let out = reduce_config_action(
            ConfigAction::SetSelectedBool(false),
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert!(out.consumed);
        assert!(!settings.always_show_add_location_prompt);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));

        let out = reduce_config_action(
            ConfigAction::SetSelectedBool(true),
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert!(out.consumed);
        assert!(settings.always_show_add_location_prompt);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
    }

    #[test]
    fn space_opens_global_rate_limit_exact_editor() {
        let mut settings = Box::new(Settings::default());
        let mut selected_index = 5usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );

        assert!(out.consumed);
        assert!(out.effects.is_empty());
        assert_eq!(
            editing,
            Some(ConfigEditState {
                item: ConfigItem::GlobalDownloadLimit,
                buffer: "Unlimited".to_string(),
                cursor: "Unlimited".len(),
                select_all: true,
            })
        );
    }

    #[test]
    fn space_opens_listen_port_exact_editor() {
        let mut settings = Box::new(Settings::default());
        let mut selected_index = 0usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );

        assert!(out.consumed);
        assert!(out.effects.is_empty());
        assert_eq!(
            editing,
            Some(ConfigEditState {
                item: ConfigItem::ClientPort,
                buffer: settings.client_port.to_string(),
                cursor: settings.client_port.to_string().len(),
                select_all: true,
            })
        );
    }

    #[test]
    fn space_opens_path_browser() {
        let mut settings = Box::new(Settings::default());
        let mut selected_index = 2usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );

        assert!(out.consumed);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::AppCommand(_)]
        ));
        assert!(editing.is_none());
    }

    #[test]
    fn completed_toggle_returns_an_immediate_settings_update() {
        let applied = Settings::default();
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut selected_index = 4usize;
        let mut items = config_items();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = None;
        let mut file_browser_generation = 0;
        let (app_command_tx, _app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        let update = handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(KeyCode::Char(
                ' ',
            ))),
            ConfigHandleContext {
                mode: &mut mode,
                anonymize: &mut false,
                settings_edit: &mut settings_edit,
                applied_settings: &applied,
                selected_index: &mut selected_index,
                items: items.as_mut_slice(),
                active_pane: &mut active_pane,
                editing: &mut editing,
                reset_confirmation: &mut reset_confirmation,
                shared_follower: false,
                compact: false,
                app_command_tx: &app_command_tx,
                shutdown_tx: &shutdown_tx,
                file_browser_generation: &mut file_browser_generation,
            },
        )
        .expect("completed toggle should apply immediately");

        assert!(update.always_show_add_location_prompt);
        assert_eq!(update.client_port, applied.client_port);
        assert_eq!(active_pane, ConfigPane::Settings);
    }

    #[test]
    fn space_advances_layout_mode_and_applies_immediately() {
        let applied = Settings::default();
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut selected_index = 3usize;
        let mut items = config_items();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = None;
        let mut file_browser_generation = 0;
        let (app_command_tx, _app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        let update = handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(KeyCode::Char(
                ' ',
            ))),
            ConfigHandleContext {
                mode: &mut mode,
                anonymize: &mut false,
                settings_edit: &mut settings_edit,
                applied_settings: &applied,
                selected_index: &mut selected_index,
                items: items.as_mut_slice(),
                active_pane: &mut active_pane,
                editing: &mut editing,
                reset_confirmation: &mut reset_confirmation,
                shared_follower: false,
                compact: false,
                app_command_tx: &app_command_tx,
                shutdown_tx: &shutdown_tx,
                file_browser_generation: &mut file_browser_generation,
            },
        )
        .expect("completed layout shift should apply immediately");

        assert_eq!(
            update.ui_layout_mode,
            crate::config::UiLayoutMode::Horizontal
        );
        assert_eq!(update.client_port, applied.client_port);
        assert_eq!(active_pane, ConfigPane::Settings);
    }

    #[test]
    fn compact_enter_and_e_do_not_open_details_or_activate_the_control() {
        let applied = Settings::default();
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut selected_index = 0usize;
        let mut items = config_items();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = None;
        let mut file_browser_generation = 0;
        let (app_command_tx, _app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        for key_code in [KeyCode::Enter, KeyCode::Char('e')] {
            let update = handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(key_code)),
                ConfigHandleContext {
                    mode: &mut mode,
                    anonymize: &mut false,
                    settings_edit: &mut settings_edit,
                    applied_settings: &applied,
                    selected_index: &mut selected_index,
                    items: items.as_mut_slice(),
                    active_pane: &mut active_pane,
                    editing: &mut editing,
                    reset_confirmation: &mut reset_confirmation,
                    shared_follower: false,
                    compact: true,
                    app_command_tx: &app_command_tx,
                    shutdown_tx: &shutdown_tx,
                    file_browser_generation: &mut file_browser_generation,
                },
            );

            assert!(update.is_none());
            assert_eq!(active_pane, ConfigPane::Settings);
            assert!(editing.is_none());
        }
    }

    #[tokio::test]
    async fn compact_space_opens_path_browser_and_details() {
        let applied = Settings::default();
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut selected_index = 2usize;
        let mut items = config_items();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = None;
        let mut file_browser_generation = 0;
        let (app_command_tx, mut app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        let update = handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(KeyCode::Char(
                ' ',
            ))),
            ConfigHandleContext {
                mode: &mut mode,
                anonymize: &mut false,
                settings_edit: &mut settings_edit,
                applied_settings: &applied,
                selected_index: &mut selected_index,
                items: items.as_mut_slice(),
                active_pane: &mut active_pane,
                editing: &mut editing,
                reset_confirmation: &mut reset_confirmation,
                shared_follower: false,
                compact: true,
                app_command_tx: &app_command_tx,
                shutdown_tx: &shutdown_tx,
                file_browser_generation: &mut file_browser_generation,
            },
        );

        assert!(update.is_none());
        assert_eq!(active_pane, ConfigPane::Details);
        assert_eq!(file_browser_generation, 1);

        assert!(matches!(
            app_command_rx.recv().await,
            Some(AppCommand::FetchFileTree {
                browser_generation: 1,
                browser_mode: FileBrowserMode::ConfigPathSelection {
                    target_item: ConfigItem::WatchFolder,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn tab_and_backtab_do_not_switch_config_panes() {
        let applied = Settings::default();
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut selected_index = 0usize;
        let mut items = config_items();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = None;
        let mut file_browser_generation = 0;
        let (app_command_tx, _app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        for key_code in [KeyCode::Tab, KeyCode::BackTab] {
            let update = handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(key_code)),
                ConfigHandleContext {
                    mode: &mut mode,
                    anonymize: &mut false,
                    settings_edit: &mut settings_edit,
                    applied_settings: &applied,
                    selected_index: &mut selected_index,
                    items: items.as_mut_slice(),
                    active_pane: &mut active_pane,
                    editing: &mut editing,
                    reset_confirmation: &mut reset_confirmation,
                    shared_follower: false,
                    compact: false,
                    app_command_tx: &app_command_tx,
                    shutdown_tx: &shutdown_tx,
                    file_browser_generation: &mut file_browser_generation,
                },
            );

            assert!(update.is_none());
            assert_eq!(active_pane, ConfigPane::Settings);
        }
    }

    #[test]
    fn reset_requires_confirmation_before_applying_default() {
        let applied = Settings {
            client_port: 7123,
            ..Settings::default()
        };
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut selected_index = 0usize;
        let mut items = config_items();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = None;
        let mut file_browser_generation = 0;
        let (app_command_tx, _app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        let request_update = handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(KeyCode::Char(
                'r',
            ))),
            ConfigHandleContext {
                mode: &mut mode,
                anonymize: &mut false,
                settings_edit: &mut settings_edit,
                applied_settings: &applied,
                selected_index: &mut selected_index,
                items: items.as_mut_slice(),
                active_pane: &mut active_pane,
                editing: &mut editing,
                reset_confirmation: &mut reset_confirmation,
                shared_follower: false,
                compact: false,
                app_command_tx: &app_command_tx,
                shutdown_tx: &shutdown_tx,
                file_browser_generation: &mut file_browser_generation,
            },
        );

        assert!(request_update.is_none());
        assert_eq!(settings_edit.client_port, 7123);
        assert_eq!(reset_confirmation, Some(ConfigItem::ClientPort));

        let confirmed_update = handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(KeyCode::Char(
                'Y',
            ))),
            ConfigHandleContext {
                mode: &mut mode,
                anonymize: &mut false,
                settings_edit: &mut settings_edit,
                applied_settings: &applied,
                selected_index: &mut selected_index,
                items: items.as_mut_slice(),
                active_pane: &mut active_pane,
                editing: &mut editing,
                reset_confirmation: &mut reset_confirmation,
                shared_follower: false,
                compact: false,
                app_command_tx: &app_command_tx,
                shutdown_tx: &shutdown_tx,
                file_browser_generation: &mut file_browser_generation,
            },
        )
        .expect("confirmed reset should apply immediately");

        assert_eq!(
            confirmed_update.client_port,
            Settings::default().client_port
        );
        assert_eq!(settings_edit.client_port, Settings::default().client_port);
        assert_eq!(reset_confirmation, None);
    }

    #[test]
    fn escape_cancels_reset_confirmation_without_changing_setting() {
        let applied = Settings {
            client_port: 7123,
            ..Settings::default()
        };
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut selected_index = 0usize;
        let mut items = config_items();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = Some(ConfigItem::ClientPort);
        let mut file_browser_generation = 0;
        let (app_command_tx, _app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        let update = handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(KeyCode::Esc)),
            ConfigHandleContext {
                mode: &mut mode,
                anonymize: &mut false,
                settings_edit: &mut settings_edit,
                applied_settings: &applied,
                selected_index: &mut selected_index,
                items: items.as_mut_slice(),
                active_pane: &mut active_pane,
                editing: &mut editing,
                reset_confirmation: &mut reset_confirmation,
                shared_follower: false,
                compact: false,
                app_command_tx: &app_command_tx,
                shutdown_tx: &shutdown_tx,
                file_browser_generation: &mut file_browser_generation,
            },
        );

        assert!(update.is_none());
        assert_eq!(settings_edit.client_port, 7123);
        assert_eq!(reset_confirmation, None);
        assert!(matches!(mode, AppMode::Config));
    }

    #[test]
    fn space_is_the_only_primary_control_action() {
        assert_eq!(map_key_to_config_action(KeyCode::Char('e'), &None), None);
        assert_eq!(map_key_to_config_action(KeyCode::Enter, &None), None);
        assert_eq!(
            map_key_to_config_action(KeyCode::Char(' '), &None),
            Some(ConfigAction::ShiftSelected)
        );
        let editing = Some(editor(ConfigItem::ClientPort, "6681"));
        assert_eq!(map_key_to_config_action(KeyCode::Char('e'), &editing), None);
        assert_eq!(
            map_key_to_config_action(KeyCode::Enter, &editing),
            Some(ConfigAction::EditCommit)
        );
    }

    #[test]
    fn config_exit_shortcuts_are_mapped_and_tab_is_ignored() {
        assert_eq!(
            map_key_to_config_action(KeyCode::Esc, &None),
            Some(ConfigAction::Exit)
        );
        assert_eq!(
            map_key_to_config_action(KeyCode::Char('q'), &None),
            Some(ConfigAction::Exit)
        );
        assert_eq!(
            map_key_to_config_action(KeyCode::Char('x'), &None),
            Some(ConfigAction::ToggleAnonymize)
        );
        assert_eq!(map_key_to_config_action(KeyCode::Char('s'), &None), None);
        assert_eq!(map_key_to_config_action(KeyCode::Tab, &None), None);
        assert_eq!(map_key_to_config_action(KeyCode::BackTab, &None), None);
    }

    #[test]
    fn x_toggles_config_anonymization_without_changing_settings() {
        let applied = Settings::default();
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut anonymize = false;
        let mut selected_index = 0usize;
        let mut items = config_items();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = None;
        let mut file_browser_generation = 0;
        let (app_command_tx, _app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        let update = handle_event(
            CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(KeyCode::Char(
                'x',
            ))),
            ConfigHandleContext {
                mode: &mut mode,
                anonymize: &mut anonymize,
                settings_edit: &mut settings_edit,
                applied_settings: &applied,
                selected_index: &mut selected_index,
                items: items.as_mut_slice(),
                active_pane: &mut active_pane,
                editing: &mut editing,
                reset_confirmation: &mut reset_confirmation,
                shared_follower: false,
                compact: false,
                app_command_tx: &app_command_tx,
                shutdown_tx: &shutdown_tx,
                file_browser_generation: &mut file_browser_generation,
            },
        );

        assert!(update.is_none());
        assert!(anonymize);
        assert_eq!(settings_edit.client_port, applied.client_port);
        assert!(matches!(mode, AppMode::Config));
    }

    #[test]
    fn exiting_config_invalidates_pending_file_browser_fetches() {
        let mut mode = AppMode::Config;
        let mut generation = 41;

        exit_config(&mut mode, &mut generation);

        assert!(matches!(mode, AppMode::Normal));
        assert_eq!(generation, 42);
    }

    #[test]
    fn control_shortcuts_only_apply_to_compatible_settings() {
        assert!(action_supported_for_item(
            &ConfigAction::ShiftSelected,
            ConfigItem::AlwaysShowAddLocationPrompt,
        ));
        assert!(action_supported_for_item(
            &ConfigAction::ShiftSelected,
            ConfigItem::ClientPort,
        ));
        assert!(action_supported_for_item(
            &ConfigAction::ShiftSelected,
            ConfigItem::UiLayoutMode,
        ));
        assert!(action_supported_for_item(
            &ConfigAction::ShiftSelected,
            ConfigItem::GlobalDownloadLimit,
        ));
        assert!(action_supported_for_item(
            &ConfigAction::ShiftSelected,
            ConfigItem::WatchFolder,
        ));
        assert!(action_supported_for_item(
            &ConfigAction::IncreaseSelected,
            ConfigItem::UiLayoutMode,
        ));
        assert!(!action_supported_for_item(
            &ConfigAction::IncreaseSelected,
            ConfigItem::GlobalDownloadLimit,
        ));
        assert!(!action_supported_for_item(
            &ConfigAction::IncreaseSelected,
            ConfigItem::WatchFolder,
        ));
    }

    #[test]
    fn layout_choice_navigation_follows_the_visual_order() {
        use crate::config::UiLayoutMode;

        assert_eq!(UiLayoutMode::Auto.next(), UiLayoutMode::Horizontal);
        assert_eq!(UiLayoutMode::Horizontal.next(), UiLayoutMode::Vertical);
        assert_eq!(UiLayoutMode::Vertical.next(), UiLayoutMode::Square);
        assert_eq!(UiLayoutMode::Square.next(), UiLayoutMode::Auto);

        assert_eq!(UiLayoutMode::Auto.previous(), UiLayoutMode::Square);
        assert_eq!(UiLayoutMode::Square.previous(), UiLayoutMode::Vertical);
        assert_eq!(UiLayoutMode::Vertical.previous(), UiLayoutMode::Horizontal);
        assert_eq!(UiLayoutMode::Horizontal.previous(), UiLayoutMode::Auto);
    }

    #[test]
    fn rate_parser_accepts_human_units_and_unlimited() {
        assert_eq!(parse_rate_limit_input("25 Mbps"), Some(25_000_000));
        assert_eq!(parse_rate_limit_input("1.5 Gbps"), Some(1_500_000_000));
        assert_eq!(parse_rate_limit_input("1.5 Tbps"), Some(1_500_000_000_000));
        assert_eq!(
            parse_rate_limit_input("1.5 Pbps"),
            Some(1_500_000_000_000_000)
        );
        assert_eq!(
            parse_rate_limit_input("1.5 Ebps"),
            Some(1_500_000_000_000_000_000)
        );
        assert_eq!(parse_rate_limit_input("500 Kbps"), Some(500_000));
        assert_eq!(
            parse_rate_limit_input("unlimited"),
            Some(UNLIMITED_RATE_LIMIT_BPS)
        );
        assert_eq!(parse_rate_limit_input("0"), Some(UNLIMITED_RATE_LIMIT_BPS));
        assert_eq!(
            parse_rate_limit_input("18446744073709551615"),
            Some(UNLIMITED_RATE_LIMIT_BPS)
        );
        assert_eq!(parse_rate_limit_input("12 MB/s"), None);
    }

    #[test]
    fn exact_editor_prefills_and_selects_the_current_value() {
        let mut settings = Box::new(Settings::default());
        settings.client_port = 7123;
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert!(out.effects.is_empty());
        assert_eq!(
            editing,
            Some(ConfigEditState {
                item: ConfigItem::ClientPort,
                buffer: "7123".to_string(),
                cursor: 4,
                select_all: true,
            })
        );
    }

    #[test]
    fn exact_editor_prefills_random_for_randomized_port_mode() {
        let mut settings = Box::new(Settings {
            randomize_client_port: true,
            ..Settings::default()
        });
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert!(out.effects.is_empty());
        assert_eq!(
            editing,
            Some(ConfigEditState {
                item: ConfigItem::ClientPort,
                buffer: "RANDOM".to_string(),
                cursor: 6,
                select_all: true,
            })
        );
    }

    #[test]
    fn first_typed_character_replaces_selected_editor_value() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = Some(ConfigEditState {
            item: ConfigItem::ClientPort,
            buffer: "6681".to_string(),
            cursor: 4,
            select_all: true,
        });

        reduce_config_action(
            ConfigAction::EditInsert('7'),
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        let editing = editing.expect("editor remains open");
        assert_eq!(editing.buffer, "7");
        assert_eq!(editing.cursor, 1);
        assert!(!editing.select_all);
    }

    #[test]
    fn pasted_editor_text_replaces_selection_and_filters_control_characters() {
        let mut port_editor = ConfigEditState {
            item: ConfigItem::ClientPort,
            buffer: "6681".to_string(),
            cursor: 4,
            select_all: true,
        };
        let port_item = port_editor.item;
        for character in " 71x23\n"
            .chars()
            .filter(|character| edit_character_allowed(port_item, *character))
        {
            insert_editor_character(&mut port_editor, character);
        }
        assert_eq!(port_editor.buffer, "7123");

        let mut rate_editor = ConfigEditState {
            item: ConfigItem::GlobalDownloadLimit,
            buffer: "Unlimited".to_string(),
            cursor: 9,
            select_all: true,
        };
        let rate_item = rate_editor.item;
        for character in "25 Mbps\n"
            .chars()
            .filter(|character| edit_character_allowed(rate_item, *character))
        {
            insert_editor_character(&mut rate_editor, character);
        }
        assert_eq!(rate_editor.buffer, "25 Mbps");
    }

    #[test]
    fn exact_editor_supports_cursor_navigation_and_deletion() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = Some(editor(ConfigItem::ClientPort, "7123"));

        for action in [
            ConfigAction::EditMoveLeft,
            ConfigAction::EditMoveLeft,
            ConfigAction::EditDelete,
            ConfigAction::EditBackspace,
        ] {
            reduce_config_action(
                action,
                &mut settings,
                &mut idx,
                items.as_mut_slice(),
                &mut editing,
            );
        }

        let editing = editing.expect("editor remains open");
        assert_eq!(editing.buffer, "73");
        assert_eq!(editing.cursor, 1);
    }

    #[test]
    fn exact_editor_draws_selection_and_field_chrome() {
        let ctx = test_theme_context();
        let mut editing = editor(ConfigItem::ClientPort, "7123");
        editing.select_all = true;
        let lines = plain_lines(&edit_field_lines(&editing, 40, &ctx));
        let rendered = lines.join("\n");

        assert!(rendered.contains("╭"));
        assert!(rendered.contains("7123"));
        assert!(rendered.contains("type to replace"));
        assert!(rendered.contains("╰"));
        assert!(lines.iter().all(|line| line.chars().count() == 40));
    }

    #[test]
    fn port_edit_validation_rejects_zero_and_out_of_range_values() {
        assert_eq!(
            port_edit_status_message(""),
            "Type a listen port or RANDOM."
        );
        assert_eq!(
            port_edit_status_message("0"),
            "Use RANDOM to select an available port at startup."
        );
        assert_eq!(
            port_edit_status_message("7123"),
            "Ready to apply port 7123."
        );
        assert_eq!(
            port_edit_status_message("random"),
            "Ready to select a new port on each start."
        );
    }

    #[test]
    fn reducer_invalid_port_edit_stays_in_edit_mode() {
        let mut settings = Box::new(Settings::default());
        let original_port = settings.client_port;
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = Some(editor(ConfigItem::ClientPort, "0"));

        let out = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert!(out.consumed);
        assert_eq!(settings.client_port, original_port);
        assert_eq!(editing, Some(editor(ConfigItem::ClientPort, "0")));
    }

    #[test]
    fn network_binding_controls_update_only_the_selected_host_setting() {
        let mut draft = Box::new(Settings::default());
        let current = Settings::default();
        let mut selected_index = 0;
        let mut mode_items = [ConfigItem::NetworkBindingMode];
        let mut editing = None;

        let result = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut draft,
            &mut selected_index,
            &mut mode_items,
            &mut editing,
        );
        assert!(matches!(
            result.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
        assert_eq!(draft.network_binding.mode, NetworkBindingMode::Interface);

        let update =
            merge_config_item_into_current(&draft, &current, ConfigItem::NetworkBindingMode, false);
        assert_eq!(update.network_binding.mode, NetworkBindingMode::Interface);
        assert_eq!(update.network_binding.interface, None);
        assert_eq!(update.network_binding.dns_policy, DnsPolicy::System);
    }

    #[test]
    fn switching_from_interface_to_local_address_clears_hidden_interface() {
        let mut draft = Box::new(Settings::default());
        draft.network_binding.mode = NetworkBindingMode::Interface;
        draft.network_binding.interface = Some("interface-test0".to_string());
        let current = (*draft).clone();
        let mut selected_index = 0;
        let mut mode_items = [ConfigItem::NetworkBindingMode];
        let mut editing = None;

        let result = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut draft,
            &mut selected_index,
            &mut mode_items,
            &mut editing,
        );

        assert!(matches!(
            result.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
        assert_eq!(draft.network_binding.mode, NetworkBindingMode::LocalAddress);
        assert_eq!(draft.network_binding.interface, None);

        let update =
            merge_config_item_into_current(&draft, &current, ConfigItem::NetworkBindingMode, false);
        assert_eq!(
            update.network_binding.mode,
            NetworkBindingMode::LocalAddress
        );
        assert_eq!(update.network_binding.interface, None);
    }

    #[test]
    fn returning_to_normal_routing_restores_system_dns_atomically() {
        let mut draft = Box::new(Settings::default());
        draft.network_binding.mode = NetworkBindingMode::Interface;
        draft.network_binding.dns_policy = DnsPolicy::Bound;
        let current = (*draft).clone();
        let mut selected_index = 0;
        let mut mode_items = [ConfigItem::NetworkBindingMode];
        let mut editing = None;

        let result = reduce_config_action(
            ConfigAction::DecreaseSelected,
            &mut draft,
            &mut selected_index,
            &mut mode_items,
            &mut editing,
        );

        assert!(matches!(
            result.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
        assert_eq!(draft.network_binding.mode, NetworkBindingMode::Any);
        assert_eq!(draft.network_binding.dns_policy, DnsPolicy::System);

        let update =
            merge_config_item_into_current(&draft, &current, ConfigItem::NetworkBindingMode, false);
        assert_eq!(update.network_binding.mode, NetworkBindingMode::Any);
        assert_eq!(update.network_binding.dns_policy, DnsPolicy::System);
    }

    #[test]
    fn bound_dns_editor_accepts_only_literal_socket_addresses() {
        let mut settings = Box::new(Settings::default());
        let mut selected_index = 0;
        let mut items = [ConfigItem::NetworkDnsServers];
        let mut editing = Some(editor(
            ConfigItem::NetworkDnsServers,
            "192.0.2.53:53, [2001:db8::53]:53",
        ));

        let result = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut selected_index,
            &mut items,
            &mut editing,
        );

        assert!(matches!(
            result.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
        assert!(editing.is_none());
        assert_eq!(settings.network_binding.dns_servers.len(), 2);

        let mut invalid_edit = Some(editor(ConfigItem::NetworkDnsServers, "resolver.example:53"));
        let result = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut selected_index,
            &mut items,
            &mut invalid_edit,
        );
        assert!(result.effects.is_empty());
        assert!(invalid_edit.is_some());
        assert_eq!(settings.network_binding.dns_servers.len(), 2);
    }

    #[test]
    fn network_detail_panel_exposes_blocked_runtime_reason() {
        let mut settings = Settings::default();
        settings.network_binding.mode = NetworkBindingMode::Interface;
        settings.network_binding.interface = Some("interface-test".to_string());
        let app_state = AppState {
            network_runtime_status: Some(crate::networking::NetworkRuntimeStatus {
                phase: NetworkRuntimePhase::Blocked,
                mode: NetworkBindingMode::Interface,
                interface: Some("interface-test".to_string()),
                interface_index: None,
                enable_ipv4: true,
                enable_ipv6: false,
                selected_ipv4_address: None,
                selected_ipv6_address: None,
                interface_ipv4_addresses: Vec::new(),
                interface_ipv6_addresses: Vec::new(),
                dns_policy: DnsPolicy::System,
                dns_servers: Vec::new(),
                generation_id: None,
                config_epoch: None,
                blocked_reason: Some("permission denied".to_string()),
                warning: Some("System DNS warning".to_string()),
            }),
            ..AppState::default()
        };
        let dht_status = DhtStatus::default();
        let dht_wave_telemetry = DhtWaveTelemetry::default();
        let theme = test_theme_context();
        let screen = ScreenContext::new(
            &app_state,
            &dht_status,
            &dht_wave_telemetry,
            &settings,
            &theme,
        );
        let editing = None;
        let render_ctx = ConfigRenderContext {
            screen: &screen,
            settings: &settings,
            editing: &editing,
            layout_kind: ConfigLayoutKind::Wide,
            terminal_area: Rect::new(0, 0, 120, 30),
            shared_follower: false,
        };

        let rendered = plain_lines(&build_network_binding_detail_lines(
            ConfigItem::NetworkInterface,
            &render_ctx,
            60,
        ))
        .join("\n");

        assert!(rendered.contains("Blocked"));
        assert!(rendered.contains("interface-test"));
        assert!(rendered.contains("permission denied"));
        assert!(rendered.contains("System DNS warning"));
    }
}
