// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(test)]
use crate::app;
use crate::app::{App, AppRuntimeMode};
use rand::{Rng, RngExt};

use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::logging;
#[cfg(feature = "synthetic-load")]
use super::synthetic_load;
use crate::config::{
    self, clear_persisted_host_id, clear_persisted_shared_config, convert_shared_to_standalone,
    convert_standalone_to_shared, effective_host_id_selection, effective_shared_config_selection,
    get_watch_path, is_shared_config_mode, load_settings_for_cli, persisted_host_id_path,
    persisted_shared_config_path, resolve_command_watch_path, set_persisted_host_id,
    set_persisted_shared_config, shared_lock_path, shared_processed_path, shared_root_path,
    HostIdSource, SharedConfigSource,
};
use crate::integrations::cli::{
    command_to_control_requests_with_resolver, expand_add_inputs, require_cli_targets,
    status_command_mode, status_control_request, status_file_modified_at,
    wait_for_status_json_after, write_control_command, write_input_command,
    write_path_command_payload, write_stop_command, Cli, Commands, StatusCommandMode,
};
use crate::integrations::control::service::{
    apply_offline_control_request, apply_offline_purge, build_move_torrent_request,
    control_event_details, list_torrent_files, online_control_success_message,
    prepare_offline_move_transaction, resolve_purge_target_info_hash, resolve_target_info_hash,
};
#[cfg(test)]
use crate::integrations::control::ControlPriorityTarget;
use crate::integrations::control::ControlRequest;
use crate::integrations::status::{offline_output_json, status_file_path};
use crate::networking::dns::validate_bound_dns_servers;
use crate::networking::{self, DnsPolicy, NetworkBindingMode, NetworkInterfaceInfo};
use crate::peer_manager;
use crate::persistence::event_journal::{
    append_event_journal_entry, event_journal_json, load_event_journal_state,
    save_event_journal_state, ControlOrigin, EventCategory, EventDetails, EventJournalEntry,
    EventJournalState, EventScope, EventType, IngestKind,
};
use crate::persistence::AppPersistence;
use crate::torrent_identity::{info_hash_from_torrent_bytes, info_hash_from_torrent_source};
use crate::Settings;
use serde_json::{json, Value};

use ratatui::{backend::CrosstermBackend, Terminal};
use std::env;
use std::io::stdout;

use tracing_subscriber::filter::Targets;
use tracing_subscriber::{filter::LevelFilter, fmt, prelude::*};

use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

#[cfg(not(windows))]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

use clap::Parser;

const DEFAULT_LOG_FILTER: LevelFilter = LevelFilter::INFO;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Text,
    Json,
}

#[derive(Debug, serde::Serialize)]
struct ShowConfigsSnapshot {
    shared_mode: bool,
    host: HostIdentitySnapshot,
    launcher: LauncherPathsSnapshot,
    local: LocalPathsSnapshot,
    effective: EffectivePathsSnapshot,
    shared: Option<SharedPathsSnapshot>,
    settings: SettingsPathSnapshot,
    settings_load_error: Option<String>,
    descriptions: Vec<ShowConfigsDescription>,
}

#[derive(Debug, serde::Serialize)]
struct HostIdentitySnapshot {
    host_id: String,
    source: HostIdSource,
    sidecar_path: Option<PathBuf>,
}

#[derive(Debug, serde::Serialize)]
struct LauncherPathsSnapshot {
    shared_config_sidecar_path: Option<PathBuf>,
    host_id_sidecar_path: Option<PathBuf>,
}

#[derive(Debug, serde::Serialize)]
struct LocalPathsSnapshot {
    config_dir: Option<PathBuf>,
    settings_path: Option<PathBuf>,
    torrent_metadata_path: Option<PathBuf>,
    config_backups_dir: Option<PathBuf>,
    runtime_data_dir: Option<PathBuf>,
    app_log_dir: Option<PathBuf>,
    cli_log_dir: Option<PathBuf>,
    persistence_dir: Option<PathBuf>,
    event_journal_file: Option<PathBuf>,
    status_file: Option<PathBuf>,
    lock_file: Option<PathBuf>,
    watch_dir: Option<PathBuf>,
    processed_dir: Option<PathBuf>,
}

#[derive(Debug, serde::Serialize)]
struct EffectiveConfigFilesSnapshot {
    settings_path: Option<PathBuf>,
    catalog_path: Option<PathBuf>,
    torrent_metadata_path: Option<PathBuf>,
    host_config_path: Option<PathBuf>,
}

#[derive(Debug, serde::Serialize)]
struct EffectivePathsSnapshot {
    config_files: EffectiveConfigFilesSnapshot,
    runtime_data_dir: Option<PathBuf>,
    app_log_dir: Option<PathBuf>,
    local_app_log_dir: Option<PathBuf>,
    cli_log_dir: Option<PathBuf>,
    persistence_dir: Option<PathBuf>,
    event_journal_file: Option<PathBuf>,
    shared_event_journal_file: Option<PathBuf>,
    status_file: Option<PathBuf>,
    host_status_file: Option<PathBuf>,
    lock_file: Option<PathBuf>,
    command_watch_dir: Option<PathBuf>,
    host_watch_dir: Option<PathBuf>,
    runtime_watch_dirs: Vec<PathBuf>,
}

#[derive(Debug, serde::Serialize)]
struct SharedPathsSnapshot {
    source: SharedConfigSource,
    mount_root: PathBuf,
    config_root: PathBuf,
    settings_path: PathBuf,
    catalog_path: PathBuf,
    torrent_metadata_path: PathBuf,
    torrents_dir: PathBuf,
    cluster_revision_file: PathBuf,
    lock_file: PathBuf,
    inbox_dir: PathBuf,
    processed_dir: PathBuf,
    data_root: PathBuf,
    host_dir: PathBuf,
    host_config_path: PathBuf,
    host_status_file: PathBuf,
    leader_status_file: PathBuf,
    host_log_dir: PathBuf,
    host_persistence_dir: PathBuf,
    shared_event_journal_file: PathBuf,
}

#[derive(Debug, serde::Serialize)]
struct SettingsPathSnapshot {
    default_download_folder: Option<PathBuf>,
    watch_folder: Option<PathBuf>,
    client_port: Option<u16>,
    output_status_interval: Option<u64>,
}

#[derive(Debug, serde::Serialize, Clone, Copy)]
struct ShowConfigsDescription {
    section: &'static str,
    key: &'static str,
    label: &'static str,
    description: &'static str,
}

const SHOW_CONFIG_DESCRIPTIONS: &[ShowConfigsDescription] = &[
    ShowConfigsDescription {
        section: "root",
        key: "shared_mode",
        label: "Shared Mode",
        description: "Whether Superseedr is using the shared cluster config backend.",
    },
    ShowConfigsDescription {
        section: "host",
        key: "host_id",
        label: "Host ID",
        description: "Host identity used for shared host config, status, logs, and runtime state.",
    },
    ShowConfigsDescription {
        section: "host",
        key: "source",
        label: "Host ID source",
        description: "Where the effective host identity came from.",
    },
    ShowConfigsDescription {
        section: "host",
        key: "sidecar_path",
        label: "Host ID sidecar",
        description: "Per-user launcher file that stores a pinned host identity.",
    },
    ShowConfigsDescription {
        section: "launcher",
        key: "shared_config_sidecar_path",
        label: "Shared config sidecar",
        description: "Per-user launcher file that points installed or protocol starts at a shared root.",
    },
    ShowConfigsDescription {
        section: "launcher",
        key: "host_id_sidecar_path",
        label: "Host ID sidecar",
        description: "Per-user launcher file that pins the shared-mode host identity.",
    },
    ShowConfigsDescription {
        section: "effective.config_files",
        key: "settings_path",
        label: "Settings",
        description: "Active settings file for this run mode.",
    },
    ShowConfigsDescription {
        section: "effective.config_files",
        key: "catalog_path",
        label: "Catalog",
        description: "Shared-mode torrent catalog; unavailable in standalone mode.",
    },
    ShowConfigsDescription {
        section: "effective.config_files",
        key: "torrent_metadata_path",
        label: "Torrent metadata",
        description: "Active metadata cache for torrent names and persisted torrent-file references.",
    },
    ShowConfigsDescription {
        section: "effective.config_files",
        key: "host_config_path",
        label: "Host config",
        description: "Shared-mode host-specific config layer for this host.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "runtime_data_dir",
        label: "Runtime data",
        description: "Active runtime state directory for the current mode.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "app_log_dir",
        label: "App logs",
        description: "Directory used by the running app for rolling log files.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "local_app_log_dir",
        label: "Local app logs",
        description: "Always-local app log directory used outside shared host storage or as fallback context.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "cli_log_dir",
        label: "CLI logs",
        description: "Directory used by CLI invocations for command logs.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "persistence_dir",
        label: "Persistence",
        description: "Active runtime persistence directory for host-local history and journals.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "event_journal_file",
        label: "Event journal",
        description: "Host-local event journal file.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "shared_event_journal_file",
        label: "Shared event journal",
        description: "Shared cluster event journal file used in shared mode.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "status_file",
        label: "Status file",
        description: "Status snapshot read by the status command; leader snapshot in shared mode.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "host_status_file",
        label: "Host status file",
        description: "This host's own runtime status snapshot.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "lock_file",
        label: "Lock file",
        description: "Single-instance or shared-leader election lock file.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "command_watch_dir",
        label: "Command watch dir",
        description: "Directory where CLI commands write control or add request files.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "host_watch_dir",
        label: "Host watch dir",
        description: "Directory this host watches for local torrent, magnet, or path inputs.",
    },
    ShowConfigsDescription {
        section: "effective",
        key: "runtime_watch_dirs",
        label: "Runtime watch dirs",
        description: "All directories the current runtime watches for input or shared changes.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "config_dir",
        label: "Config dir",
        description: "Per-user standalone config directory.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "settings_path",
        label: "Settings",
        description: "Standalone settings.toml path.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "torrent_metadata_path",
        label: "Torrent metadata",
        description: "Standalone torrent metadata cache path.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "config_backups_dir",
        label: "Config backups",
        description: "Directory for local settings backup files.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "runtime_data_dir",
        label: "Runtime data",
        description: "Per-user runtime data directory outside shared host storage.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "app_log_dir",
        label: "App logs",
        description: "Local rolling app log directory.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "cli_log_dir",
        label: "CLI logs",
        description: "Local rolling CLI log directory.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "persistence_dir",
        label: "Persistence",
        description: "Local runtime persistence directory.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "event_journal_file",
        label: "Event journal",
        description: "Local event journal file.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "status_file",
        label: "Status file",
        description: "Local status snapshot file.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "lock_file",
        label: "Lock file",
        description: "Local single-instance lock file.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "watch_dir",
        label: "Watch dir",
        description: "Local drop folder for torrent, magnet, or path inputs.",
    },
    ShowConfigsDescription {
        section: "local",
        key: "processed_dir",
        label: "Processed dir",
        description: "Archive folder for processed local watch inputs.",
    },
    ShowConfigsDescription {
        section: "settings",
        key: "default_download_folder",
        label: "Default download folder",
        description: "Configured destination used when a torrent has no per-torrent download path.",
    },
    ShowConfigsDescription {
        section: "settings",
        key: "watch_folder",
        label: "Watch folder",
        description: "User-configured primary watch folder override.",
    },
    ShowConfigsDescription {
        section: "settings",
        key: "client_port",
        label: "Client port",
        description: "Configured BitTorrent listening port.",
    },
    ShowConfigsDescription {
        section: "settings",
        key: "output_status_interval",
        label: "Status interval",
        description: "Configured status snapshot dump interval in seconds.",
    },
    ShowConfigsDescription {
        section: "settings",
        key: "settings_load_error",
        label: "Settings load error",
        description: "Reason settings values are unavailable; path reporting remains best-effort.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "source",
        label: "Source",
        description: "Where the effective shared root selection came from.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "mount_root",
        label: "Mount root",
        description: "Shared data root as mounted on this host.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "config_root",
        label: "Config root",
        description: "Shared superseedr-config directory under the mount root.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "settings_path",
        label: "Settings",
        description: "Shared cluster-wide settings file.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "catalog_path",
        label: "Catalog",
        description: "Shared cluster-wide torrent catalog file.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "torrent_metadata_path",
        label: "Torrent metadata",
        description: "Shared torrent metadata cache path.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "torrents_dir",
        label: "Torrents dir",
        description: "Directory for canonical shared .torrent copies.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "cluster_revision_file",
        label: "Cluster revision",
        description: "Marker file used to signal shared catalog/config revision changes.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "lock_file",
        label: "Lock file",
        description: "Shared leader election lock file.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "inbox_dir",
        label: "Inbox dir",
        description: "Shared folder where CLI and follower nodes enqueue leader-bound requests.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "processed_dir",
        label: "Processed dir",
        description: "Shared archive folder for requests the leader has consumed.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "data_root",
        label: "Data root",
        description: "Shared payload data root for portable shared paths.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "host_dir",
        label: "Host dir",
        description: "Shared-mode host-local directory for this host.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "host_config_path",
        label: "Host config",
        description: "Host-specific shared config file.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "host_status_file",
        label: "Host status",
        description: "This host's shared-mode status snapshot.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "leader_status_file",
        label: "Leader status",
        description: "Shared leader status snapshot followed by shared CLI status.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "host_log_dir",
        label: "Host logs",
        description: "Shared-mode app log directory for this host.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "host_persistence_dir",
        label: "Host persistence",
        description: "Shared-mode host-local persistence directory for this host.",
    },
    ShowConfigsDescription {
        section: "shared",
        key: "shared_event_journal_file",
        label: "Shared event journal",
        description: "Shared cluster event journal file.",
    },
];

// CLI types and process_input moved to integrations::cli

struct TracingInit {
    guards: Vec<logging::LogWorkerGuard>,
    setup_warnings: Vec<String>,
    attempted_file_logging: bool,
}

fn init_tracing(log_dirs: Vec<PathBuf>, filename_prefix: &str, emit_stderr: bool) -> TracingInit {
    let quiet_filter = Targets::new()
        .with_default(DEFAULT_LOG_FILTER)
        .with_target("mainline::rpc::socket", LevelFilter::ERROR);
    let attempted_file_logging = !log_dirs.is_empty();
    let mut suppressed_warnings = Vec::new();
    for log_dir in log_dirs {
        if let Err(error) = fs::create_dir_all(&log_dir) {
            let message = format!(
                "Failed to create log directory at {}: {}",
                log_dir.display(),
                error
            );
            report_logging_setup_warning(message, emit_stderr, &mut suppressed_warnings);
        } else {
            match logging::non_blocking_daily_file_writer_with_stderr_reporting(
                &log_dir,
                filename_prefix,
                31,
                emit_stderr,
            ) {
                Ok((non_blocking_general, guard_general)) => {
                    let general_layer = fmt::layer()
                        .with_writer(non_blocking_general)
                        .with_ansi(false)
                        .with_filter(quiet_filter.clone());
                    if tracing_subscriber::registry()
                        .with(general_layer)
                        .try_init()
                        .is_ok()
                    {
                        return TracingInit {
                            guards: vec![guard_general],
                            setup_warnings: suppressed_warnings,
                            attempted_file_logging,
                        };
                    } else {
                        let message = format!(
                            "Failed to initialize tracing subscriber for file logging at {}",
                            log_dir.display()
                        );
                        report_logging_setup_warning(
                            message,
                            emit_stderr,
                            &mut suppressed_warnings,
                        );
                    }
                }
                Err(error) => {
                    let message = format!(
                        "Failed to initialize file logging at {}: {}",
                        log_dir.display(),
                        error
                    );
                    report_logging_setup_warning(message, emit_stderr, &mut suppressed_warnings);
                }
            }
        }
    }

    let fallback_mode = fallback_tracing_mode(emit_stderr);
    if fallback_mode == FallbackTracingMode::Stderr {
        eprintln!(
            "[Warn] {}",
            final_logging_fallback_message(attempted_file_logging)
        );
        for warning in &suppressed_warnings {
            eprintln!("[Warn] {}", warning);
        }
    }

    let fallback_layer = match fallback_mode {
        FallbackTracingMode::Stderr => fmt::layer()
            .with_writer(io::stderr)
            .with_filter(quiet_filter)
            .boxed(),
        FallbackTracingMode::Sink => {
            let sink_filter = Targets::new()
                .with_default(LevelFilter::WARN)
                .with_target("mainline::rpc::socket", LevelFilter::ERROR);
            fmt::layer()
                .with_writer(io::sink)
                .with_filter(sink_filter)
                .boxed()
        }
    };
    let _ = tracing_subscriber::registry()
        .with(fallback_layer)
        .try_init();

    TracingInit {
        guards: Vec::new(),
        setup_warnings: suppressed_warnings,
        attempted_file_logging,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackTracingMode {
    Stderr,
    Sink,
}

fn fallback_tracing_mode(emit_stderr: bool) -> FallbackTracingMode {
    if emit_stderr {
        FallbackTracingMode::Stderr
    } else {
        FallbackTracingMode::Sink
    }
}

fn report_logging_setup_warning(
    message: String,
    emit_stderr: bool,
    suppressed_warnings: &mut Vec<String>,
) {
    if emit_stderr {
        eprintln!("[Warn] {}", message);
    }
    suppressed_warnings.push(message);
}

fn final_logging_fallback_message(attempted_file_logging: bool) -> &'static str {
    if attempted_file_logging {
        "File logging is unavailable; using stderr fallback for diagnostics."
    } else {
        "No file logging directories were available; using stderr fallback for diagnostics."
    }
}

fn tui_logging_setup_warning_message(init: &TracingInit) -> Option<String> {
    if init.setup_warnings.is_empty() {
        return None;
    }

    let summary = if init.attempted_file_logging {
        "File logging is unavailable; runtime diagnostics may be limited."
    } else {
        "No file logging directories were available; runtime diagnostics may be limited."
    };
    Some(format!("{} {}", summary, init.setup_warnings.join(" ")))
}

fn already_running_message() -> &'static str {
    "superseedr is already running."
}

