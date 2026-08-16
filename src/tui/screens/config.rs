// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppCommand, AppMode, ConfigEditState, ConfigItem, ConfigPane, FileBrowserMode};
use crate::config::Settings;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::spawn_app_command_sender;
use crate::tui::formatters::{
    format_bytes, format_limit_bps, format_speed, path_to_string, sanitize_text,
    truncate_with_ellipsis,
};
use crate::tui::layout::config::{calculate_config_layout, ConfigLayoutKind};
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::input_panel::draw_prompt_panel_with_cursor;
use directories::UserDirs;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Frame, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use tokio::sync::{broadcast, mpsc};

const UNLIMITED_RATE_LIMIT_BPS: u64 = crate::config::UNLIMITED_RATE_LIMIT_BPS;
const COUNTRY_CODES: &[&str] = &[
    "AD", "AE", "AF", "AG", "AI", "AL", "AM", "AO", "AQ", "AR", "AS", "AT", "AU", "AW", "AX", "AZ",
    "BA", "BB", "BD", "BE", "BF", "BG", "BH", "BI", "BJ", "BL", "BM", "BN", "BO", "BQ", "BR", "BS",
    "BT", "BV", "BW", "BY", "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI", "CK", "CL", "CM", "CN",
    "CO", "CR", "CU", "CV", "CW", "CX", "CY", "CZ", "DE", "DJ", "DK", "DM", "DO", "DZ", "EC", "EE",
    "EG", "EH", "ER", "ES", "ET", "FI", "FJ", "FK", "FM", "FO", "FR", "GA", "GB", "GD", "GE", "GF",
    "GG", "GH", "GI", "GL", "GM", "GN", "GP", "GQ", "GR", "GS", "GT", "GU", "GW", "GY", "HK", "HM",
    "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IM", "IN", "IO", "IQ", "IR", "IS", "IT", "JE", "JM",
    "JO", "JP", "KE", "KG", "KH", "KI", "KM", "KN", "KP", "KR", "KW", "KY", "KZ", "LA", "LB", "LC",
    "LI", "LK", "LR", "LS", "LT", "LU", "LV", "LY", "MA", "MC", "MD", "ME", "MF", "MG", "MH", "MK",
    "ML", "MM", "MN", "MO", "MP", "MQ", "MR", "MS", "MT", "MU", "MV", "MW", "MX", "MY", "MZ", "NA",
    "NC", "NE", "NF", "NG", "NI", "NL", "NO", "NP", "NR", "NU", "NZ", "OM", "PA", "PE", "PF", "PG",
    "PH", "PK", "PL", "PM", "PN", "PR", "PS", "PT", "PW", "PY", "QA", "RE", "RO", "RS", "RU", "RW",
    "SA", "SB", "SC", "SD", "SE", "SG", "SH", "SI", "SJ", "SK", "SL", "SM", "SN", "SO", "SR", "SS",
    "ST", "SV", "SX", "SY", "SZ", "TC", "TD", "TF", "TG", "TH", "TJ", "TK", "TL", "TM", "TN", "TO",
    "TR", "TT", "TV", "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ", "VA", "VC", "VE", "VG", "VI",
    "VN", "VU", "WF", "WS", "XK", "YE", "YT", "ZA", "ZM", "ZW", "ZZ",
];