#[cfg(all(feature = "dht", feature = "pex"))]
fn private_client_leak_guard_message(config_path: &str) -> String {
    format!(
        "\n!!!ERROR: POTENTIAL LEAK!!!\n---------------------------------\nYou are running the normal build of superseedr (with DHT/PEX enabled),\nbut your configuration file indicates you last used a private build.\n\nThis safety check prevents accidental use of forbidden features on private trackers.\n\nChoose an option:\n  1. If you want to use the PRIVATE build (for private trackers):\n     Install and run it:\n       cargo install superseedr --no-default-features\n       superseedr\n\n  2. If you want to switch back to the NORMAL build (for public trackers):\n     Manually edit your configuration file:\n       {config_path}\n     Change the line `private_client = true` to `private_client = false`\n     Then, run this normal build again.\n\nExiting to prevent potential tracker issues."
    )
}

fn apply_startup_network_interface(
    settings: &mut Settings,
    requested_interface: &str,
) -> io::Result<()> {
    let interfaces = networking::available_network_interfaces().unwrap_or_default();
    apply_startup_network_interface_from(settings, requested_interface, &interfaces)
}

fn apply_startup_network_interface_from(
    settings: &mut Settings,
    requested_interface: &str,
    interfaces: &[NetworkInterfaceInfo],
) -> io::Result<()> {
    let interface = requested_interface.trim();
    if interface.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--network-interface requires a non-empty interface identity",
        ));
    }

    settings.network_binding.mode = NetworkBindingMode::Interface;
    settings.network_binding.interface = Some(interface.to_string());
    settings.network_binding.ipv4_address = None;
    settings.network_binding.ipv6_address = None;

    if let Some(discovered) = interfaces
        .iter()
        .find(|candidate| candidate.identity == interface)
    {
        match (
            discovered.ipv4_addresses.is_empty(),
            discovered.ipv6_addresses.is_empty(),
        ) {
            (false, true) => {
                settings.network_binding.enable_ipv4 = true;
                settings.network_binding.enable_ipv6 = false;
                settings.network_binding.ipv6_address = None;
            }
            (true, false) => {
                settings.network_binding.enable_ipv4 = false;
                settings.network_binding.enable_ipv6 = true;
                settings.network_binding.ipv4_address = None;
            }
            (false, false)
                if !settings.network_binding.enable_ipv4
                    && !settings.network_binding.enable_ipv6 =>
            {
                settings.network_binding.enable_ipv4 = true;
                settings.network_binding.enable_ipv6 = true;
            }
            _ => {}
        }
    }

    Ok(())
}

fn apply_persisted_network_interface(
    settings: &mut Settings,
    requested_interface: &str,
    interfaces: &[NetworkInterfaceInfo],
) -> io::Result<()> {
    let interface = requested_interface.trim();
    if interface.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "set-network-interface requires a non-empty interface identity",
        ));
    }

    let discovered = interfaces
        .iter()
        .find(|candidate| candidate.identity == interface)
        .filter(|candidate| candidate.is_selectable())
        .ok_or_else(|| {
            let available = interfaces
                .iter()
                .filter(|candidate| candidate.is_selectable())
                .map(|candidate| candidate.identity.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if available.is_empty() {
                "no active non-loopback interfaces were discovered".to_string()
            } else {
                format!("available interfaces: {available}")
            };
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("network interface '{interface}' is not currently usable; {suffix}"),
            )
        })?;

    let previous_binding = settings.network_binding.clone();
    apply_startup_network_interface_from(
        settings,
        &discovered.identity,
        std::slice::from_ref(discovered),
    )?;
    if settings.network_binding.dns_policy == DnsPolicy::Bound {
        if let Err(error) = validate_bound_dns_servers(
            &settings.network_binding.dns_servers,
            settings.network_binding.enable_ipv4,
            settings.network_binding.enable_ipv6,
        ) {
            settings.network_binding = previous_binding;
            return Err(io::Error::new(
                error.kind(),
                format!("network interface '{interface}' is incompatible with bound DNS: {error}"),
            ));
        }
    }
    Ok(())
}

fn process_set_network_interface_command(
    settings: &Settings,
    requested_interface: &str,
    instance_already_running: bool,
    shared_mode: bool,
    output_mode: OutputMode,
) -> io::Result<()> {
    if instance_already_running {
        let message = if shared_mode {
            "Stop all superseedr clients using this shared configuration before changing a persisted network interface."
        } else {
            "Stop the running superseedr client before changing its persisted network interface, or change it from the TUI."
        };
        return Err(io::Error::new(io::ErrorKind::WouldBlock, message));
    }

    let interfaces = networking::available_network_interfaces()?;
    let mut updated = settings.clone();
    apply_persisted_network_interface(&mut updated, requested_interface, &interfaces)?;
    config::save_network_binding(&updated.network_binding)?;

    let interface = updated
        .network_binding
        .interface
        .as_deref()
        .expect("persisted interface binding has an interface");
    let families = match (
        updated.network_binding.enable_ipv4,
        updated.network_binding.enable_ipv6,
    ) {
        (true, true) => "IPv4 and IPv6",
        (true, false) => "IPv4",
        (false, true) => "IPv6",
        (false, false) => unreachable!("usable interface has at least one address family"),
    };
    print_success(
        output_mode,
        "set-network-interface",
        &format!("Persisted strict {families} binding to '{interface}' for this host."),
        json!({
            "interface": interface,
            "enable_ipv4": updated.network_binding.enable_ipv4,
            "enable_ipv6": updated.network_binding.enable_ipv6,
            "scope": "host",
        }),
    );
    Ok(())
}

pub(crate) async fn run() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "webtorrent")]
    crate::networking::webtorrent::native::initialize_crypto();
    let cli = Cli::parse();
    let output_mode = if cli.json {
        OutputMode::Json
    } else {
        OutputMode::Text
    };
    let has_cli_request = cli.input.is_some() || cli.command.is_some();
    if has_cli_request && cli.network_interface.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--network-interface is a client-startup option and cannot be used with a command or torrent input",
        )
        .into());
    }
    let log_dirs = if has_cli_request {
        let mut dirs = Vec::new();
        if let Some(dir) = config::local_cli_log_dir() {
            dirs.push(dir);
        }
        if let Some(dir) = config::local_runtime_data_dir() {
            dirs.push(dir);
        }
        if let Ok(dir) = env::current_dir() {
            dirs.push(dir);
        }
        dirs
    } else {
        let mut dirs = Vec::new();
        if let Some(dir) = config::runtime_log_dir() {
            dirs.push(dir);
        }
        if let Some(dir) = config::local_runtime_log_dir() {
            if !dirs.iter().any(|existing| existing == &dir) {
                dirs.push(dir);
            }
        }
        if let Ok(dir) = env::current_dir() {
            if !dirs.iter().any(|existing| existing == &dir) {
                dirs.push(dir);
            }
        }
        dirs
    };
    let tracing_init = init_tracing(
        log_dirs,
        if has_cli_request { "cli" } else { "app" },
        has_cli_request,
    );
    let tui_logging_warning = if has_cli_request {
        None
    } else {
        tui_logging_setup_warning_message(&tracing_init)
    };
    let _tracing_guards = tracing_init.guards;

    tracing::info!(target: "superseedr", "STARTING SUPERSEEDR");

    if let Some(Commands::ShowConfigs { all }) = cli.command.as_ref() {
        let (settings, settings_load_error) = match load_settings_for_cli() {
            Ok(settings) => (Some(settings), None),
            Err(error) => (None, Some(error.to_string())),
        };
        if let Err(error) =
            process_show_configs_command(settings.as_ref(), settings_load_error, *all, output_mode)
        {
            if output_mode == OutputMode::Json {
                print_json_error(cli_command_name(cli.command.as_ref()), &error.to_string());
            } else {
                eprintln!("[Error] Application failed: {}", error);
            }
            std::process::exit(1);
        }
        tracing::info!(target: "superseedr", "Show configs command processed, exiting temporary instance.");
        return Ok(());
    }

    #[cfg(feature = "synthetic-load")]
    if let Some(Commands::Benchmark(args)) = cli.command.as_ref() {
        if let Err(error) = synthetic_load::run_benchmark(args, cli.json).await {
            if output_mode == OutputMode::Json {
                print_json_error(cli_command_name(cli.command.as_ref()), &error.to_string());
            } else {
                eprintln!("[Error] Benchmark failed: {}", error);
            }
            std::process::exit(1);
        }
        tracing::info!(target: "superseedr", "Benchmark command processed, exiting temporary instance.");
        return Ok(());
    }

    #[cfg(feature = "synthetic-load")]
    if let Some(Commands::SyntheticLoad(args)) = cli.command.as_ref() {
        if let Err(error) = synthetic_load::run(args, cli.json).await {
            if output_mode == OutputMode::Json {
                print_json_error(cli_command_name(cli.command.as_ref()), &error.to_string());
            } else {
                eprintln!("[Error] Synthetic load failed: {}", error);
            }
            std::process::exit(1);
        }
        tracing::info!(target: "superseedr", "Synthetic load command processed, exiting temporary instance.");
        return Ok(());
    }

    if let Some(result) = process_launcher_setup_command(&cli, output_mode) {
        if let Err(error) = result {
            if output_mode == OutputMode::Json {
                print_json_error(cli_command_name(cli.command.as_ref()), &error.to_string());
            } else {
                eprintln!("[Error] Application failed: {}", error);
            }
            std::process::exit(1);
        }
        tracing::info!(target: "superseedr", "Launcher setup command processed, exiting temporary instance.");
        return Ok(());
    }

    let app_persistence = AppPersistence::native();
    let loaded_settings = match if has_cli_request {
        app_persistence.load_settings_for_cli()
    } else {
        app_persistence.load_settings()
    } {
        Ok(settings) => settings,
        Err(error) => {
            if has_cli_request && output_mode == OutputMode::Json {
                print_json_error(cli_command_name(cli.command.as_ref()), &error.to_string());
                std::process::exit(1);
            }
            return Err(Box::new(error) as Box<dyn std::error::Error>);
        }
    };

    if !has_cli_request {
        if let Err(e) = config::ensure_watch_directories(&loaded_settings) {
            tracing::error!(target: "superseedr", "Failed to create watch directories: {}", e);
        }
    }

    let shared_mode = is_shared_config_mode();
    let lock_file_handle = try_acquire_app_lock()?;
    let instance_already_running = lock_file_handle.is_none();

    if has_cli_request {
        if let Err(error) = process_cli_request(
            &cli,
            &loaded_settings,
            shared_mode,
            instance_already_running,
            output_mode,
        ) {
            if output_mode == OutputMode::Json {
                print_json_error(cli_command_name(cli.command.as_ref()), &error.to_string());
            } else {
                eprintln!("[Error] Application failed: {}", error);
            }
            std::process::exit(1);
        }
        tracing::info!(target: "superseedr", "Command processed, exiting temporary instance.");
        return Ok(());
    }

    let runtime_mode = if shared_mode {
        if lock_file_handle.is_some() {
            AppRuntimeMode::SharedLeader
        } else {
            AppRuntimeMode::SharedFollower
        }
    } else if lock_file_handle.is_some() {
        AppRuntimeMode::Normal
    } else {
        let message = already_running_message();
        println!("{message}");
        tracing::info!(target: "superseedr", "{message}");
        return Ok(());
    };

    let mut client_configs = loaded_settings;
    let can_persist_startup_settings = !runtime_mode.is_shared_follower();

    #[cfg(all(feature = "dht", feature = "pex"))]
    {
        if client_configs.private_client {
            let config_path_str = config::shared_settings_path()
                .or_else(config::local_settings_path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unable to determine config path.".to_string());
            let message = private_client_leak_guard_message(&config_path_str);

            println!("{message}");
            tracing::error!(target: "superseedr",
                config_path = %config_path_str,
                "Potential leak guard triggered. You are running the normal build with DHT/PEX enabled, but your configuration indicates the private build was used previously. To continue safely, either install and run the private build with `cargo install superseedr --no-default-features`, or edit the configuration at {} and change `private_client = true` to `private_client = false`. Exiting to prevent potential tracker issues.",
                config_path_str
            );
            std::process::exit(1);
        }
    }

    #[cfg(not(all(feature = "dht", feature = "pex")))]
    {
        if !client_configs.private_client {
            tracing::info!(target: "superseedr", "Setting private mode flag in configuration.");
            client_configs.private_client = true;
            if can_persist_startup_settings {
                if let Err(e) = app_persistence.save_settings(&client_configs) {
                    tracing::error!(target: "superseedr",
                        "Failed to save settings after setting private mode flag: {}",
                        e
                    );
                }
            }
        }
    }

    let port_file_path = PathBuf::from("/port-data/forwarded_port");
    tracing::info!(target: "superseedr", "Checking for dynamic port file at {:?}", port_file_path);
    if let Ok(port_str) = fs::read_to_string(&port_file_path) {
        match port_str.trim().parse::<u16>() {
            Ok(dynamic_port) => {
                if dynamic_port > 0 {
                    tracing::info!(target: "superseedr",
                        "Successfully read dynamic port {}. Overriding settings.",
                        dynamic_port
                    );
                    client_configs.client_port = dynamic_port;
                    client_configs.randomize_client_port = false;
                } else {
                    tracing::warn!(target: "superseedr", "Dynamic port file was empty or zero. Using config port.");
                }
            }
            Err(e) => {
                tracing::error!(target: "superseedr",
                    "Failed to parse port file content '{}': {}. Using config port.",
                    port_str,
                    e
                );
            }
        }
    } else {
        tracing::info!(target: "superseedr",
            "Dynamic file not found. Using port {} from settings.",
            client_configs.client_port
        );
    }

    let updated_client_id = if client_configs.client_id.is_empty() {
        Some(generate_client_id_string())
    } else {
        peer_manager::refresh_superseedr_peer_id_version(&client_configs.client_id)
    };
    if let Some(updated_client_id) = updated_client_id {
        client_configs.client_id = updated_client_id;
        if can_persist_startup_settings {
            if let Err(e) = app_persistence.save_settings(&client_configs) {
                tracing::error!(target: "superseedr", "Failed to save settings after updating client ID: {}", e);
            }
        } else {
            tracing::info!(target: "superseedr", "Updated in-memory client ID for shared follower startup.");
        }
    }

    let persisted_network_binding_override = cli
        .network_interface
        .as_ref()
        .map(|_| client_configs.network_binding.clone());
    if let Some(interface) = cli.network_interface.as_deref() {
        apply_startup_network_interface(&mut client_configs, interface)?;
        tracing::info!(target: "superseedr",
            interface,
            "Applying startup-only network interface override"
        );
    }

    tracing::info!(target: "superseedr", "Initializing application state...");
    let mut app = if persisted_network_binding_override.is_some() {
        App::new_with_lock_network_override_and_persistence(
            client_configs,
            runtime_mode,
            lock_file_handle,
            persisted_network_binding_override,
            app_persistence,
        )
        .await?
    } else {
        App::new_with_lock_and_persistence(
            client_configs,
            runtime_mode,
            lock_file_handle,
            app_persistence,
        )
        .await?
    };
    app.app_state.system_error = tui_logging_warning;
    tracing::info!(target: "superseedr", "Application state initialized. Starting TUI.");

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = cleanup_terminal();
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen,)?;
    let _ = execute!(stdout, EnableBracketedPaste);

    #[cfg(not(windows))]
    {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        );
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app_result = app.run(&mut terminal).await;
    cleanup_terminal()?;

    if let Err(error) = app_result {
        tracing::error!(target: "superseedr", "Application failed: {}", error);
        return Err(error);
    }

    Ok(())
}

fn get_lock_path() -> Option<PathBuf> {
    if is_shared_config_mode() {
        return shared_lock_path();
    }

    config::local_lock_path().or_else(|| {
        Some(
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("superseedr.lock"),
        )
    })
}

fn try_acquire_app_lock() -> io::Result<Option<File>> {
    let Some(lock_path) = get_lock_path() else {
        return Ok(None);
    };
    let file = File::create(lock_path)?;
    if file.try_lock().is_ok() {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

fn process_launcher_setup_command(cli: &Cli, output_mode: OutputMode) -> Option<io::Result<()>> {
    let command = cli.command.as_ref()?;
    match command {
        Commands::SetSharedConfig { path } => {
            Some(process_set_shared_config_command(path, output_mode))
        }
        Commands::ClearSharedConfig => Some(process_clear_shared_config_command(output_mode)),
        Commands::ShowSharedConfig => Some(process_show_shared_config_command(output_mode)),
        Commands::SetHostId { host_id } => Some(process_set_host_id_command(host_id, output_mode)),
        Commands::ClearHostId => Some(process_clear_host_id_command(output_mode)),
        Commands::ShowHostId => Some(process_show_host_id_command(output_mode)),
        Commands::ToShared { path } => Some(process_to_shared_command(path, output_mode)),
        Commands::ToStandalone => Some(process_to_standalone_command(output_mode)),
        _ => None,
    }
}

fn shared_config_selection_json(selection: &crate::config::SharedConfigSelection) -> Value {
    json!({
        "source": selection.source,
        "mount_root": selection.mount_root,
        "config_root": selection.config_root,
    })
}

fn optional_path_json(path: Option<PathBuf>) -> Value {
    match path {
        Some(path) => json!(path),
        None => Value::Null,
    }
}

fn print_optional_sidecar_path(sidecar_path: Option<&PathBuf>) {
    match sidecar_path {
        Some(sidecar_path) => println!("Sidecar Path: {}", sidecar_path.display()),
        None => println!("Sidecar Path: <unavailable>"),
    }
}

fn process_set_shared_config_command(
    path: &std::path::Path,
    output_mode: OutputMode,
) -> io::Result<()> {
    let selection = set_persisted_shared_config(path)?;
    let sidecar_path = persisted_shared_config_path()?;
    print_success(
        output_mode,
        "set-shared-config",
        &format!(
            "Persisted shared config root at {}.",
            selection.mount_root.display()
        ),
        json!({
            "enabled": true,
            "selection": shared_config_selection_json(&selection),
            "sidecar_path": sidecar_path,
        }),
    );
    Ok(())
}

fn process_clear_shared_config_command(output_mode: OutputMode) -> io::Result<()> {
    let cleared = clear_persisted_shared_config()?;
    let sidecar_path = persisted_shared_config_path()?;
    let message = if cleared {
        "Cleared persisted shared config."
    } else {
        "No persisted shared config was set."
    };
    print_success(
        output_mode,
        "clear-shared-config",
        message,
        json!({
            "enabled": false,
            "cleared": cleared,
            "sidecar_path": sidecar_path,
        }),
    );
    Ok(())
}

fn process_show_shared_config_command(output_mode: OutputMode) -> io::Result<()> {
    let selection = effective_shared_config_selection()?;
    let sidecar_path = persisted_shared_config_path().ok();

    match (output_mode, selection) {
        (OutputMode::Json, Some(selection)) => {
            print_success(
                output_mode,
                "show-shared-config",
                "Shared config is enabled.",
                json!({
                    "enabled": true,
                    "selection": shared_config_selection_json(&selection),
                    "sidecar_path": optional_path_json(sidecar_path.clone()),
                }),
            );
        }
        (OutputMode::Json, None) => {
            print_success(
                output_mode,
                "show-shared-config",
                "Shared config is disabled.",
                json!({
                    "enabled": false,
                    "selection": Value::Null,
                    "sidecar_path": optional_path_json(sidecar_path.clone()),
                }),
            );
        }
        (OutputMode::Text, Some(selection)) => {
            println!("Shared config is enabled.");
            println!(
                "Source: {}",
                match selection.source {
                    SharedConfigSource::Env => "env",
                    SharedConfigSource::CurrentDirectory => "current_directory",
                    SharedConfigSource::Launcher => "launcher",
                }
            );
            println!("Mount Root: {}", selection.mount_root.display());
            println!("Config Root: {}", selection.config_root.display());
            print_optional_sidecar_path(sidecar_path.as_ref());
        }
        (OutputMode::Text, None) => {
            println!("Shared config is disabled.");
            print_optional_sidecar_path(sidecar_path.as_ref());
        }
    }

    Ok(())
}

fn process_show_configs_command(
    settings: Option<&Settings>,
    settings_load_error: Option<String>,
    show_all: bool,
    output_mode: OutputMode,
) -> io::Result<()> {
    let snapshot = build_show_configs_snapshot(settings, settings_load_error)?;
    match output_mode {
        OutputMode::Text => {
            print_show_configs_text(&snapshot, show_all);
            Ok(())
        }
        OutputMode::Json => {
            print_success(
                output_mode,
                "show-configs",
                "Resolved Superseedr configuration paths.",
                show_configs_json_data(&snapshot, show_all),
            );
            Ok(())
        }
    }
}

fn build_show_configs_snapshot(
    settings: Option<&Settings>,
    settings_load_error: Option<String>,
) -> io::Result<ShowConfigsSnapshot> {
    let shared_selection = effective_shared_config_selection()?;
    let host_selection = effective_host_id_selection()?;
    let shared_mode = shared_selection.is_some();

    let local_config_dir = config::app_config_dir();
    let local_runtime_data_dir = config::local_runtime_data_dir();
    let local_settings_path = config::local_settings_path();
    let local_torrent_metadata_path = local_config_dir
        .as_ref()
        .map(|dir| dir.join("torrent_metadata.toml"));
    let local_config_backups_dir = local_config_dir
        .as_ref()
        .map(|dir| dir.join("backups_settings_files"));
    let local_persistence_dir = local_runtime_data_dir
        .as_ref()
        .map(|dir| dir.join("persistence"));
    let local_event_journal_file = local_persistence_dir
        .as_ref()
        .map(|dir| dir.join("event_journal.toml"));
    let local_status_file = local_runtime_data_dir
        .as_ref()
        .map(|dir| dir.join("status_files").join("app_state.json"));
    let (local_watch_dir, local_processed_dir) = config::get_watch_path()
        .map(|(watch_dir, processed_dir)| (Some(watch_dir), Some(processed_dir)))
        .unwrap_or((None, None));

    let launcher = LauncherPathsSnapshot {
        shared_config_sidecar_path: absolute_path_opt(persisted_shared_config_path().ok())?,
        host_id_sidecar_path: absolute_path_opt(persisted_host_id_path().ok())?,
    };

    let host = HostIdentitySnapshot {
        host_id: host_selection.host_id.clone(),
        source: host_selection.source,
        sidecar_path: launcher.host_id_sidecar_path.clone(),
    };

    let shared_root = shared_selection
        .as_ref()
        .map(|selection| selection.config_root.clone());
    let shared_host_dir = shared_root
        .as_ref()
        .map(|root| root.join("hosts").join(&host_selection.host_id));
    let shared_host_config_path = shared_host_dir.as_ref().map(|dir| dir.join("config.toml"));
    let shared_metadata_path = shared_root
        .as_ref()
        .map(|root| root.join("torrent_metadata.toml"));
    let shared_catalog_path = shared_root.as_ref().map(|root| root.join("catalog.toml"));
    let shared_settings_path = shared_root.as_ref().map(|root| root.join("settings.toml"));
    let shared_event_journal_file = if shared_mode {
        Some(crate::persistence::event_journal::shared_event_journal_state_file_path()?)
    } else {
        None
    };

    let shared = match shared_selection {
        Some(selection) => {
            let root = selection.config_root;
            let mount = selection.mount_root;
            let host_dir = root.join("hosts").join(&host_selection.host_id);
            Some(SharedPathsSnapshot {
                source: selection.source,
                mount_root: absolute_path(mount.clone())?,
                config_root: absolute_path(root.clone())?,
                settings_path: absolute_path(root.join("settings.toml"))?,
                catalog_path: absolute_path(root.join("catalog.toml"))?,
                torrent_metadata_path: absolute_path(root.join("torrent_metadata.toml"))?,
                torrents_dir: absolute_path(root.join("torrents"))?,
                cluster_revision_file: absolute_path(root.join("cluster.revision"))?,
                lock_file: absolute_path(root.join("superseedr.lock"))?,
                inbox_dir: absolute_path(root.join("inbox"))?,
                processed_dir: absolute_path(root.join("processed"))?,
                data_root: absolute_path(mount)?,
                host_dir: absolute_path(host_dir.clone())?,
                host_config_path: absolute_path(host_dir.join("config.toml"))?,
                host_status_file: absolute_path(host_dir.join("status.json"))?,
                leader_status_file: absolute_path(root.join("status").join("leader.json"))?,
                host_log_dir: absolute_path(host_dir.join("logs"))?,
                host_persistence_dir: absolute_path(host_dir.join("persistence"))?,
                shared_event_journal_file: absolute_path(
                    root.join("journal").join("shared_event_journal.toml"),
                )?,
            })
        }
        None => None,
    };

    let effective_config_files = EffectiveConfigFilesSnapshot {
        settings_path: absolute_path_opt(if shared_mode {
            shared_settings_path
        } else {
            local_settings_path.clone()
        })?,
        catalog_path: absolute_path_opt(shared_catalog_path)?,
        torrent_metadata_path: absolute_path_opt(if shared_mode {
            shared_metadata_path
        } else {
            local_torrent_metadata_path.clone()
        })?,
        host_config_path: absolute_path_opt(shared_host_config_path)?,
    };

    let effective_runtime_data_dir = if shared_mode {
        shared_host_dir.clone()
    } else {
        local_runtime_data_dir.clone()
    };
    let effective_app_log_dir = effective_runtime_data_dir
        .as_ref()
        .map(|dir| dir.join("logs"));
    let effective_persistence_dir = effective_runtime_data_dir
        .as_ref()
        .map(|dir| dir.join("persistence"));
    let effective_event_journal_file = effective_persistence_dir
        .as_ref()
        .map(|dir| dir.join("event_journal.toml"));
    let effective_status_file = if shared_mode {
        shared_root
            .as_ref()
            .map(|root| root.join("status").join("leader.json"))
    } else {
        local_status_file.clone()
    };
    let effective_host_status_file = if shared_mode {
        shared_host_dir.as_ref().map(|dir| dir.join("status.json"))
    } else {
        local_status_file.clone()
    };
    let effective_lock_file = if shared_mode {
        shared_root
            .as_ref()
            .map(|root| root.join("superseedr.lock"))
    } else {
        config::local_lock_path().or_else(|| {
            env::current_dir()
                .ok()
                .map(|dir| dir.join("superseedr.lock"))
        })
    };
    let host_watch_dir = settings
        .and_then(config::resolve_host_watch_path)
        .or_else(|| local_watch_dir.clone());
    let command_watch_dir = if shared_mode {
        shared_root.as_ref().map(|root| root.join("inbox"))
    } else {
        host_watch_dir.clone()
    };
    let runtime_watch_dirs = if let Some(settings) = settings {
        config::configured_watch_paths(settings)
    } else {
        let mut paths = Vec::new();
        push_unique_report_path(&mut paths, host_watch_dir.clone());
        if shared_mode {
            push_unique_report_path(&mut paths, shared_root.clone());
            push_unique_report_path(
                &mut paths,
                shared_root.as_ref().map(|root| root.join("inbox")),
            );
        } else {
            push_unique_report_path(&mut paths, command_watch_dir.clone());
        }
        paths
    };
    let settings_default_download_folder =
        settings.and_then(|settings| settings.default_download_folder.clone());
    let settings_watch_folder = settings.and_then(|settings| settings.watch_folder.clone());

    Ok(ShowConfigsSnapshot {
        shared_mode,
        host,
        launcher,
        local: LocalPathsSnapshot {
            config_dir: absolute_path_opt(local_config_dir)?,
            settings_path: absolute_path_opt(local_settings_path)?,
            torrent_metadata_path: absolute_path_opt(local_torrent_metadata_path)?,
            config_backups_dir: absolute_path_opt(local_config_backups_dir)?,
            runtime_data_dir: absolute_path_opt(local_runtime_data_dir.clone())?,
            app_log_dir: absolute_path_opt(config::local_runtime_log_dir())?,
            cli_log_dir: absolute_path_opt(config::local_cli_log_dir())?,
            persistence_dir: absolute_path_opt(local_persistence_dir)?,
            event_journal_file: absolute_path_opt(local_event_journal_file)?,
            status_file: absolute_path_opt(local_status_file)?,
            lock_file: absolute_path_opt(config::local_lock_path())?,
            watch_dir: absolute_path_opt(local_watch_dir)?,
            processed_dir: absolute_path_opt(local_processed_dir)?,
        },
        effective: EffectivePathsSnapshot {
            config_files: effective_config_files,
            runtime_data_dir: absolute_path_opt(effective_runtime_data_dir)?,
            app_log_dir: absolute_path_opt(effective_app_log_dir)?,
            local_app_log_dir: absolute_path_opt(
                local_runtime_data_dir.as_ref().map(|dir| dir.join("logs")),
            )?,
            cli_log_dir: absolute_path_opt(
                local_runtime_data_dir
                    .as_ref()
                    .map(|dir| dir.join("logs").join("cli")),
            )?,
            persistence_dir: absolute_path_opt(effective_persistence_dir)?,
            event_journal_file: absolute_path_opt(effective_event_journal_file)?,
            shared_event_journal_file: absolute_path_opt(shared_event_journal_file)?,
            status_file: absolute_path_opt(effective_status_file)?,
            host_status_file: absolute_path_opt(effective_host_status_file)?,
            lock_file: absolute_path_opt(effective_lock_file)?,
            command_watch_dir: absolute_path_opt(command_watch_dir)?,
            host_watch_dir: absolute_path_opt(host_watch_dir)?,
            runtime_watch_dirs: absolute_paths(runtime_watch_dirs)?,
        },
        shared,
        settings: SettingsPathSnapshot {
            default_download_folder: absolute_path_opt(settings_default_download_folder)?,
            watch_folder: absolute_path_opt(settings_watch_folder)?,
            client_port: settings.map(|settings| settings.client_port),
            output_status_interval: settings.map(|settings| settings.output_status_interval),
        },
        settings_load_error,
        descriptions: show_configs_descriptions(),
    })
}

fn absolute_path(path: PathBuf) -> io::Result<PathBuf> {
    std::path::absolute(path)
}

fn absolute_path_opt(path: Option<PathBuf>) -> io::Result<Option<PathBuf>> {
    path.map(absolute_path).transpose()
}

fn absolute_paths(paths: Vec<PathBuf>) -> io::Result<Vec<PathBuf>> {
    paths.into_iter().map(absolute_path).collect()
}

fn push_unique_report_path(paths: &mut Vec<PathBuf>, path: Option<PathBuf>) {
    if let Some(path) = path {
        if !paths.iter().any(|existing| existing == &path) {
            paths.push(path);
        }
    }
}

fn show_configs_descriptions() -> Vec<ShowConfigsDescription> {
    SHOW_CONFIG_DESCRIPTIONS.to_vec()
}

fn show_config_description(section: &str, key: &str) -> &'static str {
    SHOW_CONFIG_DESCRIPTIONS
        .iter()
        .find(|entry| entry.section == section && entry.key == key)
        .map(|entry| entry.description)
        .unwrap_or("")
}

fn show_configs_json_data(snapshot: &ShowConfigsSnapshot, show_all: bool) -> Value {
    if show_all {
        return json!(snapshot);
    }

    json!({
        "shared_mode": snapshot.shared_mode,
        "host": &snapshot.host,
        "effective": &snapshot.effective,
        "settings": &snapshot.settings,
        "settings_load_error": &snapshot.settings_load_error,
        "descriptions": show_configs_effective_descriptions(),
    })
}

fn show_configs_effective_descriptions() -> Vec<ShowConfigsDescription> {
    SHOW_CONFIG_DESCRIPTIONS
        .iter()
        .copied()
        .filter(|entry| {
            matches!(
                entry.section,
                "root" | "host" | "effective" | "effective.config_files" | "settings"
            )
        })
        .collect()
}

fn print_show_configs_text(snapshot: &ShowConfigsSnapshot, show_all: bool) {
    if show_all {
        println!("Superseedr resolved configuration");
    } else {
        println!("Superseedr effective configuration");
    }
    let shared_mode_label = if snapshot.shared_mode {
        "enabled"
    } else {
        "disabled"
    };
    println!(
        "Shared Mode: {} - {}",
        shared_mode_label,
        show_config_description("root", "shared_mode")
    );
    println!(
        "Host ID: {} ({}) - {}",
        snapshot.host.host_id,
        host_id_source_label(snapshot.host.source),
        show_config_description("host", "host_id")
    );

    if !show_all {
        print_show_configs_effective(snapshot);
        print_show_configs_settings(snapshot);
        return;
    }

    println!("\nLauncher:");
    print_path_line(
        "launcher",
        "shared_config_sidecar_path",
        "Shared config sidecar",
        snapshot.launcher.shared_config_sidecar_path.as_ref(),
    );
    print_path_line(
        "launcher",
        "host_id_sidecar_path",
        "Host ID sidecar",
        snapshot.launcher.host_id_sidecar_path.as_ref(),
    );

    println!("\nEffective:");
    print_path_line(
        "effective.config_files",
        "settings_path",
        "Settings",
        snapshot.effective.config_files.settings_path.as_ref(),
    );
    print_path_line(
        "effective.config_files",
        "catalog_path",
        "Catalog",
        snapshot.effective.config_files.catalog_path.as_ref(),
    );
    print_path_line(
        "effective.config_files",
        "torrent_metadata_path",
        "Torrent metadata",
        snapshot
            .effective
            .config_files
            .torrent_metadata_path
            .as_ref(),
    );
    print_path_line(
        "effective.config_files",
        "host_config_path",
        "Host config",
        snapshot.effective.config_files.host_config_path.as_ref(),
    );
    print_path_line(
        "effective",
        "runtime_data_dir",
        "Runtime data",
        snapshot.effective.runtime_data_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "app_log_dir",
        "App logs",
        snapshot.effective.app_log_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "local_app_log_dir",
        "Local app logs",
        snapshot.effective.local_app_log_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "cli_log_dir",
        "CLI logs",
        snapshot.effective.cli_log_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "persistence_dir",
        "Persistence",
        snapshot.effective.persistence_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "event_journal_file",
        "Event journal",
        snapshot.effective.event_journal_file.as_ref(),
    );
    print_path_line(
        "effective",
        "shared_event_journal_file",
        "Shared event journal",
        snapshot.effective.shared_event_journal_file.as_ref(),
    );
    print_path_line(
        "effective",
        "status_file",
        "Status file",
        snapshot.effective.status_file.as_ref(),
    );
    print_path_line(
        "effective",
        "host_status_file",
        "Host status file",
        snapshot.effective.host_status_file.as_ref(),
    );
    print_path_line(
        "effective",
        "lock_file",
        "Lock file",
        snapshot.effective.lock_file.as_ref(),
    );
    print_path_line(
        "effective",
        "command_watch_dir",
        "Command watch dir",
        snapshot.effective.command_watch_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "host_watch_dir",
        "Host watch dir",
        snapshot.effective.host_watch_dir.as_ref(),
    );
    print_path_list(
        "effective",
        "runtime_watch_dirs",
        "Runtime watch dirs",
        &snapshot.effective.runtime_watch_dirs,
    );

    println!("\nLocal:");
    print_path_line(
        "local",
        "config_dir",
        "Config dir",
        snapshot.local.config_dir.as_ref(),
    );
    print_path_line(
        "local",
        "settings_path",
        "Settings",
        snapshot.local.settings_path.as_ref(),
    );
    print_path_line(
        "local",
        "torrent_metadata_path",
        "Torrent metadata",
        snapshot.local.torrent_metadata_path.as_ref(),
    );
    print_path_line(
        "local",
        "config_backups_dir",
        "Config backups",
        snapshot.local.config_backups_dir.as_ref(),
    );
    print_path_line(
        "local",
        "runtime_data_dir",
        "Runtime data",
        snapshot.local.runtime_data_dir.as_ref(),
    );
    print_path_line(
        "local",
        "app_log_dir",
        "App logs",
        snapshot.local.app_log_dir.as_ref(),
    );
    print_path_line(
        "local",
        "cli_log_dir",
        "CLI logs",
        snapshot.local.cli_log_dir.as_ref(),
    );
    print_path_line(
        "local",
        "persistence_dir",
        "Persistence",
        snapshot.local.persistence_dir.as_ref(),
    );
    print_path_line(
        "local",
        "event_journal_file",
        "Event journal",
        snapshot.local.event_journal_file.as_ref(),
    );
    print_path_line(
        "local",
        "status_file",
        "Status file",
        snapshot.local.status_file.as_ref(),
    );
    print_path_line(
        "local",
        "lock_file",
        "Lock file",
        snapshot.local.lock_file.as_ref(),
    );
    print_path_line(
        "local",
        "watch_dir",
        "Watch dir",
        snapshot.local.watch_dir.as_ref(),
    );
    print_path_line(
        "local",
        "processed_dir",
        "Processed dir",
        snapshot.local.processed_dir.as_ref(),
    );

    println!("\nSettings:");
    print_path_line(
        "settings",
        "default_download_folder",
        "Default download folder",
        snapshot.settings.default_download_folder.as_ref(),
    );
    print_path_line(
        "settings",
        "watch_folder",
        "Watch folder",
        snapshot.settings.watch_folder.as_ref(),
    );
    match snapshot.settings.client_port {
        Some(port) => print_value_line("settings", "client_port", "Client port", &port.to_string()),
        None => print_value_line("settings", "client_port", "Client port", "<unavailable>"),
    }
    match snapshot.settings.output_status_interval {
        Some(interval) => print_value_line(
            "settings",
            "output_status_interval",
            "Status interval",
            &format!("{interval} seconds"),
        ),
        None => print_value_line(
            "settings",
            "output_status_interval",
            "Status interval",
            "<unavailable>",
        ),
    }
    if let Some(error) = &snapshot.settings_load_error {
        print_value_line(
            "settings",
            "settings_load_error",
            "Settings load error",
            error,
        );
    }

    println!("\nShared:");
    if let Some(shared) = &snapshot.shared {
        print_value_line(
            "shared",
            "source",
            "Source",
            shared_config_source_label(shared.source),
        );
        print_path_line(
            "shared",
            "mount_root",
            "Mount root",
            Some(&shared.mount_root),
        );
        print_path_line(
            "shared",
            "config_root",
            "Config root",
            Some(&shared.config_root),
        );
        print_path_line(
            "shared",
            "settings_path",
            "Settings",
            Some(&shared.settings_path),
        );
        print_path_line(
            "shared",
            "catalog_path",
            "Catalog",
            Some(&shared.catalog_path),
        );
        print_path_line(
            "shared",
            "torrent_metadata_path",
            "Torrent metadata",
            Some(&shared.torrent_metadata_path),
        );
        print_path_line(
            "shared",
            "torrents_dir",
            "Torrents dir",
            Some(&shared.torrents_dir),
        );
        print_path_line(
            "shared",
            "cluster_revision_file",
            "Cluster revision",
            Some(&shared.cluster_revision_file),
        );
        print_path_line("shared", "lock_file", "Lock file", Some(&shared.lock_file));
        print_path_line("shared", "inbox_dir", "Inbox dir", Some(&shared.inbox_dir));
        print_path_line(
            "shared",
            "processed_dir",
            "Processed dir",
            Some(&shared.processed_dir),
        );
        print_path_line("shared", "data_root", "Data root", Some(&shared.data_root));
        print_path_line("shared", "host_dir", "Host dir", Some(&shared.host_dir));
        print_path_line(
            "shared",
            "host_config_path",
            "Host config",
            Some(&shared.host_config_path),
        );
        print_path_line(
            "shared",
            "host_status_file",
            "Host status",
            Some(&shared.host_status_file),
        );
        print_path_line(
            "shared",
            "leader_status_file",
            "Leader status",
            Some(&shared.leader_status_file),
        );
        print_path_line(
            "shared",
            "host_log_dir",
            "Host logs",
            Some(&shared.host_log_dir),
        );
        print_path_line(
            "shared",
            "host_persistence_dir",
            "Host persistence",
            Some(&shared.host_persistence_dir),
        );
        print_path_line(
            "shared",
            "shared_event_journal_file",
            "Shared event journal",
            Some(&shared.shared_event_journal_file),
        );
    } else {
        println!("  <disabled>");
    }
}

fn print_path_line(section: &str, key: &str, label: &str, path: Option<&PathBuf>) {
    let description = show_config_description(section, key);
    match path {
        Some(path) => print_described_line(label, &path.display().to_string(), description),
        None => print_described_line(label, "<unavailable>", description),
    }
}

fn print_value_line(section: &str, key: &str, label: &str, value: &str) {
    print_described_line(label, value, show_config_description(section, key));
}

fn print_described_line(label: &str, value: &str, description: &str) {
    if description.is_empty() {
        println!("  {}: {}", label, value);
    } else {
        println!("  {}: {} - {}", label, value, description);
    }
}

fn print_path_list(section: &str, key: &str, label: &str, paths: &[PathBuf]) {
    let description = show_config_description(section, key);
    if paths.is_empty() {
        print_described_line(label, "<none>", description);
        return;
    }

    if description.is_empty() {
        println!("  {}:", label);
    } else {
        println!("  {}: {}", label, description);
    }
    for path in paths {
        println!("    - {}", path.display());
    }
}

fn print_show_configs_effective(snapshot: &ShowConfigsSnapshot) {
    println!("\nEffective:");
    print_path_line(
        "effective.config_files",
        "settings_path",
        "Settings",
        snapshot.effective.config_files.settings_path.as_ref(),
    );
    print_path_line(
        "effective.config_files",
        "catalog_path",
        "Catalog",
        snapshot.effective.config_files.catalog_path.as_ref(),
    );
    print_path_line(
        "effective.config_files",
        "torrent_metadata_path",
        "Torrent metadata",
        snapshot
            .effective
            .config_files
            .torrent_metadata_path
            .as_ref(),
    );
    print_path_line(
        "effective.config_files",
        "host_config_path",
        "Host config",
        snapshot.effective.config_files.host_config_path.as_ref(),
    );
    print_path_line(
        "effective",
        "runtime_data_dir",
        "Runtime data",
        snapshot.effective.runtime_data_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "app_log_dir",
        "App logs",
        snapshot.effective.app_log_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "local_app_log_dir",
        "Local app logs",
        snapshot.effective.local_app_log_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "cli_log_dir",
        "CLI logs",
        snapshot.effective.cli_log_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "persistence_dir",
        "Persistence",
        snapshot.effective.persistence_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "event_journal_file",
        "Event journal",
        snapshot.effective.event_journal_file.as_ref(),
    );
    print_path_line(
        "effective",
        "shared_event_journal_file",
        "Shared event journal",
        snapshot.effective.shared_event_journal_file.as_ref(),
    );
    print_path_line(
        "effective",
        "status_file",
        "Status file",
        snapshot.effective.status_file.as_ref(),
    );
    print_path_line(
        "effective",
        "host_status_file",
        "Host status file",
        snapshot.effective.host_status_file.as_ref(),
    );
    print_path_line(
        "effective",
        "lock_file",
        "Lock file",
        snapshot.effective.lock_file.as_ref(),
    );
    print_path_line(
        "effective",
        "command_watch_dir",
        "Command watch dir",
        snapshot.effective.command_watch_dir.as_ref(),
    );
    print_path_line(
        "effective",
        "host_watch_dir",
        "Host watch dir",
        snapshot.effective.host_watch_dir.as_ref(),
    );
    print_path_list(
        "effective",
        "runtime_watch_dirs",
        "Runtime watch dirs",
        &snapshot.effective.runtime_watch_dirs,
    );
}

fn print_show_configs_settings(snapshot: &ShowConfigsSnapshot) {
    println!("\nSettings:");
    print_path_line(
        "settings",
        "default_download_folder",
        "Default download folder",
        snapshot.settings.default_download_folder.as_ref(),
    );
    print_path_line(
        "settings",
        "watch_folder",
        "Watch folder",
        snapshot.settings.watch_folder.as_ref(),
    );
    match snapshot.settings.client_port {
        Some(port) => print_value_line("settings", "client_port", "Client port", &port.to_string()),
        None => print_value_line("settings", "client_port", "Client port", "<unavailable>"),
    }
    match snapshot.settings.output_status_interval {
        Some(interval) => print_value_line(
            "settings",
            "output_status_interval",
            "Status interval",
            &format!("{interval} seconds"),
        ),
        None => print_value_line(
            "settings",
            "output_status_interval",
            "Status interval",
            "<unavailable>",
        ),
    }
    if let Some(error) = &snapshot.settings_load_error {
        print_value_line(
            "settings",
            "settings_load_error",
            "Settings load error",
            error,
        );
    }
}

fn shared_config_source_label(source: SharedConfigSource) -> &'static str {
    match source {
        SharedConfigSource::Env => "env",
        SharedConfigSource::CurrentDirectory => "current_directory",
        SharedConfigSource::Launcher => "launcher",
    }
}