fn country_name(code: &str) -> &'static str {
    match code {
        "AD" => "Andorra",
        "AE" => "United Arab Emirates",
        "AF" => "Afghanistan",
        "AG" => "Antigua and Barbuda",
        "AI" => "Anguilla",
        "AL" => "Albania",
        "AM" => "Armenia",
        "AO" => "Angola",
        "AQ" => "Antarctica",
        "AR" => "Argentina",
        "AS" => "American Samoa",
        "AT" => "Austria",
        "AU" => "Australia",
        "AW" => "Aruba",
        "AX" => "Aland Islands",
        "AZ" => "Azerbaijan",
        "BA" => "Bosnia and Herzegovina",
        "BB" => "Barbados",
        "BD" => "Bangladesh",
        "BE" => "Belgium",
        "BF" => "Burkina Faso",
        "BG" => "Bulgaria",
        "BH" => "Bahrain",
        "BI" => "Burundi",
        "BJ" => "Benin",
        "BL" => "Saint Barthelemy",
        "BM" => "Bermuda",
        "BN" => "Brunei",
        "BO" => "Bolivia",
        "BQ" => "Caribbean Netherlands",
        "BR" => "Brazil",
        "BS" => "Bahamas",
        "BT" => "Bhutan",
        "BV" => "Bouvet Island",
        "BW" => "Botswana",
        "BY" => "Belarus",
        "BZ" => "Belize",
        "CA" => "Canada",
        "CC" => "Cocos (Keeling) Islands",
        "CD" => "Democratic Republic of the Congo",
        "CF" => "Central African Republic",
        "CG" => "Republic of the Congo",
        "CH" => "Switzerland",
        "CI" => "Cote d'Ivoire",
        "CK" => "Cook Islands",
        "CL" => "Chile",
        "CM" => "Cameroon",
        "CN" => "China",
        "CO" => "Colombia",
        "CR" => "Costa Rica",
        "CU" => "Cuba",
        "CV" => "Cape Verde",
        "CW" => "Curacao",
        "CX" => "Christmas Island",
        "CY" => "Cyprus",
        "CZ" => "Czech Republic",
        "DE" => "Germany",
        "DJ" => "Djibouti",
        "DK" => "Denmark",
        "DM" => "Dominica",
        "DO" => "Dominican Republic",
        "DZ" => "Algeria",
        "EC" => "Ecuador",
        "EE" => "Estonia",
        "EG" => "Egypt",
        "EH" => "Western Sahara",
        "ER" => "Eritrea",
        "ES" => "Spain",
        "ET" => "Ethiopia",
        "FI" => "Finland",
        "FJ" => "Fiji",
        "FK" => "Falkland Islands",
        "FM" => "Micronesia",
        "FO" => "Faroe Islands",
        "FR" => "France",
        "GA" => "Gabon",
        "GB" => "United Kingdom",
        "GD" => "Grenada",
        "GE" => "Georgia",
        "GF" => "French Guiana",
        "GG" => "Guernsey",
        "GH" => "Ghana",
        "GI" => "Gibraltar",
        "GL" => "Greenland",
        "GM" => "Gambia",
        "GN" => "Guinea",
        "GP" => "Guadeloupe",
        "GQ" => "Equatorial Guinea",
        "GR" => "Greece",
        "GS" => "South Georgia and South Sandwich Islands",
        "GT" => "Guatemala",
        "GU" => "Guam",
        "GW" => "Guinea-Bissau",
        "GY" => "Guyana",
        "HK" => "Hong Kong",
        "HM" => "Heard Island and McDonald Islands",
        "HN" => "Honduras",
        "HR" => "Croatia",
        "HT" => "Haiti",
        "HU" => "Hungary",
        "ID" => "Indonesia",
        "IE" => "Ireland",
        "IL" => "Israel",
        "IM" => "Isle of Man",
        "IN" => "India",
        "IO" => "British Indian Ocean Territory",
        "IQ" => "Iraq",
        "IR" => "Iran",
        "IS" => "Iceland",
        "IT" => "Italy",
        "JE" => "Jersey",
        "JM" => "Jamaica",
        "JO" => "Jordan",
        "JP" => "Japan",
        "KE" => "Kenya",
        "KG" => "Kyrgyzstan",
        "KH" => "Cambodia",
        "KI" => "Kiribati",
        "KM" => "Comoros",
        "KN" => "Saint Kitts and Nevis",
        "KP" => "North Korea",
        "KR" => "South Korea",
        "KW" => "Kuwait",
        "KY" => "Cayman Islands",
        "KZ" => "Kazakhstan",
        "LA" => "Laos",
        "LB" => "Lebanon",
        "LC" => "Saint Lucia",
        "LI" => "Liechtenstein",
        "LK" => "Sri Lanka",
        "LR" => "Liberia",
        "LS" => "Lesotho",
        "LT" => "Lithuania",
        "LU" => "Luxembourg",
        "LV" => "Latvia",
        "LY" => "Libya",
        "MA" => "Morocco",
        "MC" => "Monaco",
        "MD" => "Moldova",
        "ME" => "Montenegro",
        "MF" => "Saint Martin",
        "MG" => "Madagascar",
        "MH" => "Marshall Islands",
        "MK" => "North Macedonia",
        "ML" => "Mali",
        "MM" => "Myanmar",
        "MN" => "Mongolia",
        "MO" => "Macau",
        "MP" => "Northern Mariana Islands",
        "MQ" => "Martinique",
        "MR" => "Mauritania",
        "MS" => "Montserrat",
        "MT" => "Malta",
        "MU" => "Mauritius",
        "MV" => "Maldives",
        "MW" => "Malawi",
        "MX" => "Mexico",
        "MY" => "Malaysia",
        "MZ" => "Mozambique",
        "NA" => "Namibia",
        "NC" => "New Caledonia",
        "NE" => "Niger",
        "NF" => "Norfolk Island",
        "NG" => "Nigeria",
        "NI" => "Nicaragua",
        "NL" => "Netherlands",
        "NO" => "Norway",
        "NP" => "Nepal",
        "NR" => "Nauru",
        "NU" => "Niue",
        "NZ" => "New Zealand",
        "OM" => "Oman",
        "PA" => "Panama",
        "PE" => "Peru",
        "PF" => "French Polynesia",
        "PG" => "Papua New Guinea",
        "PH" => "Philippines",
        "PK" => "Pakistan",
        "PL" => "Poland",
        "PM" => "Saint Pierre and Miquelon",
        "PN" => "Pitcairn Islands",
        "PR" => "Puerto Rico",
        "PS" => "Palestine",
        "PT" => "Portugal",
        "PW" => "Palau",
        "PY" => "Paraguay",
        "QA" => "Qatar",
        "RE" => "Reunion",
        "RO" => "Romania",
        "RS" => "Serbia",
        "RU" => "Russia",
        "RW" => "Rwanda",
        "SA" => "Saudi Arabia",
        "SB" => "Solomon Islands",
        "SC" => "Seychelles",
        "SD" => "Sudan",
        "SE" => "Sweden",
        "SG" => "Singapore",
        "SH" => "Saint Helena",
        "SI" => "Slovenia",
        "SJ" => "Svalbard and Jan Mayen",
        "SK" => "Slovakia",
        "SL" => "Sierra Leone",
        "SM" => "San Marino",
        "SN" => "Senegal",
        "SO" => "Somalia",
        "SR" => "Suriname",
        "SS" => "South Sudan",
        "ST" => "Sao Tome and Principe",
        "SV" => "El Salvador",
        "SX" => "Sint Maarten",
        "SY" => "Syria",
        "SZ" => "Eswatini",
        "TC" => "Turks and Caicos Islands",
        "TD" => "Chad",
        "TF" => "French Southern Territories",
        "TG" => "Togo",
        "TH" => "Thailand",
        "TJ" => "Tajikistan",
        "TK" => "Tokelau",
        "TL" => "Timor-Leste",
        "TM" => "Turkmenistan",
        "TN" => "Tunisia",
        "TO" => "Tonga",
        "TR" => "Turkey",
        "TT" => "Trinidad and Tobago",
        "TV" => "Tuvalu",
        "TW" => "Taiwan",
        "TZ" => "Tanzania",
        "UA" => "Ukraine",
        "UG" => "Uganda",
        "UM" => "United States Minor Outlying Islands",
        "US" => "United States",
        "UY" => "Uruguay",
        "UZ" => "Uzbekistan",
        "VA" => "Vatican City",
        "VC" => "Saint Vincent and the Grenadines",
        "VE" => "Venezuela",
        "VG" => "British Virgin Islands",
        "VI" => "United States Virgin Islands",
        "VN" => "Vietnam",
        "VU" => "Vanuatu",
        "WF" => "Wallis and Futuna",
        "WS" => "Samoa",
        "XK" => "Kosovo",
        "YE" => "Yemen",
        "YT" => "Mayotte",
        "ZA" => "South Africa",
        "ZM" => "Zambia",
        "ZW" => "Zimbabwe",
        "ZZ" => "Unknown or unassigned",
        _ => "Unknown",
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConfigAction {
    Exit,
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
    CountryMoveUp,
    CountryMoveDown,
    CountryToggle,
    CountryToggleAll,
    CountryAllOn,
    CountrySearchStart,
    CountrySearchInsert(char),
    CountrySearchBackspace,
    CountrySearchClear,
}

pub enum ConfigEffect {
    AppCommand(Box<AppCommand>),
    ApplySettings,
}

pub struct ConfigHandleContext<'a> {
    pub mode: &'a mut AppMode,
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
            item: ConfigItem::RegionalIpBlocking,
            category: ConfigCategory::Network,
            label: "Regional IP Blocking",
            control: ConfigControlKind::Bool,
            scope: ConfigScope::Shared,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::RegionalBlockedCountries,
            category: ConfigCategory::Network,
            label: "Blocked Countries",
            control: ConfigControlKind::Text,
            scope: ConfigScope::Shared,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::RegionalAutomaticUpdates,
            category: ConfigCategory::Network,
            label: "Automatic GeoIP Updates",
            control: ConfigControlKind::Bool,
            scope: ConfigScope::Shared,
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

fn value_for_item(item: ConfigItem, settings: &Settings) -> String {
    match item {
        ConfigItem::ClientPort if settings.randomize_client_port => {
            format!("Random ({})", settings.client_port)
        }
        ConfigItem::ClientPort => settings.client_port.to_string(),
        ConfigItem::RegionalIpBlocking => enabled_label(settings.regional_ip_blocking.enabled),
        ConfigItem::RegionalBlockedCountries => {
            if settings.regional_ip_blocking.blocked_countries.is_empty() {
                "All ON".to_string()
            } else {
                format!(
                    "OFF: {}",
                    settings.regional_ip_blocking.blocked_countries.join(", ")
                )
            }
        }
        ConfigItem::RegionalAutomaticUpdates => {
            enabled_label(settings.regional_ip_blocking.auto_update)
        }
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

fn config_item_is_dirty(item: ConfigItem, draft: &Settings, applied: &Settings) -> bool {
    match item {
        ConfigItem::ClientPort => {
            draft.client_port != applied.client_port
                || draft.randomize_client_port != applied.randomize_client_port
        }
        ConfigItem::RegionalIpBlocking => {
            draft.regional_ip_blocking.enabled != applied.regional_ip_blocking.enabled
        }
        ConfigItem::RegionalBlockedCountries => {
            draft.regional_ip_blocking.blocked_countries
                != applied.regional_ip_blocking.blocked_countries
        }
        ConfigItem::RegionalAutomaticUpdates => {
            draft.regional_ip_blocking.auto_update != applied.regional_ip_blocking.auto_update
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
        ConfigItem::RegionalIpBlocking => {
            update.regional_ip_blocking.enabled = draft.regional_ip_blocking.enabled;
        }
        ConfigItem::RegionalBlockedCountries => {
            update.regional_ip_blocking.blocked_countries =
                draft.regional_ip_blocking.blocked_countries.clone();
        }
        ConfigItem::RegionalAutomaticUpdates => {
            update.regional_ip_blocking.auto_update = draft.regional_ip_blocking.auto_update;
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

fn enabled_label(enabled: bool) -> String {
    if enabled { "Enabled" } else { "Disabled" }.to_string()
}

fn parse_country_codes_input(input: &str) -> Option<Vec<String>> {
    let countries = input
        .split(',')
        .map(str::trim)
        .filter(|country| !country.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    crate::regional_ip::normalize_country_codes(&countries)
        .ok()
        .map(|countries| countries.into_iter().collect())
}

fn edit_character_allowed(item: ConfigItem, character: char) -> bool {
    match item {
        ConfigItem::ClientPort => {
            character.is_ascii_digit() || "random".contains(character.to_ascii_lowercase())
        }
        ConfigItem::GlobalDownloadLimit | ConfigItem::GlobalUploadLimit => {
            character.is_ascii_alphanumeric() || matches!(character, '.' | ' ' | '/')
        }
        ConfigItem::RegionalBlockedCountries => {
            character.is_ascii_alphabetic() || matches!(character, ',' | ' ')
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

fn country_search_character_allowed(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, ' ' | '-' | '\'')
}

fn filtered_country_indices(query: Option<&str>) -> Vec<usize> {
    let query = query.unwrap_or_default().trim().to_lowercase();
    if query.is_empty() {
        return (0..COUNTRY_CODES.len()).collect();
    }
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut matches = COUNTRY_CODES
        .iter()
        .enumerate()
        .filter_map(|(index, code)| {
            let name = country_name(code);
            let normalized_code = code.to_lowercase();
            let normalized_name = name.to_lowercase();
            let score = if normalized_code == query {
                1_000_000
            } else if normalized_code.contains(&query) {
                900_000
            } else if let Some(position) = normalized_name.find(&query) {
                800_000_i64.saturating_sub(position as i64)
            } else {
                matcher.fuzzy_match(name, &query)?
            };
            Some((index, score))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left_index, left_score), (right_index, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| COUNTRY_CODES[*left_index].cmp(COUNTRY_CODES[*right_index]))
    });
    matches.into_iter().map(|(index, _)| index).collect()
}

fn selected_country_index(editor: &ConfigEditState) -> Option<usize> {
    let matches = filtered_country_indices(editor.country_search.as_deref());
    matches
        .contains(&editor.cursor)
        .then_some(editor.cursor)
        .or_else(|| matches.first().copied())
}

fn align_country_cursor(editor: &mut ConfigEditState) {
    if let Some(index) = selected_country_index(editor) {
        editor.cursor = index;
    }
}

fn move_country_cursor(editor: &mut ConfigEditState, offset: isize) {
    let matches = filtered_country_indices(editor.country_search.as_deref());
    if matches.is_empty() {
        return;
    }
    let position = matches
        .iter()
        .position(|index| *index == editor.cursor)
        .unwrap_or(0);
    let next = position
        .saturating_add_signed(offset)
        .min(matches.len().saturating_sub(1));
    editor.cursor = matches[next];
}

fn country_list_viewport(
    cursor_position: usize,
    match_count: usize,
    visible_countries: usize,
) -> (usize, usize) {
    let visible_countries = visible_countries.max(1);
    let start = (cursor_position / visible_countries) * visible_countries;
    (
        start,
        start.saturating_add(visible_countries).min(match_count),
    )
}

fn map_key_to_config_action(
    key_code: KeyCode,
    editing: &Option<ConfigEditState>,
) -> Option<ConfigAction> {
    if editing
        .as_ref()
        .is_some_and(|editor| editor.item == ConfigItem::RegionalBlockedCountries)
    {
        let search_active = editing
            .as_ref()
            .is_some_and(|editor| editor.country_search.is_some());
        return match key_code {
            KeyCode::Up => Some(ConfigAction::CountryMoveUp),
            KeyCode::Char('k') if !search_active => Some(ConfigAction::CountryMoveUp),
            KeyCode::Down => Some(ConfigAction::CountryMoveDown),
            KeyCode::Char('j') if !search_active => Some(ConfigAction::CountryMoveDown),
            KeyCode::Char(' ') => Some(ConfigAction::CountryToggle),
            KeyCode::Char('a' | 'A') if !search_active => Some(ConfigAction::CountryToggleAll),
            KeyCode::Char('r' | 'R') if !search_active => Some(ConfigAction::CountryAllOn),
            KeyCode::Char('/') if !search_active => Some(ConfigAction::CountrySearchStart),
            KeyCode::Char(c) if search_active && country_search_character_allowed(c) => {
                Some(ConfigAction::CountrySearchInsert(c))
            }
            KeyCode::Backspace if search_active => Some(ConfigAction::CountrySearchBackspace),
            KeyCode::Home => Some(ConfigAction::EditMoveHome),
            KeyCode::End => Some(ConfigAction::EditMoveEnd),
            KeyCode::Esc if search_active => Some(ConfigAction::CountrySearchClear),
            KeyCode::Esc => Some(ConfigAction::EditCancel),
            KeyCode::Enter => Some(ConfigAction::EditCommit),
            _ => None,
        };
    }
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
        KeyCode::Char('/') => Some(ConfigAction::CountrySearchStart),
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
                        country_search: None,
                    });
                }
                ConfigItem::RegionalIpBlocking => {
                    settings_edit.regional_ip_blocking.enabled =
                        !settings_edit.regional_ip_blocking.enabled;
                    result.effects.push(ConfigEffect::ApplySettings);
                }
                ConfigItem::RegionalBlockedCountries => {
                    let buffer = settings_edit
                        .regional_ip_blocking
                        .blocked_countries
                        .join(", ");
                    let cursor = settings_edit
                        .regional_ip_blocking
                        .blocked_countries
                        .first()
                        .and_then(|country| COUNTRY_CODES.iter().position(|code| *code == country))
                        .unwrap_or(0);
                    *editing = Some(ConfigEditState {
                        cursor,
                        buffer,
                        item: ConfigItem::RegionalBlockedCountries,
                        select_all: false,
                        country_search: None,
                    });
                }
                ConfigItem::RegionalAutomaticUpdates => {
                    settings_edit.regional_ip_blocking.auto_update =
                        !settings_edit.regional_ip_blocking.auto_update;
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
                        country_search: None,
                    });
                }
            }
        }
        ConfigAction::SetSelectedBool(value) => {
            result.consumed = true;
            let changed = match items[*selected_index] {
                ConfigItem::AlwaysShowAddLocationPrompt => {
                    let changed = settings_edit.always_show_add_location_prompt != value;
                    settings_edit.always_show_add_location_prompt = value;
                    changed
                }
                ConfigItem::RegionalIpBlocking => {
                    let changed = settings_edit.regional_ip_blocking.enabled != value;
                    settings_edit.regional_ip_blocking.enabled = value;
                    changed
                }
                ConfigItem::RegionalAutomaticUpdates => {
                    let changed = settings_edit.regional_ip_blocking.auto_update != value;
                    settings_edit.regional_ip_blocking.auto_update = value;
                    changed
                }
                _ => false,
            };
            if changed {
                result.effects.push(ConfigEffect::ApplySettings);
            }
        }
        ConfigAction::MoveUp => {
            result.consumed = true;
            *selected_index = previous_visible_setting_index(items, *selected_index);
        }
        ConfigAction::MoveDown => {
            result.consumed = true;
            *selected_index = next_visible_setting_index(items, *selected_index);
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
                ConfigItem::RegionalIpBlocking => {
                    settings_edit.regional_ip_blocking.enabled =
                        default_settings.regional_ip_blocking.enabled;
                }
                ConfigItem::RegionalBlockedCountries => {
                    settings_edit.regional_ip_blocking.blocked_countries =
                        default_settings.regional_ip_blocking.blocked_countries;
                }
                ConfigItem::RegionalAutomaticUpdates => {
                    settings_edit.regional_ip_blocking.auto_update =
                        default_settings.regional_ip_blocking.auto_update;
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
            if items[*selected_index] == ConfigItem::UiLayoutMode {
                settings_edit.ui_layout_mode = settings_edit.ui_layout_mode.next();
                result.effects.push(ConfigEffect::ApplySettings);
            }
        }
        ConfigAction::DecreaseSelected => {
            result.consumed = true;
            if items[*selected_index] == ConfigItem::UiLayoutMode {
                settings_edit.ui_layout_mode = settings_edit.ui_layout_mode.previous();
                result.effects.push(ConfigEffect::ApplySettings);
            }
        }
        ConfigAction::CountryMoveUp => {
            result.consumed = true;
            if let Some(editor) = editing {
                move_country_cursor(editor, -1);
            }
        }
        ConfigAction::CountryMoveDown => {
            result.consumed = true;
            if let Some(editor) = editing {
                move_country_cursor(editor, 1);
            }
        }
        ConfigAction::CountryToggle => {
            result.consumed = true;
            if let Some(editor) = editing {
                let Some(country_index) = selected_country_index(editor) else {
                    return result;
                };
                let code = COUNTRY_CODES[country_index];
                let mut blocked = parse_country_codes_input(&editor.buffer).unwrap_or_default();
                if let Some(index) = blocked.iter().position(|country| country == code) {
                    blocked.remove(index);
                } else {
                    blocked.push(code.to_string());
                    blocked.sort_unstable();
                }
                editor.buffer = blocked.join(", ");
            }
        }
        ConfigAction::CountryAllOn => {
            result.consumed = true;
            if let Some(editor) = editing {
                editor.buffer.clear();
            }
        }
        ConfigAction::CountryToggleAll => {
            result.consumed = true;
            if let Some(editor) = editing {
                let blocked = parse_country_codes_input(&editor.buffer).unwrap_or_default();
                editor.buffer = if blocked.len() == COUNTRY_CODES.len() {
                    String::new()
                } else {
                    COUNTRY_CODES.join(", ")
                };
            }
        }
        ConfigAction::CountrySearchStart => {
            result.consumed = true;
            if editing.is_none()
                && items.get(*selected_index) == Some(&ConfigItem::RegionalBlockedCountries)
            {
                let buffer = settings_edit
                    .regional_ip_blocking
                    .blocked_countries
                    .join(", ");
                let cursor = settings_edit
                    .regional_ip_blocking
                    .blocked_countries
                    .first()
                    .and_then(|country| COUNTRY_CODES.iter().position(|code| *code == country))
                    .unwrap_or(0);
                *editing = Some(ConfigEditState {
                    cursor,
                    buffer,
                    item: ConfigItem::RegionalBlockedCountries,
                    select_all: false,
                    country_search: None,
                });
            }
            if let Some(editor) = editing {
                if editor.item == ConfigItem::RegionalBlockedCountries {
                    editor.country_search = Some(String::new());
                }
            }
        }
        ConfigAction::CountrySearchInsert(character) => {
            result.consumed = true;
            if let Some(editor) = editing {
                if let Some(query) = editor.country_search.as_mut() {
                    query.push(character);
                    align_country_cursor(editor);
                }
            }
        }
        ConfigAction::CountrySearchBackspace => {
            result.consumed = true;
            if let Some(editor) = editing {
                if let Some(query) = editor.country_search.as_mut() {
                    query.pop();
                    align_country_cursor(editor);
                }
            }
        }
        ConfigAction::CountrySearchClear => {
            result.consumed = true;
            if let Some(editor) = editing {
                editor.country_search = None;
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
                editor.cursor = if editor.item == ConfigItem::RegionalBlockedCountries {
                    filtered_country_indices(editor.country_search.as_deref())
                        .first()
                        .copied()
                        .unwrap_or(0)
                } else {
                    0
                };
                editor.select_all = false;
            }
        }
        ConfigAction::EditMoveEnd => {
            result.consumed = true;
            if let Some(editor) = editing {
                editor.cursor = if editor.item == ConfigItem::RegionalBlockedCountries {
                    filtered_country_indices(editor.country_search.as_deref())
                        .last()
                        .copied()
                        .unwrap_or(0)
                } else {
                    editor.buffer.len()
                };
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
                    ConfigItem::RegionalBlockedCountries => {
                        if let Some(countries) = parse_country_codes_input(&editor.buffer) {
                            changed =
                                settings_edit.regional_ip_blocking.blocked_countries != countries;
                            settings_edit.regional_ip_blocking.blocked_countries = countries;
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

fn config_list_rows(items: &[ConfigItem]) -> Vec<ConfigListRow> {
    let mut rows = Vec::new();
    for category in ConfigCategory::all() {
        let category_items = items
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, item)| config_category_for_item(*item) == *category)
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

fn visible_setting_indices(items: &[ConfigItem]) -> Vec<usize> {
    config_list_rows(items)
        .into_iter()
        .filter_map(|row| match row {
            ConfigListRow::Setting { global_index, .. } => Some(global_index),
            ConfigListRow::Category(_) => None,
        })
        .collect()
}

fn next_visible_setting_index(items: &[ConfigItem], selected_index: usize) -> usize {
    let visible_indices = visible_setting_indices(items);
    visible_indices
        .iter()
        .position(|index| *index == selected_index)
        .and_then(|position| visible_indices.get(position + 1).copied())
        .unwrap_or(selected_index)
}

fn previous_visible_setting_index(items: &[ConfigItem], selected_index: usize) -> usize {
    let visible_indices = visible_setting_indices(items);
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

    let rows_model = config_list_rows(items);
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
                let descriptor = descriptor_for_item(*item);
                let is_highlighted = if let Some(editor) = editing {
                    editor.item == *item
                } else {
                    *global_index == selected_index
                };
                let locked = config_item_is_locked(*item, render_ctx.shared_follower);
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
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(if is_highlighted { "▶" } else { " " }, marker_style),
                        Span::raw(" "),
                        Span::styled(descriptor.label, label_style),
                    ])),
                    *row_area,
                );
            }
        }
    }
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

    if let Some(editor) = render_ctx.editing.as_ref().filter(|editor| {
        editor.item == active_item && editor.item == ConfigItem::RegionalBlockedCountries
    }) {
        render_country_selector(f, editor, render_ctx, inner);
        return;
    }

    let mut lines = if let Some(editor) = render_ctx.editing {
        if editor.item == active_item {
            build_edit_detail_lines(editor, render_ctx, inner.width, inner.height)
        } else {
            build_setting_detail_lines(active_item, render_ctx, inner.width)
        }
    } else {
        build_setting_detail_lines(active_item, render_ctx, inner.width)
    };
    if let Some(error) = render_ctx.screen.ui.system_error.as_deref() {
        let status_line = Line::from(vec![
            Span::styled(
                "STATUS  ",
                ctx.apply(Style::default().fg(ctx.state_error()).bold()),
            ),
            Span::styled(
                error.to_string(),
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

fn render_country_selector(
    f: &mut Frame,
    editor: &ConfigEditState,
    render_ctx: &ConfigRenderContext<'_, '_>,
    area: Rect,
) {
    let ctx = render_ctx.screen.theme;
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
    let search_active = editor.country_search.is_some();
    let value = editor
        .country_search
        .as_deref()
        .map(sanitize_text)
        .unwrap_or_default();
    let trailing_spans = if search_active {
        vec![
            Span::raw("  "),
            Span::styled(
                "Fuzzy",
                ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
            ),
        ]
    } else {
        vec![Span::styled(
            "Press / to search",
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        )]
    };
    draw_prompt_panel_with_cursor(
        f,
        chunks[0],
        " Country Search ".to_string(),
        value,
        trailing_spans,
        search_active,
        ctx,
    );

    let lines =
        build_country_selector_detail_lines(editor, render_ctx, chunks[1].width, chunks[1].height);
    f.render_widget(
        Paragraph::new(lines).style(ctx.apply(Style::default().fg(ctx.theme.semantic.text))),
        chunks[1],
    );
}

fn build_setting_detail_lines(
    item: ConfigItem,
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    match item {
        ConfigItem::ClientPort => build_port_detail_lines(render_ctx, width),
        ConfigItem::RegionalIpBlocking
        | ConfigItem::RegionalBlockedCountries
        | ConfigItem::RegionalAutomaticUpdates => {
            build_regional_ip_detail_lines(item, render_ctx, width)
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

fn build_regional_ip_detail_lines(
    item: ConfigItem,
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let settings = &render_ctx.settings.regional_ip_blocking;
    let status = &render_ctx.screen.ui.regional_ip_status;
    let configured = match item {
        ConfigItem::RegionalIpBlocking => enabled_label(settings.enabled),
        ConfigItem::RegionalBlockedCountries => {
            if settings.blocked_countries.is_empty() {
                "All countries ON".to_string()
            } else {
                format!("OFF: {}", settings.blocked_countries.join(", "))
            }
        }
        ConfigItem::RegionalAutomaticUpdates => enabled_label(settings.auto_update),
        _ => unreachable!("regional detail requested for a non-regional setting"),
    };
    let state = match status.state {
        crate::regional_ip::RegionalDatabaseState::Disabled => "Disabled",
        crate::regional_ip::RegionalDatabaseState::Checking => "Checking",
        crate::regional_ip::RegionalDatabaseState::Downloading => "Downloading",
        crate::regional_ip::RegionalDatabaseState::Ready => "Ready",
        crate::regional_ip::RegionalDatabaseState::Error => "Error",
    };
    let state_color = match status.state {
        crate::regional_ip::RegionalDatabaseState::Ready => ctx.state_success(),
        crate::regional_ip::RegionalDatabaseState::Error => ctx.state_error(),
        crate::regional_ip::RegionalDatabaseState::Checking
        | crate::regional_ip::RegionalDatabaseState::Downloading => ctx.state_warning(),
        crate::regional_ip::RegionalDatabaseState::Disabled => ctx.theme.semantic.subtext0,
    };
    let description = match item {
        ConfigItem::RegionalIpBlocking => {
            "Rejects incoming and outgoing peers whose IP belongs to a selected country."
        }
        ConfigItem::RegionalBlockedCountries => {
            "All countries start ON. Open the selector and turn countries OFF to block them."
        }
        ConfigItem::RegionalAutomaticUpdates => {
            "Checks once per day for the current monthly country database on each host."
        }
        _ => unreachable!("regional detail requested for a non-regional setting"),
    };
    let mut lines = vec![
        detail_row(
            "Configured",
            configured,
            ctx.apply(Style::default().fg(ctx.theme.semantic.text).bold()),
            ctx,
        ),
        Line::from(""),
        detail_divider(width, ctx),
        info_note_line(description, ctx),
        Line::from(""),
        info_section_heading("HOST DATABASE", ctx),
        detail_row(
            "State",
            state.to_string(),
            ctx.apply(Style::default().fg(state_color).bold()),
            ctx,
        ),
        detail_row(
            "Release",
            status
                .database_month
                .clone()
                .unwrap_or_else(|| "Not loaded".to_string()),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
            ctx,
        ),
        detail_row(
            "Ranges",
            status.range_count.to_string(),
            ctx.apply(Style::default().fg(ctx.accent_sapphire())),
            ctx,
        ),
    ];
    if let Some(message) = status.message.as_deref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.to_string(),
            ctx.apply(Style::default().fg(state_color)),
        )));
    }
    lines.push(Line::from(""));
    lines.push(info_note_line("GeoIP data by DB-IP (CC BY 4.0).", ctx));
    lines
}

fn build_port_detail_lines(
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
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
    let configured_value = if draft_random {
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
            active.to_string(),
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
            path_to_string(draft),
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
            path_to_string(active.as_deref()),
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
    height: u16,
) -> Vec<Line<'static>> {
    if editor.item == ConfigItem::RegionalBlockedCountries {
        return build_country_selector_detail_lines(editor, render_ctx, width, height);
    }
    let ctx = render_ctx.screen.theme;
    let item = editor.item;
    let buffer = editor.buffer.as_str();
    let current = value_for_item(item, render_ctx.settings);
    let (status, valid) = match item {
        ConfigItem::ClientPort => (
            port_edit_status_message(buffer),
            is_random_port_input(buffer) || buffer.parse::<u16>().is_ok_and(|port| port > 0),
        ),
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
        ConfigItem::RegionalBlockedCountries => {
            let parsed = parse_country_codes_input(buffer);
            (
                parsed
                    .as_ref()
                    .map(|countries| format!("Ready: {}", countries.join(", ")))
                    .unwrap_or_else(|| {
                        "Use comma-separated two-letter codes, for example ES, PT.".to_string()
                    }),
                parsed.is_some(),
            )
        }
        _ => ("Ready".to_string(), true),
    };
    let mut lines = Vec::new();
    lines.extend(edit_field_lines(editor, width, ctx));
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
        info_note_line(
            match item {
                ConfigItem::ClientPort => {
                    "Valid range: 1–65535. Enter validates and updates the runtime listener."
                }
                ConfigItem::RegionalBlockedCountries => {
                    "Enter applies the shared country list. Duplicate codes are removed."
                }
                _ => {
                    "Enter applies the rate in bits per second. Accepted suffixes range from Kbps through Ebps. Zero means unlimited."
                }
            },
            ctx,
        ),
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

fn build_country_selector_detail_lines(
    editor: &ConfigEditState,
    render_ctx: &ConfigRenderContext<'_, '_>,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let ctx = render_ctx.screen.theme;
    let blocked = parse_country_codes_input(&editor.buffer).unwrap_or_default();
    let blocked_count = COUNTRY_CODES
        .iter()
        .filter(|code| blocked.iter().any(|blocked| blocked == *code))
        .count();
    let matches = filtered_country_indices(editor.country_search.as_deref());
    let cursor = selected_country_index(editor);
    let cursor_position = cursor
        .and_then(|cursor| matches.iter().position(|index| *index == cursor))
        .unwrap_or(0);
    let visible_countries = height.saturating_sub(2).max(1) as usize;
    let (start, end) = country_list_viewport(cursor_position, matches.len(), visible_countries);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(
                    "Allowed {}/{}",
                    COUNTRY_CODES.len() - blocked_count,
                    COUNTRY_CODES.len()
                ),
                ctx.apply(Style::default().fg(ctx.state_success()).bold()),
            ),
            Span::styled(
                format!("   Blocked {blocked_count}"),
                ctx.apply(Style::default().fg(if blocked_count == 0 {
                    ctx.theme.semantic.subtext0
                } else {
                    ctx.state_error()
                })),
            ),
            Span::styled(
                format!("   Matches {}", matches.len()),
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
            ),
        ]),
        detail_divider(width, ctx),
    ];
    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "No countries match this search.",
            ctx.apply(Style::default().fg(ctx.state_warning())),
        )));
        return lines;
    }
    for absolute_index in &matches[start..end] {
        let code = COUNTRY_CODES[*absolute_index];
        let selected = Some(*absolute_index) == cursor;
        let is_blocked = blocked.iter().any(|blocked| blocked == code);
        let statistics =
            country_statistics_label(render_ctx.screen.app.state.peer_country_stats.get(code));
        let country_details = format!("{}{}", country_name(code), statistics);
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "▶ " } else { "  " },
                ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
            ),
            Span::styled(
                if is_blocked { "OFF" } else { "ON " },
                ctx.apply(
                    Style::default()
                        .fg(if is_blocked {
                            ctx.state_error()
                        } else {
                            ctx.state_success()
                        })
                        .bold(),
                ),
            ),
            Span::raw("  "),
            Span::styled(
                code.to_string(),
                ctx.apply(
                    Style::default()
                        .fg(if selected {
                            ctx.theme.semantic.text
                        } else {
                            ctx.theme.semantic.subtext1
                        })
                        .bold(),
                ),
            ),
            Span::raw("  "),
            Span::styled(
                truncate_with_ellipsis(&country_details, width.saturating_sub(11) as usize),
                ctx.apply(Style::default().fg(if selected {
                    ctx.theme.semantic.text
                } else {
                    ctx.theme.semantic.subtext1
                })),
            ),
        ]));
    }
    lines
}