fn host_id_source_label(source: HostIdSource) -> &'static str {
    match source {
        HostIdSource::Env => "env",
        HostIdSource::Launcher => "launcher",
        HostIdSource::Hostname => "hostname",
        HostIdSource::System => "system",
        HostIdSource::Default => "default",
    }
}

fn process_set_host_id_command(host_id: &str, output_mode: OutputMode) -> io::Result<()> {
    let host_id = set_persisted_host_id(host_id)?;
    let sidecar_path = persisted_host_id_path()?;
    print_success(
        output_mode,
        "set-host-id",
        &format!("Persisted host id '{}'.", host_id),
        json!({
            "host_id": host_id,
            "sidecar_path": sidecar_path,
        }),
    );
    Ok(())
}

fn process_clear_host_id_command(output_mode: OutputMode) -> io::Result<()> {
    let cleared = clear_persisted_host_id()?;
    let sidecar_path = persisted_host_id_path()?;
    let message = if cleared {
        "Cleared persisted host id."
    } else {
        "No persisted host id was set."
    };
    print_success(
        output_mode,
        "clear-host-id",
        message,
        json!({
            "cleared": cleared,
            "sidecar_path": sidecar_path,
        }),
    );
    Ok(())
}

fn process_show_host_id_command(output_mode: OutputMode) -> io::Result<()> {
    let selection = effective_host_id_selection()?;
    let sidecar_path = persisted_host_id_path().ok();

    match output_mode {
        OutputMode::Json => {
            print_success(
                output_mode,
                "show-host-id",
                "Resolved host id.",
                json!({
                    "host_id": selection.host_id,
                    "source": selection.source,
                    "sidecar_path": optional_path_json(sidecar_path),
                }),
            );
        }
        OutputMode::Text => {
            println!("Host ID: {}", selection.host_id);
            println!(
                "Source: {}",
                match selection.source {
                    HostIdSource::Env => "env",
                    HostIdSource::Launcher => "launcher",
                    HostIdSource::Hostname => "hostname",
                    HostIdSource::System => "system",
                    HostIdSource::Default => "default",
                }
            );
            print_optional_sidecar_path(sidecar_path.as_ref());
        }
    }

    Ok(())
}

fn process_to_shared_command(path: &std::path::Path, output_mode: OutputMode) -> io::Result<()> {
    let selection = convert_standalone_to_shared(path)?;
    print_success(
        output_mode,
        "to-shared",
        &format!(
            "Converted standalone config to shared config at {}.",
            selection.mount_root.display()
        ),
        json!({
            "selection": shared_config_selection_json(&selection),
        }),
    );
    Ok(())
}

fn process_to_standalone_command(output_mode: OutputMode) -> io::Result<()> {
    convert_shared_to_standalone()?;
    print_success(
        output_mode,
        "to-standalone",
        "Converted shared config to standalone config.",
        json!({}),
    );
    Ok(())
}

fn process_cli_request(
    cli: &Cli,
    settings: &Settings,
    shared_mode: bool,
    leader_is_running: bool,
    output_mode: OutputMode,
) -> io::Result<()> {
    if let Some(direct_input) = &cli.input {
        #[cfg(all(feature = "dht", feature = "pex"))]
        tracing::info!(target: "superseedr", "Processing direct input: {}", direct_input);
        let command_path = queue_direct_input_command(settings, direct_input)?;
        print_success(
            output_mode,
            "add",
            &format!("Queued add command at {}", command_path.display()),
            json!({
                "queued": [{
                    "input": direct_input,
                    "command_path": command_path,
                }]
            }),
        );
        return Ok(());
    }

    let Some(command) = &cli.command else {
        return Ok(());
    };

    match command {
        Commands::Add {
            inputs,
            validated,
            path,
        } => {
            let mut queued = Vec::new();
            let download_path = match path {
                Some(path) => Some(validate_add_download_path(path)?),
                None => None,
            };
            let mut offline_add_settings = settings.clone();
            for input in expand_add_inputs(inputs) {
                tracing::info!(target: "superseedr", "Processing Add subcommand input: {}", input);
                if *validated || download_path.is_some() {
                    let request = add_control_request_for_input(
                        &input,
                        download_path.clone(),
                        *validated,
                        shared_mode,
                    )?;
                    let result = if shared_mode && leader_is_running {
                        let _ = queue_control_request_command(settings, &request)?;
                        let message = online_control_success_message(&request);
                        if output_mode == OutputMode::Text {
                            print_queued_control_message(
                                &request,
                                true,
                                leader_is_running,
                                output_mode,
                            );
                        }
                        json!({
                            "input": input,
                            "queued": true,
                            "pending_leader": false,
                            "request": request,
                            "message": message,
                        })
                    } else if leader_is_running {
                        let _ = queue_control_request_command(settings, &request)?;
                        let message = online_control_success_message(&request);
                        if output_mode == OutputMode::Text {
                            print_success(
                                output_mode,
                                request.action_name(),
                                &message,
                                json!({ "queued": true, "request": request }),
                            );
                        }
                        json!({
                            "input": input,
                            "queued": true,
                            "request": request,
                            "message": message,
                        })
                    } else {
                        let message =
                            apply_offline_control_request_mut(&mut offline_add_settings, &request)?;
                        if output_mode == OutputMode::Text {
                            print_success(
                                output_mode,
                                request.action_name(),
                                &message,
                                json!({ "applied": true, "request": request, "message": message }),
                            );
                        }
                        json!({
                            "input": input,
                            "applied": true,
                            "request": request,
                            "message": message,
                        })
                    };
                    queued.push(result);
                } else {
                    let command_path = queue_direct_input_command(settings, &input)?;
                    if output_mode == OutputMode::Text {
                        println!("Queued add command at {}", command_path.display());
                    }
                    queued.push(json!({
                        "input": input,
                        "command_path": command_path,
                    }));
                }
            }
            if output_mode == OutputMode::Json {
                print_success(
                    output_mode,
                    "add",
                    if *validated || download_path.is_some() {
                        "Processed add request(s)."
                    } else {
                        "Queued add command(s)."
                    },
                    json!({ "queued": queued }),
                );
            }
            Ok(())
        }
        Commands::Journal { catalog_recovery } => {
            process_journal_command(settings, *catalog_recovery, output_mode)?;
            Ok(())
        }
        Commands::SetSharedConfig { path } => process_set_shared_config_command(path, output_mode),
        Commands::SetNetworkInterface { interface } => process_set_network_interface_command(
            settings,
            interface,
            leader_is_running,
            shared_mode,
            output_mode,
        ),
        Commands::ClearSharedConfig => process_clear_shared_config_command(output_mode),
        Commands::ShowSharedConfig => process_show_shared_config_command(output_mode),
        Commands::ShowConfigs { all } => {
            process_show_configs_command(Some(settings), None, *all, output_mode)
        }
        Commands::Torrents => {
            process_torrents_command(settings, output_mode).map_err(io::Error::other)
        }
        Commands::Info { target } => {
            process_info_command(settings, target, output_mode).map_err(io::Error::other)
        }
        Commands::Files { target } => {
            process_files_command(settings, target, output_mode).map_err(io::Error::other)
        }
        Commands::Status { .. } => {
            let status_mode = status_command_mode(command)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
            let request = status_control_request(command)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
            if shared_mode {
                process_shared_status_request(settings, status_mode, leader_is_running, output_mode)
            } else if leader_is_running {
                process_online_status_request(settings, &request, status_mode, output_mode)
            } else {
                process_offline_control_request(settings, &request, output_mode)
            }
        }
        Commands::StopClient => {
            if !leader_is_running {
                print_success(
                    output_mode,
                    "stop-client",
                    "superseedr is not running.",
                    json!({ "running": false }),
                );
                return Ok(());
            }
            tracing::info!(target: "superseedr", "Processing StopClient command.");
            let _ = queue_runtime_stop_command(settings)?;
            print_success(
                output_mode,
                "stop-client",
                "Queued stop request.",
                json!({ "queued": true }),
            );
            Ok(())
        }
        Commands::Purge { targets } => {
            let resolved_targets = require_cli_targets(targets, "purge")
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
            let mut requests = Vec::new();
            for target in resolved_targets {
                let info_hash_hex =
                    resolve_purge_target_info_hash(settings, &target).map_err(io::Error::other)?;
                requests.push(ControlRequest::Delete {
                    info_hash_hex,
                    delete_files: true,
                });
            }
            process_control_requests(
                settings,
                &requests,
                "purge",
                shared_mode,
                leader_is_running,
                output_mode,
            )
        }
        Commands::Move {
            info_hash_hex,
            path,
        } => {
            if leader_is_running {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Stop the running superseedr client before using the move command.",
                ));
            }
            let request = build_move_torrent_request(settings, info_hash_hex, path)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
            process_control_requests(
                settings,
                &[request],
                "move",
                shared_mode,
                leader_is_running,
                output_mode,
            )
        }
        _ => {
            let requests =
                command_to_control_requests_with_resolver(command, |target, command_name| {
                    resolve_target_info_hash(settings, target, command_name)
                })
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Unsupported command")
                })?;

            let command_name = cli_command_name(Some(command))
                .or_else(|| requests.first().map(ControlRequest::action_name))
                .unwrap_or("control");
            process_control_requests(
                settings,
                &requests,
                command_name,
                shared_mode,
                leader_is_running,
                output_mode,
            )
        }
    }
}

fn resolve_cli_command_sink(settings: &Settings) -> io::Result<PathBuf> {
    resolve_command_watch_path(settings).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve the command watch path",
        )
    })
}

fn queue_direct_input_command(settings: &Settings, input: &str) -> io::Result<PathBuf> {
    let watch_path = resolve_cli_command_sink(settings)?;
    if input.starts_with("magnet:") {
        return write_input_command(input, &watch_path);
    }

    let absolute_path = fs::canonicalize(input)?;
    if is_shared_config_mode() {
        if let Some(relative_payload) = config::encode_shared_cli_torrent_path(&absolute_path)? {
            return write_path_command_payload(
                &relative_payload,
                absolute_path.to_string_lossy().as_ref(),
                &watch_path,
            );
        }
    }

    write_input_command(input, &watch_path)
}

fn validate_add_download_path(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Add --path must not be empty",
        ));
    }
    if !path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Add --path does not exist: {}", path.display()),
        ));
    }
    if !path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Add --path must be a directory: {}", path.display()),
        ));
    }
    fs::canonicalize(path)
}

fn add_control_request_for_input(
    input: &str,
    download_path: Option<PathBuf>,
    validation_status: bool,
    shared_mode: bool,
) -> io::Result<ControlRequest> {
    if input.starts_with("magnet:") {
        return Ok(ControlRequest::AddMagnet {
            magnet_link: input.to_string(),
            download_path,
            container_name: None,
            validation_status,
            file_priorities: Vec::new(),
        });
    }

    let source_path = control_torrent_source_path(input, shared_mode)?;
    Ok(ControlRequest::AddTorrentFile {
        source_path,
        download_path,
        container_name: None,
        validation_status,
        file_priorities: Vec::new(),
    })
}

fn control_torrent_source_path(input: &str, shared_mode: bool) -> io::Result<PathBuf> {
    let source_path = fs::canonicalize(input)?;
    if !shared_mode {
        return Ok(source_path);
    }

    stage_shared_control_torrent_file(&source_path)
}

fn stage_shared_control_torrent_file(source_path: &Path) -> io::Result<PathBuf> {
    let Some(shared_root) = shared_root_path() else {
        return Ok(source_path.to_path_buf());
    };
    let staging_dir = shared_root.join("staged-adds");
    fs::create_dir_all(&staging_dir)?;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut random_bytes = [0_u8; 8];
    rand::rng().fill_bytes(&mut random_bytes);
    let staged_path = staging_dir.join(format!(
        "staged-{}-{}.torrent",
        now_ms,
        hex::encode(random_bytes)
    ));
    fs::copy(source_path, &staged_path)?;
    Ok(staged_path)
}

fn queue_runtime_stop_command(settings: &Settings) -> io::Result<PathBuf> {
    let watch_path = resolve_cli_command_sink(settings)?;
    write_stop_command(&watch_path)
}

fn queue_control_request_command(
    settings: &Settings,
    request: &ControlRequest,
) -> io::Result<PathBuf> {
    let watch_path = resolve_cli_command_sink(settings)?;
    write_control_command(request, &watch_path)
}

fn print_queued_control_message(
    request: &ControlRequest,
    shared_mode: bool,
    leader_is_running: bool,
    output_mode: OutputMode,
) {
    let message = if shared_mode && !leader_is_running {
        format!(
            "Queued {} request pending leader availability.",
            request.action_name()
        )
    } else {
        online_control_success_message(request)
    };

    if shared_mode && !leader_is_running {
        print_success(
            output_mode,
            request.action_name(),
            &message,
            json!({ "queued": true, "pending_leader": true, "request": request }),
        );
    } else {
        print_success(
            output_mode,
            request.action_name(),
            &message,
            json!({ "queued": true, "pending_leader": false, "request": request }),
        );
    }
}

fn process_shared_status_request(
    settings: &Settings,
    mode: StatusCommandMode,
    leader_is_running: bool,
    output_mode: OutputMode,
) -> io::Result<()> {
    match mode {
        StatusCommandMode::Snapshot => {
            if !leader_is_running {
                let raw = offline_output_json(settings)?;
                return print_json_passthrough(output_mode, "status", &raw);
            }

            match fs::read_to_string(status_file_path()?) {
                Ok(raw) => print_json_passthrough(output_mode, "status", &raw),
                Err(_) => {
                    let raw = offline_output_json(settings)?;
                    print_json_passthrough(output_mode, "status", &raw)
                }
            }
        }
        StatusCommandMode::Follow { interval_secs } => {
            let mut last_modified_at = status_file_modified_at()?;
            loop {
                let raw = wait_for_status_json_after(
                    last_modified_at,
                    Duration::from_secs(interval_secs.saturating_mul(3).max(15)),
                )?;
                print_json_passthrough(output_mode, "status", &raw)?;
                io::stdout().flush()?;
                last_modified_at = status_file_modified_at()?;
            }
        }
        StatusCommandMode::SetInterval { .. } | StatusCommandMode::Stop => Err(io::Error::other(
            "Shared mode leader status snapshots are always enabled every 5 seconds; start/stop is not supported in shared mode",
        )),
    }
}

fn process_online_status_request(
    settings: &Settings,
    request: &ControlRequest,
    mode: StatusCommandMode,
    output_mode: OutputMode,
) -> io::Result<()> {
    match mode {
        StatusCommandMode::Snapshot => {
            let previous_modified_at = status_file_modified_at()?;
            let _ = queue_control_request_command(settings, request)?;
            let raw = wait_for_status_json_after(previous_modified_at, Duration::from_secs(15))?;
            print_json_passthrough(output_mode, "status", &raw)
        }
        StatusCommandMode::Follow { interval_secs } => {
            let mut last_modified_at = status_file_modified_at()?;
            let _ = queue_control_request_command(settings, request)?;
            loop {
                let raw = wait_for_status_json_after(
                    last_modified_at,
                    Duration::from_secs(interval_secs.saturating_mul(3).max(15)),
                )?;
                print_json_passthrough(output_mode, "status", &raw)?;
                io::stdout().flush()?;
                last_modified_at = status_file_modified_at()?;
            }
        }
        StatusCommandMode::SetInterval { interval_secs } => {
            let _ = queue_control_request_command(settings, request)?;
            let status_path = status_file_path()?;
            print_success(
                output_mode,
                "status",
                &format!(
                    "Set status output interval to {} seconds.\nStatus file: {}",
                    interval_secs,
                    status_path.display()
                ),
                json!({
                    "message": "Set status output interval.",
                    "interval_secs": interval_secs,
                    "status_file": status_path,
                }),
            );
            Ok(())
        }
        StatusCommandMode::Stop => {
            let _ = queue_control_request_command(settings, request)?;
            print_success(
                output_mode,
                "status",
                "Queued status streaming stop request.",
                json!({ "queued": true, "follow": false }),
            );
            Ok(())
        }
    }
}

fn process_offline_control_request(
    settings: &Settings,
    request: &ControlRequest,
    output_mode: OutputMode,
) -> io::Result<()> {
    let mut next_settings = settings.clone();
    if matches!(request, ControlRequest::StatusNow) {
        let raw = offline_output_json(&next_settings)?;
        return print_json_passthrough(output_mode, "status", &raw);
    }
    let message = apply_offline_control_request_mut(&mut next_settings, request)?;
    print_success(
        output_mode,
        request.action_name(),
        &message,
        json!({ "applied": true, "request": request, "message": message }),
    );
    Ok(())
}

fn process_control_requests(
    settings: &Settings,
    requests: &[ControlRequest],
    command_name: &str,
    shared_mode: bool,
    leader_is_running: bool,
    output_mode: OutputMode,
) -> io::Result<()> {
    let mut results = Vec::new();
    let mut offline_settings = settings.clone();

    for request in requests {
        let result = if shared_mode && leader_is_running {
            let _ = queue_control_request_command(settings, request)?;
            let message = online_control_success_message(request);
            if output_mode == OutputMode::Text {
                print_queued_control_message(request, true, leader_is_running, output_mode);
            }
            json!({
                "queued": true,
                "pending_leader": false,
                "request": request,
                "message": message,
            })
        } else if leader_is_running {
            let _ = queue_control_request_command(settings, request)?;
            let message = online_control_success_message(request);
            if output_mode == OutputMode::Text {
                print_success(
                    output_mode,
                    request.action_name(),
                    &message,
                    json!({ "queued": true, "request": request }),
                );
            }
            json!({
                "queued": true,
                "request": request,
                "message": message,
            })
        } else {
            if matches!(request, ControlRequest::StatusNow) {
                let raw = offline_output_json(&offline_settings)?;
                if output_mode == OutputMode::Json {
                    return print_json_passthrough(output_mode, "status", &raw);
                }
                print_json_passthrough(output_mode, "status", &raw)?;
                continue;
            }
            let message = apply_offline_control_request_mut(&mut offline_settings, request)?;
            if output_mode == OutputMode::Text {
                print_success(
                    output_mode,
                    request.action_name(),
                    &message,
                    json!({ "applied": true, "request": request, "message": message }),
                );
            }
            json!({
                "applied": true,
                "request": request,
                "message": message,
            })
        };

        results.push(result);
    }

    if output_mode == OutputMode::Json {
        print_success(
            output_mode,
            command_name,
            "Processed control request(s).",
            json!({ "results": results }),
        );
    }

    Ok(())
}

fn apply_offline_control_request_mut(
    settings: &mut Settings,
    request: &ControlRequest,
) -> io::Result<String> {
    let result = apply_offline_control_request_mut_with_save(settings, request, |settings| {
        config::save_settings(settings)
    });
    record_offline_control_journal_entry(request, &result);
    result.map_err(io::Error::other)
}

fn apply_offline_control_request_mut_with_save<F>(
    settings: &mut Settings,
    request: &ControlRequest,
    save_settings: F,
) -> Result<String, String>
where
    F: FnOnce(&Settings) -> io::Result<()>,
{
    match request {
        ControlRequest::StatusNow => {
            return Err(
                "Status snapshot requests should use process_offline_control_request".to_string(),
            );
        }
        ControlRequest::StatusFollowStart { .. } | ControlRequest::StatusFollowStop => {
            return Err(
                "Streaming status commands require a running superseedr instance".to_string(),
            );
        }
        _ => {}
    }

    if let ControlRequest::MoveTorrent {
        info_hash_hex,
        download_path,
    } = request
    {
        let previous_settings = settings.clone();
        let transaction = prepare_offline_move_transaction(settings, info_hash_hex, download_path)?;
        *settings = transaction.next_settings().clone();
        // Sources remain intact until this save commits. At every interruption point,
        // either the old catalog can read the source or the new catalog can read the staged copy.
        if let Err(error) = save_settings(settings) {
            *settings = previous_settings;
            return Err(format!(
                "Failed to save updated settings: {}. Both source and staged destination files were retained because the catalog commit status is ambiguous; verify the persisted download path before retrying.",
                error
            ));
        }
        return Ok(transaction.commit());
    }

    let mut result = match request {
        ControlRequest::Delete {
            info_hash_hex,
            delete_files: true,
        } => apply_offline_purge(settings, info_hash_hex),
        _ => apply_offline_control_request(settings, request),
    };
    if result.is_ok() {
        if let Err(error) = save_settings(settings) {
            result = Err(format!("Failed to save updated settings: {}", error));
        }
    }
    result
}

fn process_files_command(
    settings: &Settings,
    target: &str,
    output_mode: OutputMode,
) -> Result<(), String> {
    let info_hash_hex = resolve_target_info_hash(settings, target, "files")?;
    let files = list_torrent_files(settings, &info_hash_hex)?;
    if files.is_empty() {
        return Err(format!(
            "Torrent '{}' does not have any persisted file entries",
            info_hash_hex
        ));
    }

    if output_mode == OutputMode::Json {
        print_success(
            output_mode,
            "files",
            "Listed torrent files.",
            json!({ "info_hash_hex": info_hash_hex, "files": files }),
        );
    } else {
        for file in files {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                file.file_index,
                file.length,
                file.relative_path,
                file.full_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<unavailable>".to_string()),
                file_priority_label(file.priority),
            );
        }
    }

    Ok(())
}

fn file_priority_label(priority: crate::app::FilePriority) -> &'static str {
    match priority {
        crate::app::FilePriority::Normal => "normal",
        crate::app::FilePriority::High => "high",
        crate::app::FilePriority::Skip => "skip",
        crate::app::FilePriority::Mixed => "mixed",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CatalogRecoveryStatus {
    AlreadyInCatalog,
    Recoverable,
    SourceMissing,
    SourceHashMismatch,
    UnsupportedSource,
}

impl CatalogRecoveryStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::AlreadyInCatalog => "already_in_catalog",
            Self::Recoverable => "recoverable",
            Self::SourceMissing => "source_missing",
            Self::SourceHashMismatch => "source_hash_mismatch",
            Self::UnsupportedSource => "unsupported_source",
        }
    }
}

#[derive(Debug, Clone)]
struct CatalogRecoveryCandidate {
    event_id: u64,
    ts_iso: String,
    info_hash_hex: String,
    source_path: Option<PathBuf>,
    payload_path: Option<PathBuf>,
    recovered_validation_status: bool,
    status: CatalogRecoveryStatus,
}

fn catalog_info_hashes(settings: &Settings) -> HashSet<String> {
    settings
        .torrents
        .iter()
        .filter_map(|torrent| info_hash_from_torrent_source(&torrent.torrent_or_magnet))
        .map(hex::encode)
        .collect()
}

fn processed_source_candidates(source_path: &Path) -> Vec<PathBuf> {
    let Some(file_name) = source_path.file_name() else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    if source_path.exists() {
        candidates.push(source_path.to_path_buf());
    }
    if let Some(shared_processed) = shared_processed_path() {
        candidates.push(shared_processed.join(file_name));
    }
    if let Some((_, processed)) = get_watch_path() {
        candidates.push(processed.join(file_name));
    }
    candidates
}

fn recover_source_from_journal_entry(entry: &EventJournalEntry) -> Option<(String, PathBuf)> {
    let source_path = entry.source_path.as_ref()?;
    let ingest_kind = match &entry.details {
        EventDetails::Ingest { ingest_kind, .. } => *ingest_kind,
        _ => return None,
    };

    for candidate in processed_source_candidates(source_path) {
        if !candidate.exists() {
            continue;
        }

        match ingest_kind {
            IngestKind::MagnetFile => {
                let Ok(content) = fs::read_to_string(&candidate) else {
                    continue;
                };
                let magnet = content.trim();
                if magnet.starts_with("magnet:") {
                    return Some((magnet.to_string(), candidate));
                }
            }
            IngestKind::PathFile => {
                let Ok(content) = fs::read_to_string(&candidate) else {
                    continue;
                };
                let Ok(torrent_path) =
                    crate::config::resolve_shared_cli_torrent_path(Path::new(content.trim()))
                else {
                    continue;
                };
                if torrent_path.exists() {
                    return Some((torrent_path.to_string_lossy().to_string(), torrent_path));
                }
            }
            IngestKind::TorrentFile => {
                return Some((candidate.to_string_lossy().to_string(), candidate));
            }
        }
    }

    None
}

fn recovered_source_info_hash(source: &str, recovered_path: &Path) -> Option<Vec<u8>> {
    if source.starts_with("magnet:") {
        return info_hash_from_torrent_source(source);
    }

    fs::read(recovered_path)
        .ok()
        .and_then(|bytes| info_hash_from_torrent_bytes(&bytes))
}

fn ingest_payload_path(entry: &EventJournalEntry) -> Option<PathBuf> {
    match &entry.details {
        EventDetails::Ingest { payload_path, .. } => payload_path.clone(),
        _ => None,
    }
}

fn recovered_validation_status(entry: &EventJournalEntry) -> bool {
    ingest_payload_path(entry)
        .as_deref()
        .is_some_and(|path| path.exists())
}

fn analyze_catalog_recovery(
    settings: &Settings,
    journal: &EventJournalState,
) -> Vec<CatalogRecoveryCandidate> {
    let mut known_hashes = catalog_info_hashes(settings);
    let mut candidates = Vec::new();

    for entry in journal.entries.iter().rev() {
        if entry.category != EventCategory::Ingest || entry.event_type != EventType::IngestAdded {
            continue;
        }
        if !matches!(
            entry.details,
            EventDetails::Ingest {
                ingest_kind: IngestKind::MagnetFile
                    | IngestKind::TorrentFile
                    | IngestKind::PathFile,
                ..
            }
        ) {
            continue;
        }

        let Some(info_hash_hex) = entry
            .info_hash_hex
            .as_ref()
            .map(|value| value.to_ascii_lowercase())
        else {
            continue;
        };

        if known_hashes.contains(&info_hash_hex) {
            candidates.push(CatalogRecoveryCandidate {
                event_id: entry.id,
                ts_iso: entry.ts_iso.clone(),
                info_hash_hex,
                source_path: entry.source_path.clone(),
                payload_path: ingest_payload_path(entry),
                recovered_validation_status: recovered_validation_status(entry),
                status: CatalogRecoveryStatus::AlreadyInCatalog,
            });
            continue;
        }

        let status = match recover_source_from_journal_entry(entry) {
            Some((source, recovered_path)) => {
                if recovered_source_info_hash(&source, &recovered_path)
                    .map(|hash| hex::encode(hash).eq_ignore_ascii_case(&info_hash_hex))
                    .unwrap_or(false)
                {
                    known_hashes.insert(info_hash_hex.clone());
                    CatalogRecoveryStatus::Recoverable
                } else {
                    CatalogRecoveryStatus::SourceHashMismatch
                }
            }
            None if entry.source_path.is_some() => CatalogRecoveryStatus::SourceMissing,
            None => CatalogRecoveryStatus::UnsupportedSource,
        };

        candidates.push(CatalogRecoveryCandidate {
            event_id: entry.id,
            ts_iso: entry.ts_iso.clone(),
            info_hash_hex,
            source_path: entry.source_path.clone(),
            payload_path: ingest_payload_path(entry),
            recovered_validation_status: recovered_validation_status(entry),
            status,
        });
    }

    candidates.reverse();
    candidates
}