fn country_statistics_label(stats: Option<&crate::app::PeerCountryStats>) -> String {
    let Some(stats) = stats.filter(|stats| stats.peer_count > 0) else {
        return " · 0 peers".to_string();
    };
    let peer_label = if stats.peer_count == 1 {
        "peer"
    } else {
        "peers"
    };
    format!(
        " · {} {} · ↓ {} · ↑ {}",
        stats.peer_count,
        peer_label,
        compact_country_bytes(stats.total_downloaded_bytes),
        compact_country_bytes(stats.total_uploaded_bytes)
    )
}

fn compact_country_bytes(bytes: u64) -> String {
    format_bytes(bytes).replace(".00 ", " ").replace(' ', "")
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
    if render_ctx
        .editing
        .as_ref()
        .is_some_and(|editor| editor.item == ConfigItem::RegionalBlockedCountries)
    {
        actions.push(("Space", "toggle", ActionTone::Toggle));
        actions.push(("↑/↓", "country", ActionTone::Navigate));
        if render_ctx
            .editing
            .as_ref()
            .is_some_and(|editor| editor.country_search.is_some())
        {
            actions.push(("type", "search", ActionTone::Edit));
            actions.push(("Esc", "clear search", ActionTone::Clear));
        } else {
            actions.push(("/", "search", ActionTone::Edit));
            actions.push(("a", "all OFF/ON", ActionTone::Toggle));
            actions.push(("r", "defaults", ActionTone::Clear));
            actions.push(("Esc", "cancel", ActionTone::Cancel));
        }
        actions.push(("Enter", "apply", ActionTone::Confirm));
    } else if render_ctx.editing.is_some() {
        actions.push(("Enter", "apply", ActionTone::Confirm));
        actions.push(("Esc", "cancel", ActionTone::Cancel));
        actions.push(("←/→", "cursor", ActionTone::Navigate));
    } else {
        if !locked {
            if active_item == ConfigItem::RegionalBlockedCountries {
                actions.push(("Space", "open", ActionTone::Edit));
                actions.push(("/", "search", ActionTone::Edit));
            } else {
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
            | ConfigAction::CountryToggle
            | ConfigAction::CountryToggleAll
            | ConfigAction::CountryAllOn
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
        ConfigAction::CountrySearchStart => item == ConfigItem::RegionalBlockedCountries,
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
        if editor.item == ConfigItem::RegionalBlockedCountries {
            let query = editor.country_search.get_or_insert_default();
            query.extend(
                text.chars()
                    .filter(|character| country_search_character_allowed(*character)),
            );
            align_country_cursor(editor);
            return None;
        }
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
                && (action_mutates_selected_setting(&action)
                    || action == ConfigAction::CountrySearchStart)
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
            ConfigItem::RegionalIpBlocking,
            ConfigItem::RegionalBlockedCountries,
            ConfigItem::RegionalAutomaticUpdates,
            ConfigItem::DefaultDownloadFolder,
            ConfigItem::WatchFolder,
            ConfigItem::UiLayoutMode,
            ConfigItem::AlwaysShowAddLocationPrompt,
            ConfigItem::GlobalDownloadLimit,
            ConfigItem::GlobalUploadLimit,
        ]
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
            country_search: None,
        }
    }

    fn rendered_config(
        width: u16,
        height: u16,
        layout_mode: crate::config::UiLayoutMode,
        active_pane: ConfigPane,
        selected_index: usize,
    ) -> String {
        rendered_config_with_editing(
            width,
            height,
            layout_mode,
            active_pane,
            selected_index,
            None,
        )
    }

    fn rendered_config_with_editing(
        width: u16,
        height: u16,
        layout_mode: crate::config::UiLayoutMode,
        active_pane: ConfigPane,
        selected_index: usize,
        editing: Option<ConfigEditState>,
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
            ConfigItem::RegionalIpBlocking,
            ConfigItem::RegionalBlockedCountries,
            ConfigItem::RegionalAutomaticUpdates,
            ConfigItem::UiLayoutMode,
            ConfigItem::GlobalDownloadLimit,
            ConfigItem::GlobalUploadLimit,
        ] {
            assert_eq!(descriptor_for_item(item).scope, ConfigScope::Shared);
        }
    }

    #[test]
    fn config_list_rows_group_settings_under_category_headers() {
        let rows = config_list_rows(&config_items());

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
    fn visible_navigation_follows_grouped_category_order() {
        let items = config_items();

        assert_eq!(next_visible_setting_index(&items, 0), 1);
        assert_eq!(next_visible_setting_index(&items, 3), 4);
        assert_eq!(next_visible_setting_index(&items, 9), 6);
        assert_eq!(next_visible_setting_index(&items, 6), 6);
        assert_eq!(previous_visible_setting_index(&items, 6), 9);
        assert_eq!(previous_visible_setting_index(&items, 4), 3);
        assert_eq!(previous_visible_setting_index(&items, 0), 0);
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
    }

    #[test]
    fn country_setting_footer_advertises_direct_search() {
        let rendered = rendered_config(
            120,
            30,
            crate::config::UiLayoutMode::Horizontal,
            ConfigPane::Settings,
            2,
        );

        assert!(rendered.contains("[Space] open"));
        assert!(rendered.contains("[/] search"));

        let selector = rendered_config_with_editing(
            160,
            32,
            crate::config::UiLayoutMode::Horizontal,
            ConfigPane::Details,
            2,
            Some(editor(ConfigItem::RegionalBlockedCountries, "")),
        );
        assert!(selector.contains("[a] all OFF/ON"));
        assert!(selector.contains("[r] defaults"));
    }

    #[test]
    fn detail_panels_keep_controls_above_and_information_below_the_divider() {
        let settings = Settings::default();
        let app_state = AppState {
            externally_accessable_port_v4: true,
            externally_accessable_port_v6: true,
            peer_country_stats: std::collections::HashMap::from([(
                "ES".to_string(),
                crate::app::PeerCountryStats {
                    peer_count: 12,
                    total_downloaded_bytes: 1_024,
                    total_uploaded_bytes: 2_048,
                },
            )]),
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

        let port_editor = editor(ConfigItem::ClientPort, "7123");
        let edit_lines = build_edit_detail_lines(&port_editor, &render_ctx, 60, 24);
        assert_detail_hierarchy(&edit_lines, &["7123"], &["Current", "Valid range"]);
        assert_first_below_divider(&edit_lines, "Valid range");

        let mut country_editor = editor(ConfigItem::RegionalBlockedCountries, "");
        country_editor.cursor = COUNTRY_CODES.iter().position(|code| *code == "ES").unwrap();
        let country_lines =
            build_country_selector_detail_lines(&country_editor, &render_ctx, 60, 24);
        let country_text = plain_lines(&country_lines).join("\n");
        assert_eq!(country_lines.len(), 24);
        assert!(country_text.contains("ES  Spain · 12 peers · ↓ 1KB · ↑ 2KB"));

        let mut lines = build_port_detail_lines(&render_ctx, 60);
        insert_below_detail_divider(&mut lines, Line::from("STATUS  listener update failed"));
        assert_detail_hierarchy(&lines, &["Configured"], &["STATUS", "Runtime"]);
    }

    #[test]
    fn path_footer_uses_the_shared_space_edit_action() {
        let path_index = config_items()
            .iter()
            .position(|item| *item == ConfigItem::DefaultDownloadFolder)
            .unwrap();
        let rendered = rendered_config(
            120,
            30,
            crate::config::UiLayoutMode::Horizontal,
            ConfigPane::Settings,
            path_index,
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
            9,
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
        let rows = config_list_rows(&config_items());
        let (start, end) = config_list_viewport(&rows, 9, 4);
        let visible = &rows[start..end];

        assert_eq!(
            visible[0],
            ConfigListRow::Category(ConfigCategory::Downloads)
        );
        assert!(visible.contains(&ConfigListRow::Setting {
            global_index: 9,
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

        assert_eq!(idx, 6);
    }

    #[test]
    fn reducer_edit_commit_requests_immediate_download_limit_apply() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 8usize;
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
        let mut idx = 7usize;
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
        let mut selected_index = 8usize;
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
                country_search: None,
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
                country_search: None,
            })
        );
    }

    #[test]
    fn space_opens_path_browser() {
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
        let mut selected_index = 7usize;
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
        let mut selected_index = 6usize;
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
        let mut selected_index = 5usize;
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
        assert_eq!(map_key_to_config_action(KeyCode::Char('s'), &None), None);
        assert_eq!(map_key_to_config_action(KeyCode::Tab, &None), None);
        assert_eq!(map_key_to_config_action(KeyCode::BackTab, &None), None);
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
    fn country_parser_normalizes_and_validates_two_letter_codes() {
        assert!(COUNTRY_CODES.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(COUNTRY_CODES
            .iter()
            .all(|code| code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_uppercase())));
        assert!(COUNTRY_CODES
            .iter()
            .all(|code| country_name(code) != "Unknown"));
        assert_eq!(country_name("ES"), "Spain");
        assert_eq!(
            parse_country_codes_input(" pt, ES, pt "),
            Some(vec!["ES".to_string(), "PT".to_string()])
        );
        assert_eq!(parse_country_codes_input(""), Some(Vec::new()));
        assert_eq!(parse_country_codes_input("Spain"), None);
    }

    #[test]
    fn regional_controls_toggle_and_commit_country_list() {
        let mut settings = Box::new(Settings::default());
        let mut items = config_items();
        let mut selected_index = items
            .iter()
            .position(|item| *item == ConfigItem::RegionalIpBlocking)
            .unwrap();
        let mut editing = None;

        let toggle = reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert!(settings.regional_ip_blocking.enabled);
        assert!(matches!(
            toggle.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));

        selected_index = items
            .iter()
            .position(|item| *item == ConfigItem::RegionalBlockedCountries)
            .unwrap();
        editing = Some(editor(ConfigItem::RegionalBlockedCountries, "pt, ES"));
        let countries = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(
            settings.regional_ip_blocking.blocked_countries,
            vec!["ES".to_string(), "PT".to_string()]
        );
        assert!(matches!(
            countries.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
    }

    #[test]
    fn country_selector_defaults_all_on_and_toggles_selected_country_off() {
        let mut settings = Box::new(Settings::default());
        let mut items = config_items();
        let mut selected_index = items
            .iter()
            .position(|item| *item == ConfigItem::RegionalBlockedCountries)
            .unwrap();
        let mut editing = None;

        reduce_config_action(
            ConfigAction::ShiftSelected,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(
            editing.as_ref().map(|editor| editor.buffer.as_str()),
            Some("")
        );

        reduce_config_action(
            ConfigAction::CountryToggle,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(
            editing.as_ref().map(|editor| editor.buffer.as_str()),
            Some("AD")
        );

        assert_eq!(
            map_key_to_config_action(KeyCode::Char('r'), &editing),
            Some(ConfigAction::CountryAllOn)
        );
        reduce_config_action(
            ConfigAction::CountryAllOn,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(
            editing.as_ref().map(|editor| editor.buffer.as_str()),
            Some("")
        );

        assert_eq!(
            map_key_to_config_action(KeyCode::Char('a'), &editing),
            Some(ConfigAction::CountryToggleAll)
        );
        reduce_config_action(
            ConfigAction::CountryToggleAll,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(
            editing
                .as_ref()
                .and_then(|editor| parse_country_codes_input(&editor.buffer))
                .map(|countries| countries.len()),
            Some(COUNTRY_CODES.len())
        );
        reduce_config_action(
            ConfigAction::CountryToggleAll,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(
            editing.as_ref().map(|editor| editor.buffer.as_str()),
            Some("")
        );

        reduce_config_action(
            ConfigAction::CountryToggle,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );

        let result = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(
            settings.regional_ip_blocking.blocked_countries,
            vec!["AD".to_string()]
        );
        assert!(matches!(
            result.effects.as_slice(),
            [ConfigEffect::ApplySettings]
        ));
    }

    #[test]
    fn country_selector_searches_codes_and_full_names_before_toggling() {
        let mut settings = Box::new(Settings::default());
        let mut items = config_items();
        let mut selected_index = items
            .iter()
            .position(|item| *item == ConfigItem::RegionalBlockedCountries)
            .unwrap();
        let mut editing = Some(editor(ConfigItem::RegionalBlockedCountries, ""));

        reduce_config_action(
            ConfigAction::CountrySearchStart,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        for character in "spain".chars() {
            reduce_config_action(
                ConfigAction::CountrySearchInsert(character),
                &mut settings,
                &mut selected_index,
                items.as_mut_slice(),
                &mut editing,
            );
        }

        let editor = editing.as_ref().unwrap();
        assert_eq!(editor.country_search.as_deref(), Some("spain"));
        assert_eq!(COUNTRY_CODES[selected_country_index(editor).unwrap()], "ES");
        let matches = filtered_country_indices(Some("spain"));
        assert_eq!(matches.first(), Some(&editor.cursor));
        assert!(matches.contains(&editor.cursor));
        assert_eq!(
            map_key_to_config_action(KeyCode::Char('r'), &editing),
            Some(ConfigAction::CountrySearchInsert('r'))
        );
        assert_eq!(
            map_key_to_config_action(KeyCode::Char('a'), &editing),
            Some(ConfigAction::CountrySearchInsert('a'))
        );

        reduce_config_action(
            ConfigAction::CountryToggle,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(editing.as_ref().unwrap().buffer, "ES");

        reduce_config_action(
            ConfigAction::CountrySearchClear,
            &mut settings,
            &mut selected_index,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(editing.as_ref().unwrap().country_search, None);
        assert_eq!(
            map_key_to_config_action(KeyCode::Char('r'), &editing),
            Some(ConfigAction::CountryAllOn)
        );
        assert_eq!(country_list_viewport(0, 100, 20), (0, 20));
        assert_eq!(country_list_viewport(19, 100, 20), (0, 20));
        assert_eq!(country_list_viewport(20, 100, 20), (20, 40));
    }

    #[test]
    fn country_search_uses_live_config_events_and_the_shared_search_panel() {
        let applied = Settings::default();
        let mut settings_edit = Box::new(applied.clone());
        let mut mode = AppMode::Config;
        let mut items = config_items();
        let mut selected_index = items
            .iter()
            .position(|item| *item == ConfigItem::RegionalBlockedCountries)
            .unwrap();
        let mut active_pane = ConfigPane::Settings;
        let mut editing = None;
        let mut reset_confirmation = None;
        let mut file_browser_generation = 0;
        let (app_command_tx, _app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        for key_code in [
            KeyCode::Char('/'),
            KeyCode::Char('s'),
            KeyCode::Char('p'),
            KeyCode::Char('n'),
        ] {
            let update = handle_event(
                CrosstermEvent::Key(ratatui::crossterm::event::KeyEvent::from(key_code)),
                ConfigHandleContext {
                    mode: &mut mode,
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
        }

        let editor = editing.as_ref().expect("country search should be open");
        assert_eq!(active_pane, ConfigPane::Details);
        assert_eq!(editor.country_search.as_deref(), Some("spn"));
        assert_eq!(COUNTRY_CODES[selected_country_index(editor).unwrap()], "ES");

        let rendered = rendered_config_with_editing(
            120,
            32,
            crate::config::UiLayoutMode::Horizontal,
            ConfigPane::Details,
            selected_index,
            editing.clone(),
        );
        assert!(rendered.contains("Country Search"));
        assert!(rendered.contains("> spn_"));
        assert!(rendered.contains("Fuzzy"));
        assert!(rendered.contains("ES  Spain"));
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
                country_search: None,
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
                country_search: None,
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
            country_search: None,
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
            country_search: None,
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
            country_search: None,
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
}