fn print_catalog_recovery_report(candidates: &[CatalogRecoveryCandidate], output_mode: OutputMode) {
    let recoverable = candidates
        .iter()
        .filter(|candidate| candidate.status == CatalogRecoveryStatus::Recoverable)
        .count();
    let already_in_catalog = candidates
        .iter()
        .filter(|candidate| candidate.status == CatalogRecoveryStatus::AlreadyInCatalog)
        .count();

    if output_mode == OutputMode::Json {
        print_success(
            output_mode,
            "journal",
            "Analyzed catalog recovery from journal.",
            json!({
                "recoverable": recoverable,
                "already_in_catalog": already_in_catalog,
                "candidates": candidates.iter().map(|candidate| json!({
                    "event_id": candidate.event_id,
                    "ts_iso": candidate.ts_iso,
                    "info_hash_hex": candidate.info_hash_hex,
                    "source_path": candidate.source_path,
                    "payload_path": candidate.payload_path,
                    "recovered_validation_status": candidate.recovered_validation_status,
                    "status": candidate.status.as_str(),
                })).collect::<Vec<_>>(),
            }),
        );
        return;
    }

    println!(
        "Catalog recovery: {} recoverable, {} already in catalog",
        recoverable, already_in_catalog
    );
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.status != CatalogRecoveryStatus::AlreadyInCatalog)
    {
        println!(
            "{}\tverified={}\t{}\t{}\t{}\t{}",
            candidate.status.as_str(),
            candidate.recovered_validation_status,
            candidate.info_hash_hex,
            candidate.event_id,
            candidate
                .source_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            candidate
                .payload_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        );
    }
}

fn process_journal_command(
    settings: &Settings,
    catalog_recovery: bool,
    output_mode: OutputMode,
) -> io::Result<()> {
    if catalog_recovery {
        let journal = load_event_journal_state();
        let candidates = analyze_catalog_recovery(settings, &journal);
        print_catalog_recovery_report(&candidates, output_mode);
        return Ok(());
    }

    match output_mode {
        OutputMode::Json => {
            let raw = event_journal_json()?;
            print_json_passthrough(output_mode, "journal", &raw)
        }
        OutputMode::Text => {
            let journal = load_event_journal_state();
            if journal.entries.is_empty() {
                println!("No journal entries.");
                return Ok(());
            }

            for (index, entry) in journal.entries.iter().enumerate() {
                if index > 0 {
                    println!();
                }

                println!("#{} {} {:?}", entry.id, entry.ts_iso, entry.event_type);
                println!("Scope: {:?}", entry.scope);
                println!("Category: {:?}", entry.category);
                if let Some(host_id) = &entry.host_id {
                    println!("Host: {}", host_id);
                }
                if let Some(torrent_name) = &entry.torrent_name {
                    println!("Torrent: {}", torrent_name);
                }
                if let Some(info_hash_hex) = &entry.info_hash_hex {
                    println!("Hash: {}", info_hash_hex);
                }
                if let Some(message) = &entry.message {
                    println!("Message: {}", message);
                }
                if let Some(source_path) = &entry.source_path {
                    println!("Source: {}", source_path.display());
                }
                if let Some(source_watch_folder) = &entry.source_watch_folder {
                    println!("Watch Folder: {}", source_watch_folder.display());
                }
                println!("Details: {}", format_event_details(&entry.details));
            }

            Ok(())
        }
    }
}

fn process_torrents_command(settings: &Settings, output_mode: OutputMode) -> Result<(), String> {
    if settings.torrents.is_empty() {
        print_success(
            output_mode,
            "torrents",
            "No torrents configured.",
            json!({ "torrents": [] }),
        );
        return Ok(());
    }

    if output_mode == OutputMode::Json {
        let torrents = settings
            .torrents
            .iter()
            .map(|torrent| torrent_details_value(settings, torrent))
            .collect::<Vec<_>>();
        print_success(
            output_mode,
            "torrents",
            "Listed torrents.",
            json!({ "torrents": torrents }),
        );
    } else {
        for (index, torrent) in settings.torrents.iter().enumerate() {
            if index > 0 {
                println!();
            }

            print_torrent_details(settings, torrent);
        }
    }

    Ok(())
}

fn process_info_command(
    settings: &Settings,
    target: &str,
    output_mode: OutputMode,
) -> Result<(), String> {
    let info_hash_hex = resolve_target_info_hash(settings, target, "info")?;
    let torrent = settings
        .torrents
        .iter()
        .find(|torrent| {
            info_hash_from_torrent_source(&torrent.torrent_or_magnet)
                .map(hex::encode)
                .as_deref()
                == Some(info_hash_hex.as_str())
        })
        .ok_or_else(|| format!("Torrent '{}' was not found", info_hash_hex))?;

    if output_mode == OutputMode::Json {
        print_success(
            output_mode,
            "info",
            "Loaded torrent info.",
            json!({ "torrent": torrent_details_value(settings, torrent) }),
        );
    } else {
        print_torrent_details(settings, torrent);
    }
    Ok(())
}

fn print_torrent_details(settings: &Settings, torrent: &crate::config::TorrentSettings) {
    let info_hash_hex = info_hash_from_torrent_source(&torrent.torrent_or_magnet).map(hex::encode);

    println!("Name: {}", torrent.name);
    println!(
        "Hex: {}",
        info_hash_hex.as_deref().unwrap_or("<unavailable>")
    );
    println!("Source: {}", torrent.torrent_or_magnet);
    println!("Files:");

    match info_hash_hex.as_deref() {
        Some(info_hash_hex) => match list_torrent_files(settings, info_hash_hex) {
            Ok(files) if !files.is_empty() => {
                for file in files {
                    println!(
                        "  {}\t{}\t{}\t{}\t{}",
                        file.file_index,
                        file.length,
                        file.relative_path,
                        file.full_path
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "<unavailable>".to_string()),
                        file_priority_label(file.priority),
                    );
                }
            }
            Ok(_) => println!("  <none>"),
            Err(error) => println!("  <unavailable: {}>", error),
        },
        None => println!("  <unavailable: info hash could not be derived>"),
    }
}

fn format_event_details(details: &crate::persistence::event_journal::EventDetails) -> String {
    match details {
        crate::persistence::event_journal::EventDetails::None => "none".to_string(),
        crate::persistence::event_journal::EventDetails::Ingest {
            origin,
            ingest_kind,
            download_path,
            container_name,
            payload_path,
        } => {
            let mut details = format!("ingest origin={origin:?} kind={ingest_kind:?}");
            if let Some(path) = download_path {
                details.push_str(&format!(" download_path={}", path.display()));
            }
            if let Some(name) = container_name {
                details.push_str(&format!(" container_name={}", name));
            }
            if let Some(path) = payload_path {
                details.push_str(&format!(" payload_path={}", path.display()));
            }
            details
        }
        crate::persistence::event_journal::EventDetails::DataHealth {
            issue_count,
            issue_files,
        } => {
            if issue_files.is_empty() {
                format!("data_health issue_count={issue_count}")
            } else {
                format!(
                    "data_health issue_count={} files={}",
                    issue_count,
                    issue_files.join(", ")
                )
            }
        }
        crate::persistence::event_journal::EventDetails::Control {
            origin,
            action,
            target_info_hash_hex,
            file_index,
            file_path,
            priority,
        } => {
            let mut parts = vec![format!("control origin={origin:?} action={action}")];
            if let Some(target) = target_info_hash_hex {
                parts.push(format!("target={target}"));
            }
            if let Some(file_index) = file_index {
                parts.push(format!("file_index={file_index}"));
            }
            if let Some(file_path) = file_path {
                parts.push(format!("file_path={file_path}"));
            }
            if let Some(priority) = priority {
                parts.push(format!("priority={priority}"));
            }
            parts.join(" ")
        }
        crate::persistence::event_journal::EventDetails::Network {
            interface,
            generation_id,
            listen_port,
        } => {
            let mut parts = vec!["network".to_string()];
            if let Some(interface) = interface {
                parts.push(format!("interface={interface}"));
            }
            if let Some(generation_id) = generation_id {
                parts.push(format!("generation={generation_id}"));
            }
            if let Some(listen_port) = listen_port {
                parts.push(format!("listen_port={listen_port}"));
            }
            parts.join(" ")
        }
    }
}

fn torrent_details_value(settings: &Settings, torrent: &crate::config::TorrentSettings) -> Value {
    let info_hash_hex = info_hash_from_torrent_source(&torrent.torrent_or_magnet).map(hex::encode);
    let (files, files_error) = match info_hash_hex.as_deref() {
        Some(info_hash_hex) => match list_torrent_files(settings, info_hash_hex) {
            Ok(files) => (json!(files), Value::Null),
            Err(error) => (json!([]), json!(error)),
        },
        None => (json!([]), json!("info hash could not be derived")),
    };

    json!({
        "name": torrent.name,
        "info_hash_hex": info_hash_hex,
        "source": torrent.torrent_or_magnet,
        "download_path": torrent.download_path,
        "container_name": torrent.container_name,
        "torrent_control_state": torrent.torrent_control_state,
        "delete_files": torrent.delete_files,
        "file_priorities": torrent.file_priorities,
        "files": files,
        "files_error": files_error,
    })
}

fn cli_command_name(command: Option<&Commands>) -> Option<&'static str> {
    match command {
        Some(Commands::Add { .. }) => Some("add"),
        Some(Commands::StopClient) => Some("stop-client"),
        Some(Commands::Journal { .. }) => Some("journal"),
        Some(Commands::SetSharedConfig { .. }) => Some("set-shared-config"),
        Some(Commands::SetNetworkInterface { .. }) => Some("set-network-interface"),
        Some(Commands::ClearSharedConfig) => Some("clear-shared-config"),
        Some(Commands::ShowSharedConfig) => Some("show-shared-config"),
        Some(Commands::ShowConfigs { .. }) => Some("show-configs"),
        Some(Commands::SetHostId { .. }) => Some("set-host-id"),
        Some(Commands::ClearHostId) => Some("clear-host-id"),
        Some(Commands::ShowHostId) => Some("show-host-id"),
        Some(Commands::ToShared { .. }) => Some("to-shared"),
        Some(Commands::ToStandalone) => Some("to-standalone"),
        Some(Commands::Torrents) => Some("torrents"),
        Some(Commands::Info { .. }) => Some("info"),
        Some(Commands::Status { .. }) => Some("status"),
        Some(Commands::Pause { .. }) => Some("pause"),
        Some(Commands::Resume { .. }) => Some("resume"),
        Some(Commands::Remove { .. }) => Some("remove"),
        Some(Commands::Purge { .. }) => Some("purge"),
        Some(Commands::Files { .. }) => Some("files"),
        Some(Commands::Priority { .. }) => Some("priority"),
        Some(Commands::Move { .. }) => Some("move"),
        #[cfg(feature = "synthetic-load")]
        Some(Commands::Benchmark(_)) => Some("benchmark"),
        #[cfg(feature = "synthetic-load")]
        Some(Commands::SyntheticLoad(_)) => Some("synthetic-load"),
        None => None,
    }
}

fn print_success(output_mode: OutputMode, command: &str, message: &str, data: Value) {
    match output_mode {
        OutputMode::Text => println!("{}", message),
        OutputMode::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "command": command,
                    "data": data,
                }))
                .expect("serialize cli success envelope")
            );
        }
    }
}

fn print_json_error(command: Option<&str>, error: &str) {
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "ok": false,
            "command": command,
            "error": error,
        }))
        .expect("serialize cli error envelope")
    );
}

fn print_json_passthrough(
    output_mode: OutputMode,
    command: &str,
    raw_json: &str,
) -> io::Result<()> {
    match output_mode {
        OutputMode::Text => {
            println!("{}", raw_json);
            Ok(())
        }
        OutputMode::Json => {
            let parsed: Value = serde_json::from_str(raw_json)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "command": command,
                    "data": parsed,
                }))
                .map_err(io::Error::other)?
            );
            Ok(())
        }
    }
}

fn record_offline_control_journal_entry(request: &ControlRequest, result: &Result<String, String>) {
    let mut journal = load_event_journal_state();
    let event_type = if result.is_ok() {
        EventType::ControlApplied
    } else {
        EventType::ControlFailed
    };
    let message = match result {
        Ok(message) | Err(message) => Some(message.clone()),
    };
    append_event_journal_entry(
        &mut journal,
        EventJournalEntry {
            scope: EventScope::Host,
            host_id: config::shared_host_id(),
            ts_iso: chrono::Utc::now().to_rfc3339(),
            category: EventCategory::Control,
            event_type,
            message,
            details: control_event_details(request, ControlOrigin::CliOffline),
            ..Default::default()
        },
    );
    if let Err(error) = save_event_journal_state(&journal) {
        tracing::error!(target: "superseedr", "Failed to save offline control journal entry: {}", error);
    }
}

fn cleanup_terminal() -> Result<(), Box<dyn std::error::Error>> {
    let _ = disable_raw_mode();
    // Common cleanup for all platforms
    let _ = execute!(stdout(), LeaveAlternateScreen,);
    let _ = execute!(stdout(), DisableBracketedPaste);

    #[cfg(not(windows))]
    {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }

    Ok(())
}

fn generate_client_id_string() -> String {
    const RANDOM_LEN: usize = 12;

    let mut rng = rand::rng();
    let random_chars: String = (0..RANDOM_LEN)
        .map(|_| {
            const CHARSET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();

    format!(
        "{}{}",
        peer_manager::superseedr_peer_id_prefix(),
        random_chars
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::clear_shared_config_state_for_tests;
    use tempfile::tempdir;

    fn shared_env_guard() -> &'static std::sync::Mutex<()> {
        crate::config::shared_env_guard_for_tests()
    }

    struct EnvVarRestore {
        key: &'static str,
        value: Option<std::ffi::OsString>,
    }

    impl EnvVarRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                value: std::env::var_os(key),
            }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            match &self.value {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    struct AppPathsRestore;

    impl Drop for AppPathsRestore {
        fn drop(&mut self) {
            crate::config::set_app_paths_override_for_tests(None);
            clear_shared_config_state_for_tests();
        }
    }

    fn set_test_app_paths(root: &Path) -> AppPathsRestore {
        crate::config::set_app_paths_override_for_tests(Some((
            root.join("config"),
            root.join("data"),
        )));
        AppPathsRestore
    }

    fn startup_test_interface(
        identity: &str,
        ipv4_addresses: Vec<std::net::Ipv4Addr>,
        ipv6_addresses: Vec<std::net::Ipv6Addr>,
    ) -> NetworkInterfaceInfo {
        NetworkInterfaceInfo {
            identity: identity.to_string(),
            display_name: identity.to_string(),
            ipv4_index: (!ipv4_addresses.is_empty()).then_some(7),
            ipv6_index: (!ipv6_addresses.is_empty()).then_some(7),
            is_up: true,
            is_loopback: false,
            ipv4_addresses,
            ipv6_addresses,
        }
    }

    #[test]
    fn startup_network_interface_override_selects_the_available_family() {
        let mut settings = Settings::default();
        settings.network_binding.ipv4_address = Some(std::net::Ipv4Addr::new(198, 51, 100, 10));
        settings.network_binding.ipv6_address = Some("2001:db8::10".parse().unwrap());
        let interfaces = [startup_test_interface(
            "interface-test0",
            vec![std::net::Ipv4Addr::new(192, 0, 2, 10)],
            Vec::new(),
        )];

        apply_startup_network_interface_from(&mut settings, "interface-test0", &interfaces)
            .expect("apply startup interface");

        assert_eq!(settings.network_binding.mode, NetworkBindingMode::Interface);
        assert_eq!(
            settings.network_binding.interface.as_deref(),
            Some("interface-test0")
        );
        assert!(settings.network_binding.enable_ipv4);
        assert!(!settings.network_binding.enable_ipv6);
        assert!(settings.network_binding.ipv4_address.is_none());
        assert!(settings.network_binding.ipv6_address.is_none());
    }

    #[test]
    fn startup_network_interface_override_retains_missing_interface_fail_closed_policy() {
        let mut settings = Settings::default();

        apply_startup_network_interface_from(&mut settings, "interface-missing0", &[])
            .expect("stage missing startup interface");

        assert_eq!(settings.network_binding.mode, NetworkBindingMode::Interface);
        assert_eq!(
            settings.network_binding.interface.as_deref(),
            Some("interface-missing0")
        );
        assert!(settings.network_binding.enable_ipv4);
        assert!(settings.network_binding.enable_ipv6);
    }

    #[test]
    fn startup_network_interface_override_rejects_an_empty_identity() {
        let mut settings = Settings::default();

        let error = apply_startup_network_interface_from(&mut settings, "  ", &[])
            .expect_err("empty startup interface must fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(settings.network_binding.mode, NetworkBindingMode::Any);
        assert!(settings.network_binding.interface.is_none());
    }

    #[test]
    fn persisted_network_interface_enables_discovered_families_and_clears_exact_sources() {
        let mut settings = Settings::default();
        settings.network_binding.ipv4_address = Some(std::net::Ipv4Addr::new(198, 51, 100, 10));
        settings.network_binding.ipv6_address = Some("2001:db8::10".parse().unwrap());
        let interfaces = [startup_test_interface(
            "interface-test0",
            vec![std::net::Ipv4Addr::new(192, 0, 2, 10)],
            vec!["2001:db8::20".parse().unwrap()],
        )];

        apply_persisted_network_interface(&mut settings, "interface-test0", &interfaces)
            .expect("apply persisted interface");

        assert_eq!(settings.network_binding.mode, NetworkBindingMode::Interface);
        assert_eq!(
            settings.network_binding.interface.as_deref(),
            Some("interface-test0")
        );
        assert!(settings.network_binding.enable_ipv4);
        assert!(settings.network_binding.enable_ipv6);
        assert!(settings.network_binding.ipv4_address.is_none());
        assert!(settings.network_binding.ipv6_address.is_none());
    }

    #[test]
    fn persisted_network_interface_rejects_an_unavailable_interface_without_changes() {
        let mut settings = Settings::default();
        let previous = settings.network_binding.clone();

        let error = apply_persisted_network_interface(&mut settings, "interface-missing0", &[])
            .expect_err("unavailable persistent interface must fail");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(settings.network_binding, previous);
    }

    #[test]
    fn persisted_network_interface_does_not_widen_supported_family_policy() {
        let mut settings = Settings::default();
        settings.network_binding.enable_ipv6 = false;
        let interfaces = [startup_test_interface(
            "interface-test0",
            vec![std::net::Ipv4Addr::new(192, 0, 2, 10)],
            vec!["2001:db8::20".parse().unwrap()],
        )];

        apply_persisted_network_interface(&mut settings, "interface-test0", &interfaces)
            .expect("apply persisted interface");

        assert!(settings.network_binding.enable_ipv4);
        assert!(!settings.network_binding.enable_ipv6);
    }

    #[test]
    fn persisted_network_interface_rejects_incompatible_bound_dns_without_changes() {
        let mut settings = Settings::default();
        settings.network_binding.dns_policy = DnsPolicy::Bound;
        settings.network_binding.dns_servers = vec!["[2001:db8::53]:53".parse().unwrap()];
        let previous = settings.network_binding.clone();
        let interfaces = [startup_test_interface(
            "interface-test0",
            vec![std::net::Ipv4Addr::new(192, 0, 2, 10)],
            Vec::new(),
        )];

        let error =
            apply_persisted_network_interface(&mut settings, "interface-test0", &interfaces)
                .expect_err("IPv4-only interface must reject IPv6-only bound DNS");

        assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
        assert!(error.to_string().contains("incompatible with bound DNS"));
        assert_eq!(settings.network_binding, previous);
    }

    #[test]
    fn persisted_network_interface_repairs_a_binding_with_both_families_disabled() {
        let mut settings = Settings::default();
        settings.network_binding.enable_ipv4 = false;
        settings.network_binding.enable_ipv6 = false;
        let interfaces = [startup_test_interface(
            "interface-test0",
            vec![std::net::Ipv4Addr::new(192, 0, 2, 10)],
            vec!["2001:db8::20".parse().unwrap()],
        )];

        apply_persisted_network_interface(&mut settings, "interface-test0", &interfaces)
            .expect("repair persisted interface binding");

        assert!(settings.network_binding.enable_ipv4);
        assert!(settings.network_binding.enable_ipv6);
    }

    #[test]
    fn main_tests_use_the_process_wide_shared_environment_guard() {
        assert!(std::ptr::eq(
            shared_env_guard(),
            crate::config::shared_env_guard_for_tests()
        ));
    }

    fn assert_abs_opt(path: &Option<PathBuf>, label: &str) {
        let path = path
            .as_ref()
            .unwrap_or_else(|| panic!("{label} should be available"));
        assert!(path.is_absolute(), "{label} should be absolute: {path:?}");
    }

    fn sample_settings() -> Settings {
        Settings {
            torrents: vec![config::TorrentSettings {
                torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                    .to_string(),
                name: "Sample Alpha".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn write_sample_torrent_file() -> (tempfile::TempDir, String) {
        let dir = tempdir().expect("create tempdir");
        let torrent = crate::torrent_file::Torrent {
            info: crate::torrent_file::Info {
                name: "sample-pack".to_string(),
                piece_length: 16_384,
                pieces: vec![0; 20],
                files: vec![
                    crate::torrent_file::InfoFile {
                        length: 10,
                        path: vec!["folder".to_string(), "alpha.bin".to_string()],
                        md5sum: None,
                        attr: None,
                    },
                    crate::torrent_file::InfoFile {
                        length: 20,
                        path: vec!["folder".to_string(), "beta.bin".to_string()],
                        md5sum: None,
                        attr: None,
                    },
                ],
                ..Default::default()
            },
            announce: Some("http://tracker.test".to_string()),
            ..Default::default()
        };
        let bytes = serde_bencode::to_bytes(&torrent).expect("serialize torrent");
        let path = dir
            .path()
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.torrent");
        fs::write(&path, bytes).expect("write torrent fixture");
        (dir, path.to_string_lossy().to_string())
    }

    fn write_recovery_torrent_file(file_name: &str) -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempdir().expect("create tempdir");
        let torrent = crate::torrent_file::Torrent {
            info: crate::torrent_file::Info {
                name: "sample-recovery-pack".to_string(),
                piece_length: 16_384,
                pieces: vec![1; 20],
                files: vec![crate::torrent_file::InfoFile {
                    length: 12,
                    path: vec!["payload".to_string(), "item.bin".to_string()],
                    md5sum: None,
                    attr: None,
                }],
                ..Default::default()
            },
            announce: Some("http://tracker.test".to_string()),
            ..Default::default()
        };
        let bytes = serde_bencode::to_bytes(&torrent).expect("serialize recovery torrent");
        let info_hash_hex =
            hex::encode(info_hash_from_torrent_bytes(&bytes).expect("recovery torrent info hash"));
        let path = dir.path().join(file_name);
        fs::write(&path, bytes).expect("write recovery torrent fixture");
        (dir, path, info_hash_hex)
    }

    fn ingest_added_entry(
        info_hash_hex: String,
        source_path: PathBuf,
        ingest_kind: IngestKind,
    ) -> EventJournalEntry {
        EventJournalEntry {
            id: 1,
            ts_iso: "2026-01-01T00:00:00Z".to_string(),
            category: EventCategory::Ingest,
            event_type: EventType::IngestAdded,
            info_hash_hex: Some(info_hash_hex),
            source_path: Some(source_path),
            details: EventDetails::Ingest {
                origin: crate::persistence::event_journal::IngestOrigin::WatchFolder,
                ingest_kind,
                download_path: None,
                container_name: None,
                payload_path: None,
            },
            ..Default::default()
        }
    }

    #[test]
    fn offline_pause_updates_torrent_control_state() {
        let mut settings = sample_settings();
        let request = ControlRequest::Pause {
            info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
        };

        let result = apply_offline_control_request(&mut settings, &request);

        assert!(result.is_ok());
        assert_eq!(
            settings.torrents[0].torrent_control_state,
            app::TorrentControlState::Paused
        );
    }

    #[test]
    fn already_running_message_matches_terminal_text() {
        assert_eq!(already_running_message(), "superseedr is already running.");
    }

    #[test]
    fn final_logging_fallback_message_reports_emergency_stderr_path() {
        assert_eq!(
            final_logging_fallback_message(true),
            "File logging is unavailable; using stderr fallback for diagnostics."
        );
        assert_eq!(
            final_logging_fallback_message(false),
            "No file logging directories were available; using stderr fallback for diagnostics."
        );
    }

    #[test]
    fn fallback_tracing_mode_keeps_non_cli_fallback_off_stderr() {
        assert_eq!(fallback_tracing_mode(false), FallbackTracingMode::Sink);
        assert_eq!(fallback_tracing_mode(true), FallbackTracingMode::Stderr);
    }

    #[test]
    fn logging_setup_warning_is_collected_when_stderr_is_suppressed() {
        let mut suppressed_warnings = Vec::new();

        report_logging_setup_warning(
            "Failed to initialize file logging at /tmp/demo: denied".to_string(),
            false,
            &mut suppressed_warnings,
        );

        assert_eq!(
            suppressed_warnings,
            vec!["Failed to initialize file logging at /tmp/demo: denied"]
        );
    }

    #[test]
    fn tui_logging_setup_warning_message_reports_suppressed_warnings() {
        let init = TracingInit {
            guards: Vec::new(),
            setup_warnings: vec![
                "Failed to initialize file logging at /tmp/demo: denied".to_string()
            ],
            attempted_file_logging: true,
        };

        let message = tui_logging_setup_warning_message(&init).expect("warning message");

        assert!(message.contains("File logging is unavailable"));
        assert!(message.contains("Failed to initialize file logging at /tmp/demo: denied"));
        assert!(tui_logging_setup_warning_message(&TracingInit {
            guards: Vec::new(),
            setup_warnings: Vec::new(),
            attempted_file_logging: true,
        })
        .is_none());
    }

    #[test]
    #[cfg(all(feature = "dht", feature = "pex"))]
    fn private_client_leak_guard_message_includes_recovery_steps() {
        let message = private_client_leak_guard_message("/tmp/config.toml");

        assert!(message.contains("!!!ERROR: POTENTIAL LEAK!!!"));
        assert!(message.contains("cargo install superseedr --no-default-features"));
        assert!(message.contains("/tmp/config.toml"));
        assert!(message.contains("private_client = true"));
    }

    #[test]
    fn offline_delete_removes_matching_torrent() {
        let mut settings = sample_settings();
        let request = ControlRequest::Delete {
            info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
            delete_files: false,
        };

        let result = apply_offline_control_request(&mut settings, &request);

        assert!(result.is_ok());
        assert!(settings.torrents.is_empty());
    }

    #[test]
    fn offline_move_rejects_magnet_without_persisted_metadata() {
        let _guard = shared_env_guard().lock().unwrap();
        let app_paths = tempdir().expect("create app paths");
        let _restore = set_test_app_paths(app_paths.path());
        let destination = tempdir().expect("create destination");
        let mut settings = sample_settings();
        settings.torrents[0].container_name = Some("sample-container".to_string());
        settings.torrents[0]
            .file_priorities
            .insert(1, app::FilePriority::High);
        let request = ControlRequest::MoveTorrent {
            info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
            download_path: destination.path().to_path_buf(),
        };

        let result = apply_offline_control_request(&mut settings, &request);

        let error = result.expect_err("missing manifest should reject move");
        assert!(error.contains("does not have persisted file metadata"));
        assert!(settings.torrents[0].download_path.is_none());
        assert_eq!(
            settings.torrents[0].container_name.as_deref(),
            Some("sample-container")
        );
        assert_eq!(
            settings.torrents[0].file_priorities.get(&1),
            Some(&app::FilePriority::High)
        );
    }

    #[test]
    fn move_request_builder_rejects_unknown_hash() {
        let destination = tempdir().expect("create destination");
        let settings = sample_settings();

        let error = build_move_torrent_request(
            &settings,
            "2222222222222222222222222222222222222222",
            destination.path(),
        )
        .expect_err("unknown torrent should fail");

        assert!(error.contains("was not found"));
    }

    #[test]
    fn move_request_builder_rejects_missing_destination() {
        let destination = tempdir().expect("create destination");
        let settings = sample_settings();
        let missing_path = destination.path().join("missing");

        let error = build_move_torrent_request(
            &settings,
            "1111111111111111111111111111111111111111",
            &missing_path,
        )
        .expect_err("missing destination should fail");

        assert!(error.contains("does not exist"));
    }

    #[test]
    fn move_command_rejects_a_running_client_before_touching_payloads() {
        let destination = tempdir().expect("create destination");
        let cli = Cli {
            json: false,
            network_interface: None,
            input: None,
            command: Some(Commands::Move {
                info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
                path: destination.path().to_path_buf(),
            }),
        };

        let error = process_cli_request(&cli, &Settings::default(), false, true, OutputMode::Text)
            .expect_err("running client should reject move");

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("Stop the running"));
    }

    #[test]
    fn offline_move_commits_staged_payload_files_and_checks_metadata() {
        let (_torrent_dir, torrent_path) = write_sample_torrent_file();
        let source_root = tempdir().expect("create source root");
        let destination_root = tempdir().expect("create destination root");
        let source_payload_dir = source_root.path().join("sample-container").join("folder");
        fs::create_dir_all(&source_payload_dir).expect("create source payload dir");
        let source_alpha = source_payload_dir.join("alpha.bin");
        let source_beta = source_payload_dir.join("beta.bin");
        fs::write(&source_alpha, b"alpha-data").expect("write alpha payload");
        fs::write(&source_beta, b"beta-data").expect("write beta payload");
        let info_hash_hex = hex::encode(
            info_hash_from_torrent_source(&torrent_path).expect("sample torrent info hash"),
        );
        let mut settings = Settings {
            torrents: vec![config::TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Alpha".to_string(),
                download_path: Some(source_root.path().to_path_buf()),
                container_name: Some("sample-container".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = ControlRequest::MoveTorrent {
            info_hash_hex,
            download_path: destination_root.path().to_path_buf(),
        };

        apply_offline_control_request(&mut settings, &request).expect("move torrent payload");

        let destination_payload_dir = destination_root
            .path()
            .join("sample-container")
            .join("folder");
        assert_eq!(
            fs::read(destination_payload_dir.join("alpha.bin")).expect("read moved alpha"),
            b"alpha-data"
        );
        assert_eq!(
            fs::read(destination_payload_dir.join("beta.bin")).expect("read moved beta"),
            b"beta-data"
        );
        assert!(!source_alpha.exists());
        assert!(!source_beta.exists());
        assert_eq!(
            settings.torrents[0].download_path.as_deref(),
            Some(
                fs::canonicalize(destination_root.path())
                    .expect("canonical destination")
                    .as_path()
            )
        );
    }

    #[test]
    fn offline_move_stages_destination_before_saving_and_then_removes_source() {
        let (_torrent_dir, torrent_path) = write_sample_torrent_file();
        let source_root = tempdir().expect("create source root");
        let destination_root = tempdir().expect("create destination root");
        let source_file = source_root
            .path()
            .join("sample-container")
            .join("folder")
            .join("alpha.bin");
        fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source payload dir");
        fs::write(&source_file, b"alpha-data").expect("write source payload");
        let destination_file = destination_root
            .path()
            .join("sample-container")
            .join("folder")
            .join("alpha.bin");
        let info_hash_hex = hex::encode(
            info_hash_from_torrent_source(&torrent_path).expect("sample torrent info hash"),
        );
        let mut settings = Settings {
            torrents: vec![config::TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Alpha".to_string(),
                download_path: Some(source_root.path().to_path_buf()),
                container_name: Some("sample-container".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = ControlRequest::MoveTorrent {
            info_hash_hex,
            download_path: destination_root.path().to_path_buf(),
        };
        let mut save_observed = false;

        apply_offline_control_request_mut_with_save(&mut settings, &request, |next_settings| {
            save_observed = true;
            assert!(
                source_file.exists(),
                "source must remain until catalog save"
            );
            assert_eq!(
                fs::read(&destination_file).expect("read staged destination"),
                b"alpha-data"
            );
            assert_eq!(
                next_settings.torrents[0].download_path.as_deref(),
                Some(
                    fs::canonicalize(destination_root.path())
                        .expect("canonical destination")
                        .as_path()
                )
            );
            Ok(())
        })
        .expect("commit offline move");

        assert!(save_observed);
        assert!(!source_file.exists());
        assert_eq!(
            fs::read(destination_file).expect("read committed destination"),
            b"alpha-data"
        );
    }

    #[test]
    fn offline_move_save_failure_retains_both_payload_locations_for_recovery() {
        let (_torrent_dir, torrent_path) = write_sample_torrent_file();
        let source_root = tempdir().expect("create source root");
        let destination_root = tempdir().expect("create destination root");
        let source_file = source_root
            .path()
            .join("sample-container")
            .join("folder")
            .join("alpha.bin");
        fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source payload dir");
        fs::write(&source_file, b"alpha-data").expect("write source payload");
        let destination_file = destination_root
            .path()
            .join("sample-container")
            .join("folder")
            .join("alpha.bin");
        let info_hash_hex = hex::encode(
            info_hash_from_torrent_source(&torrent_path).expect("sample torrent info hash"),
        );
        let original_download_path = source_root.path().to_path_buf();
        let mut settings = Settings {
            torrents: vec![config::TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Alpha".to_string(),
                download_path: Some(original_download_path.clone()),
                container_name: Some("sample-container".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = ControlRequest::MoveTorrent {
            info_hash_hex,
            download_path: destination_root.path().to_path_buf(),
        };

        let error = apply_offline_control_request_mut_with_save(
            &mut settings,
            &request,
            |_next_settings| {
                assert!(
                    source_file.exists(),
                    "source must remain during catalog save"
                );
                assert!(destination_file.exists(), "destination must be staged");
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected settings save failure",
                ))
            },
        )
        .expect_err("save failure should reject move");

        assert!(error.contains("injected settings save failure"));
        assert!(error.contains("Both source and staged destination files were retained"));
        assert_eq!(
            fs::read(&source_file).expect("read retained source"),
            b"alpha-data"
        );
        assert_eq!(
            fs::read(&destination_file).expect("read retained destination"),
            b"alpha-data"
        );
        assert_eq!(
            settings.torrents[0].download_path.as_deref(),
            Some(original_download_path.as_path())
        );
    }

    #[test]
    fn offline_move_rejects_conflicting_destination_without_deleting_source() {
        let (_torrent_dir, torrent_path) = write_sample_torrent_file();
        let source_root = tempdir().expect("create source root");
        let destination_root = tempdir().expect("create destination root");
        let source_payload_dir = source_root.path().join("sample-container").join("folder");
        let destination_payload_dir = destination_root
            .path()
            .join("sample-container")
            .join("folder");
        fs::create_dir_all(&source_payload_dir).expect("create source payload dir");
        fs::create_dir_all(&destination_payload_dir).expect("create destination payload dir");
        let source_alpha = source_payload_dir.join("alpha.bin");
        fs::write(&source_alpha, b"alpha-data").expect("write alpha payload");
        fs::write(destination_payload_dir.join("alpha.bin"), b"different-data")
            .expect("write conflicting destination");
        let info_hash_hex = hex::encode(
            info_hash_from_torrent_source(&torrent_path).expect("sample torrent info hash"),
        );
        let mut settings = Settings {
            torrents: vec![config::TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Alpha".to_string(),
                download_path: Some(source_root.path().to_path_buf()),
                container_name: Some("sample-container".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = ControlRequest::MoveTorrent {
            info_hash_hex,
            download_path: destination_root.path().to_path_buf(),
        };

        let error = apply_offline_control_request(&mut settings, &request)
            .expect_err("conflicting destination should fail");

        assert!(error.contains("already exists"));
        assert_eq!(
            fs::read(&source_alpha).expect("source remains"),
            b"alpha-data"
        );
        assert_eq!(
            settings.torrents[0].download_path.as_deref(),
            Some(source_root.path())
        );
    }

    #[test]
    fn catalog_recovery_validates_torrent_file_contents_for_normal_filename() {
        let (_dir, torrent_path, info_hash_hex) =
            write_recovery_torrent_file("manual-input.torrent");
        let journal = EventJournalState {
            next_id: 2,
            entries: vec![ingest_added_entry(
                info_hash_hex.clone(),
                torrent_path,
                IngestKind::TorrentFile,
            )],
        };

        let candidates = analyze_catalog_recovery(&Settings::default(), &journal);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].info_hash_hex, info_hash_hex);
        assert_eq!(candidates[0].status, CatalogRecoveryStatus::Recoverable);
    }

    #[test]
    fn catalog_recovery_validates_path_file_by_referenced_torrent_contents() {
        let (dir, torrent_path, info_hash_hex) = write_recovery_torrent_file("payload.torrent");
        let path_file = dir.path().join("manual-input.path");
        fs::write(&path_file, torrent_path.to_string_lossy().as_bytes())
            .expect("write path fixture");
        let journal = EventJournalState {
            next_id: 2,
            entries: vec![ingest_added_entry(
                info_hash_hex.clone(),
                path_file,
                IngestKind::PathFile,
            )],
        };

        let candidates = analyze_catalog_recovery(&Settings::default(), &journal);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].info_hash_hex, info_hash_hex);
        assert_eq!(candidates[0].status, CatalogRecoveryStatus::Recoverable);
    }

    #[test]
    fn offline_resume_updates_torrent_control_state() {
        let mut settings = sample_settings();
        settings.torrents[0].torrent_control_state = app::TorrentControlState::Paused;
        let request = ControlRequest::Resume {
            info_hash_hex: "1111111111111111111111111111111111111111".to_string(),
        };

        let result = apply_offline_control_request(&mut settings, &request);

        assert!(result.is_ok());
        assert_eq!(
            settings.torrents[0].torrent_control_state,
            app::TorrentControlState::Running
        );
    }

    #[test]
    fn offline_priority_updates_file_priority_by_index() {
        let (_dir, torrent_path) = write_sample_torrent_file();
        let mut settings = Settings {
            torrents: vec![config::TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Pack".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = ControlRequest::SetFilePriority {
            info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            target: ControlPriorityTarget::FileIndex(1),
            priority: app::FilePriority::High,
        };

        let result = apply_offline_control_request(&mut settings, &request);

        assert!(result.is_ok());
        assert_eq!(
            settings.torrents[0].file_priorities.get(&1),
            Some(&app::FilePriority::High)
        );
    }

    #[test]
    fn offline_priority_updates_file_priority_by_relative_path() {
        let (_dir, torrent_path) = write_sample_torrent_file();
        let mut settings = Settings {
            torrents: vec![config::TorrentSettings {
                torrent_or_magnet: torrent_path,
                name: "Sample Pack".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let request = ControlRequest::SetFilePriority {
            info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            target: ControlPriorityTarget::FilePath("folder/beta.bin".to_string()),
            priority: app::FilePriority::Skip,
        };

        let result = apply_offline_control_request(&mut settings, &request);

        assert!(result.is_ok());
        assert_eq!(
            settings.torrents[0].file_priorities.get(&1),
            Some(&app::FilePriority::Skip)
        );
    }

    #[test]
    fn shared_mode_without_running_leader_mutates_shared_settings_offline() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempdir().expect("create tempdir");
        let shared_root = dir.path().join("shared-root");
        std::fs::create_dir_all(&shared_root).expect("create shared root");
        let previous_shared_dir = std::env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let previous_host_id = std::env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", &shared_root);
        std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", "host-a");
        clear_shared_config_state_for_tests();

        let mut settings = crate::config::load_settings().expect("load shared settings");
        settings.torrents.push(crate::config::TorrentSettings {
            torrent_or_magnet: "magnet:?xt=urn:btih:1111111111111111111111111111111111111111"
                .to_string(),
            name: "Sample Alpha".to_string(),
            ..Default::default()
        });
        crate::config::save_settings(&settings).expect("save shared settings");

        let loaded = crate::config::load_settings().expect("reload shared settings");
        let cli = Cli {
            json: false,
            network_interface: None,
            input: None,
            command: Some(Commands::Pause {
                targets: vec!["1111111111111111111111111111111111111111".to_string()],
            }),
        };

        process_cli_request(&cli, &loaded, true, false, OutputMode::Text)
            .expect("shared offline pause");

        let reloaded = crate::config::load_settings().expect("reload paused shared settings");
        assert_eq!(
            reloaded.torrents[0].torrent_control_state,
            app::TorrentControlState::Paused
        );

        let inbox = crate::config::shared_inbox_path().expect("shared inbox path");
        let inbox_entries = std::fs::read_dir(inbox)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(
            inbox_entries, 0,
            "offline shared mutation should not queue inbox files"
        );

        let host_journal_path = crate::persistence::event_journal::event_journal_state_file_path()
            .expect("host journal path");
        let host_journal_raw =
            std::fs::read_to_string(&host_journal_path).expect("read host journal");
        let host_journal_state: crate::persistence::event_journal::EventJournalState =
            toml::from_str(&host_journal_raw).expect("parse host journal");
        assert!(host_journal_state.entries.iter().any(|entry| {
            entry.scope == EventScope::Host
                && entry.category == EventCategory::Control
                && entry.event_type == EventType::ControlApplied
        }));

        let shared_journal_path =
            crate::persistence::event_journal::shared_event_journal_state_file_path()
                .expect("shared journal path");
        let shared_journal_raw = std::fs::read_to_string(&shared_journal_path).unwrap_or_default();
        let shared_journal_state = if shared_journal_raw.trim().is_empty() {
            crate::persistence::event_journal::EventJournalState::default()
        } else {
            toml::from_str(&shared_journal_raw).expect("parse shared journal")
        };
        assert!(
            shared_journal_state.entries.is_empty(),
            "offline shared mutation should not write shared journal entries"
        );

        if let Some(value) = previous_shared_dir {
            std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = previous_host_id {
            std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[test]
    fn shared_offline_multi_add_carries_forward_previous_adds() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempdir().expect("create tempdir");
        let shared_root = std::fs::canonicalize(dir.path()).expect("canonical shared root");
        let download_path = shared_root.join("downloads");
        std::fs::create_dir_all(&download_path).expect("create downloads");
        let _shared_dir_restore = EnvVarRestore::capture("SUPERSEEDR_SHARED_CONFIG_DIR");
        let _host_id_restore = EnvVarRestore::capture("SUPERSEEDR_SHARED_HOST_ID");

        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", &shared_root);
        std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", "host-a");
        clear_shared_config_state_for_tests();

        let settings = crate::config::load_settings().expect("initialize shared settings");
        let input = concat!(
            "magnet:?xt=urn:btih:1111111111111111111111111111111111111111&dn=linux-alpha.iso&xl=100",
            ",magnet:?xt=urn:btih:2222222222222222222222222222222222222222&dn=linux-beta.iso&xl=200",
            ",magnet:?xt=urn:btih:3333333333333333333333333333333333333333&dn=linux-gamma.iso&xl=300"
        )
        .to_string();
        let cli = Cli {
            json: false,
            network_interface: None,
            input: None,
            command: Some(Commands::Add {
                validated: true,
                path: Some(download_path.clone()),
                inputs: vec![input],
            }),
        };

        process_cli_request(&cli, &settings, true, false, OutputMode::Text)
            .expect("shared offline multi add");

        let reloaded = crate::config::load_settings().expect("reload shared settings");
        assert_eq!(reloaded.torrents.len(), 3);
        assert!(reloaded
            .torrents
            .iter()
            .all(|torrent| torrent.validation_status));
        assert!(reloaded
            .torrents
            .iter()
            .all(|torrent| torrent.download_path.as_deref() == Some(download_path.as_path())));

        let metadata = crate::config::load_torrent_metadata().expect("load metadata");
        assert_eq!(metadata.torrents.len(), 3);
        assert_eq!(
            metadata.torrents[0].files[0].relative_path,
            "linux-alpha.iso"
        );
        assert_eq!(
            metadata.torrents[1].files[0].relative_path,
            "linux-beta.iso"
        );
        assert_eq!(
            metadata.torrents[2].files[0].relative_path,
            "linux-gamma.iso"
        );

        clear_shared_config_state_for_tests();
    }

    #[test]
    fn validate_add_download_path_rejects_regular_file() {
        let dir = tempdir().expect("create tempdir");
        let file_path = dir.path().join("not-a-directory");
        std::fs::write(&file_path, b"content").expect("write file");

        let error = validate_add_download_path(&file_path).expect_err("regular file should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("must be a directory"));
    }

    #[test]
    fn shared_control_torrent_file_add_stages_source_path() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempdir().expect("create tempdir");
        let shared_mount = dir.path().join("shared");
        std::fs::create_dir_all(&shared_mount).expect("create shared mount");
        let source_path = dir.path().join("input.torrent");
        std::fs::write(&source_path, b"torrent bytes").expect("write source torrent");
        let _shared_dir_restore = EnvVarRestore::capture("SUPERSEEDR_SHARED_CONFIG_DIR");
        let _host_id_restore = EnvVarRestore::capture("SUPERSEEDR_SHARED_HOST_ID");

        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", &shared_mount);
        std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", "host-a");
        clear_shared_config_state_for_tests();

        let request =
            add_control_request_for_input(source_path.to_string_lossy().as_ref(), None, true, true)
                .expect("build shared add request");

        match request {
            ControlRequest::AddTorrentFile { source_path, .. } => {
                let shared_config_root = crate::config::shared_root_path()
                    .expect("shared config root should be available");
                assert!(source_path.starts_with(shared_config_root.join("staged-adds")));
                assert_eq!(
                    std::fs::read(&source_path).expect("read staged torrent"),
                    b"torrent bytes"
                );
            }
            other => panic!("unexpected request: {other:?}"),
        }

        clear_shared_config_state_for_tests();
    }

    #[test]
    fn optional_path_json_serializes_path_or_null() {
        assert_eq!(
            optional_path_json(Some(PathBuf::from("C:\\sample\\sidecar.toml"))),
            json!("C:\\sample\\sidecar.toml")
        );
        assert_eq!(optional_path_json(None), Value::Null);
    }

    #[test]
    fn show_configs_standalone_resolves_absolute_paths() {
        let _guard = shared_env_guard().lock().unwrap();
        let _shared_dir_restore = EnvVarRestore::capture("SUPERSEEDR_SHARED_CONFIG_DIR");
        let temp = tempdir().expect("create tempdir");
        let _app_paths_restore = set_test_app_paths(temp.path());
        std::env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        clear_shared_config_state_for_tests();

        let settings = Settings {
            watch_folder: Some(PathBuf::from("relative-watch")),
            default_download_folder: Some(PathBuf::from("relative-downloads")),
            ..Settings::default()
        };

        let snapshot =
            build_show_configs_snapshot(Some(&settings), None).expect("build path snapshot");

        assert!(!snapshot.shared_mode);
        assert!(snapshot.shared.is_none());
        assert_abs_opt(&snapshot.local.config_dir, "local config dir");
        assert_abs_opt(&snapshot.local.settings_path, "local settings path");
        assert_abs_opt(
            &snapshot.local.torrent_metadata_path,
            "local torrent metadata path",
        );
        assert_abs_opt(&snapshot.effective.status_file, "effective status file");
        assert_abs_opt(
            &snapshot.effective.command_watch_dir,
            "effective command watch dir",
        );
        assert_abs_opt(&snapshot.settings.watch_folder, "settings watch folder");
        assert_abs_opt(
            &snapshot.settings.default_download_folder,
            "settings default download folder",
        );
        assert!(snapshot
            .effective
            .runtime_watch_dirs
            .iter()
            .all(|path| path.is_absolute()));
        assert!(snapshot.descriptions.iter().any(|entry| {
            entry.section == "effective"
                && entry.key == "status_file"
                && entry.description.contains("Status snapshot")
        }));

        let default_output = show_configs_json_data(&snapshot, false);
        assert!(default_output.get("effective").is_some());
        assert!(default_output.get("local").is_none());
        assert!(default_output.get("shared").is_none());
        assert!(default_output["descriptions"]
            .as_array()
            .expect("descriptions array")
            .iter()
            .all(|entry| entry["section"] != "local"));

        let all_output = show_configs_json_data(&snapshot, true);
        assert!(all_output.get("local").is_some());
    }

    #[test]
    fn show_configs_without_loaded_settings_keeps_path_report_available() {
        let _guard = shared_env_guard().lock().unwrap();
        let _shared_dir_restore = EnvVarRestore::capture("SUPERSEEDR_SHARED_CONFIG_DIR");
        let temp = tempdir().expect("create tempdir");
        let _app_paths_restore = set_test_app_paths(temp.path());
        std::env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        clear_shared_config_state_for_tests();

        let snapshot = build_show_configs_snapshot(None, Some("settings failed".to_string()))
            .expect("build path snapshot without settings");

        assert!(!snapshot.shared_mode);
        assert_eq!(
            snapshot.settings_load_error.as_deref(),
            Some("settings failed")
        );
        assert_eq!(snapshot.settings.client_port, None);
        assert_eq!(snapshot.settings.output_status_interval, None);
        assert_abs_opt(&snapshot.local.settings_path, "local settings path");
        assert_abs_opt(
            &snapshot.effective.command_watch_dir,
            "effective command watch dir",
        );
        assert!(snapshot
            .effective
            .runtime_watch_dirs
            .iter()
            .all(|path| path.is_absolute()));
        let value = serde_json::to_value(&snapshot).expect("serialize snapshot");
        assert!(value["descriptions"]
            .as_array()
            .expect("descriptions array")
            .iter()
            .any(|entry| entry["section"] == "settings" && entry["key"] == "settings_load_error"));
    }

    #[test]
    fn show_configs_shared_mode_includes_shared_absolute_paths() {
        let _guard = shared_env_guard().lock().unwrap();
        let _shared_dir_restore = EnvVarRestore::capture("SUPERSEEDR_SHARED_CONFIG_DIR");
        let _host_id_restore = EnvVarRestore::capture("SUPERSEEDR_SHARED_HOST_ID");
        let _legacy_host_id_restore = EnvVarRestore::capture("SUPERSEEDR_HOST_ID");
        let temp = tempdir().expect("create tempdir");
        let _app_paths_restore = set_test_app_paths(temp.path());
        let shared_root = temp.path().join("shared-root");

        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", &shared_root);
        std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", "node-a");
        std::env::remove_var("SUPERSEEDR_HOST_ID");
        clear_shared_config_state_for_tests();

        let settings = Settings {
            watch_folder: Some(shared_root.join("watch-in")),
            default_download_folder: Some(shared_root.join("downloads")),
            ..Settings::default()
        };

        let snapshot =
            build_show_configs_snapshot(Some(&settings), None).expect("build shared path snapshot");
        let shared = snapshot.shared.as_ref().expect("shared paths");

        assert!(snapshot.shared_mode);
        assert_eq!(snapshot.host.host_id, "node-a");
        assert!(shared.mount_root.is_absolute());
        assert!(shared.config_root.is_absolute());
        assert_eq!(
            shared.config_root,
            std::path::absolute(shared_root.join("superseedr-config"))
                .expect("absolute shared config root")
        );
        assert_eq!(
            snapshot.effective.config_files.catalog_path,
            Some(shared.catalog_path.clone())
        );
        assert_eq!(
            snapshot.effective.config_files.host_config_path,
            Some(shared.host_config_path.clone())
        );
        assert_eq!(
            snapshot.effective.command_watch_dir,
            Some(shared.inbox_dir.clone())
        );
        assert_eq!(
            snapshot.effective.shared_event_journal_file,
            Some(shared.shared_event_journal_file.clone())
        );
        assert!(snapshot
            .effective
            .runtime_watch_dirs
            .iter()
            .all(|path| path.is_absolute()));
    }

    #[test]
    fn shared_status_follow_start_returns_error_for_non_stream_requests() {
        let error = process_shared_status_request(
            &Settings::default(),
            StatusCommandMode::SetInterval { interval_secs: 5 },
            true,
            OutputMode::Text,
        )
        .expect_err("shared status follow start should error");

        assert!(error
            .to_string()
            .contains("Shared mode leader status snapshots are always enabled every 5 seconds"));
    }

    #[test]
    fn shared_status_follow_stop_returns_error() {
        let error = process_shared_status_request(
            &Settings::default(),
            StatusCommandMode::Stop,
            true,
            OutputMode::Text,
        )
        .expect_err("shared status follow stop should error");

        assert!(error
            .to_string()
            .contains("Shared mode leader status snapshots are always enabled every 5 seconds"));
    }

    #[test]
    fn shared_status_now_uses_offline_snapshot_when_no_leader_is_running() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempdir().expect("create tempdir");
        let shared_root = dir.path().join("shared-root");
        std::fs::create_dir_all(&shared_root).expect("create shared root");
        let previous_shared_dir = std::env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let previous_host_id = std::env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", &shared_root);
        std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", "host-a");
        clear_shared_config_state_for_tests();

        let status_path = status_file_path().expect("shared status path");
        let status_parent = status_path.parent().expect("status parent");
        std::fs::create_dir_all(status_parent).expect("create status dir");
        std::fs::write(&status_path, "{not valid json").expect("write stale invalid status file");

        let result = process_shared_status_request(
            &Settings::default(),
            StatusCommandMode::Snapshot,
            false,
            OutputMode::Json,
        );

        assert!(
            result.is_ok(),
            "shared status should fall back to offline output"
        );

        if let Some(value) = previous_shared_dir {
            std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = previous_host_id {
            std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }

    #[test]
    fn shared_cli_acquires_shared_lock_when_no_leader_is_running() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempdir().expect("create tempdir");
        let shared_root = dir.path().join("shared-root");
        std::fs::create_dir_all(&shared_root).expect("create shared root");
        let previous_shared_dir = std::env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        let previous_host_id = std::env::var_os("SUPERSEEDR_SHARED_HOST_ID");

        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", &shared_root);
        std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", "host-a");
        clear_shared_config_state_for_tests();
        let shared_lock = shared_lock_path().expect("shared lock path");
        let shared_lock_parent = shared_lock.parent().expect("shared lock parent");
        std::fs::create_dir_all(shared_lock_parent).expect("create shared config root");

        let first = try_acquire_app_lock().expect("acquire shared cli lock");
        assert!(
            first.is_some(),
            "first shared cli lock attempt should succeed"
        );

        let second = try_acquire_app_lock().expect("second shared cli lock attempt");
        assert!(
            second.is_none(),
            "second shared cli lock attempt should observe an existing holder"
        );

        drop(first);

        if let Some(value) = previous_shared_dir {
            std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        if let Some(value) = previous_host_id {
            std::env::set_var("SUPERSEEDR_SHARED_HOST_ID", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_HOST_ID");
        }
        clear_shared_config_state_for_tests();
    }
}
