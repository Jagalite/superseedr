// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
mod command;
mod config;
mod control_service;
mod dht_service;
mod errors;
mod fs_atomic;
mod global_peer_manager;
mod integrations;
mod integrity_scheduler;
mod networking;
mod persistence;
mod resource_manager;
mod storage;
mod telemetry;
mod theme;
mod token_bucket;
mod torrent_file;
mod torrent_identity;
mod torrent_manager;
mod tracker;
mod tui;
mod tuning;
mod watch_inbox;

use app::{App, AppRuntimeMode};
use rand::Rng;

use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::Write;

use std::path::PathBuf;
use std::time::Duration;

use crate::config::Settings;
use crate::config::{
    clear_persisted_host_id, clear_persisted_shared_config, convert_shared_to_standalone,
    convert_standalone_to_shared, effective_host_id_selection, effective_shared_config_selection,
    is_shared_config_mode, load_settings, load_settings_for_cli, persisted_host_id_path,
    persisted_shared_config_path, resolve_command_watch_path, set_persisted_host_id,
    set_persisted_shared_config, shared_lock_path, HostIdSource, SharedConfigSource,
};
use crate::control_service::{
    apply_offline_control_request, apply_offline_purge, control_event_details, list_torrent_files,
    online_control_success_message, resolve_purge_target_info_hash, resolve_target_info_hash,
};
use crate::integrations::cli::{
    command_to_control_requests_with_resolver, expand_add_inputs, require_cli_targets,
    status_command_mode, status_control_request, status_file_modified_at,
    wait_for_status_json_after, write_control_command, write_input_command,
    write_path_command_payload, write_stop_command, Cli, Commands, StatusCommandMode,
};
#[cfg(test)]
use crate::integrations::control::ControlPriorityTarget;
use crate::integrations::control::ControlRequest;
use crate::integrations::status::{offline_output_json, status_file_path};
use crate::persistence::event_journal::{
    append_event_journal_entry, event_journal_json, load_event_journal_state,
    save_event_journal_state, ControlOrigin, EventCategory, EventJournalEntry, EventScope,
    EventType,
};
use crate::torrent_identity::info_hash_from_torrent_source;
use serde_json::{json, Value};

use tracing_appender::rolling::RollingFileAppender;
use tracing_appender::rolling::Rotation;

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

// CLI types and process_input moved to integrations::cli

fn init_tracing(
    log_dirs: Vec<PathBuf>,
    filename_prefix: &str,
    emit_stderr: bool,
) -> Vec<tracing_appender::non_blocking::WorkerGuard> {
    let quiet_filter = Targets::new()
        .with_default(DEFAULT_LOG_FILTER)
        .with_target("mainline::rpc::socket", LevelFilter::ERROR);

    if !emit_stderr {
        for log_dir in &log_dirs {
            if fs::create_dir_all(log_dir).is_err() {
                continue;
            }

            let log_path = log_dir.join(format!("{}.log", filename_prefix));
            let file = match OpenOptions::new().create(true).append(true).open(&log_path) {
                Ok(file) => file,
                Err(_) => continue,
            };

            let (non_blocking_general, guard_general) = tracing_appender::non_blocking(file);
            let general_layer = fmt::layer()
                .with_writer(non_blocking_general)
                .with_ansi(false)
                .with_filter(quiet_filter.clone());
            if tracing_subscriber::registry()
                .with(general_layer)
                .try_init()
                .is_ok()
            {
                return vec![guard_general];
            }
        }
    }

    for log_dir in log_dirs {
        if let Err(error) = fs::create_dir_all(&log_dir) {
            if emit_stderr {
                eprintln!(
                    "[Warn] Failed to create log directory at {}: {}",
                    log_dir.display(),
                    error
                );
            }
        } else {
            match RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .max_log_files(31)
                .filename_prefix(filename_prefix)
                .filename_suffix("log")
                .build(&log_dir)
            {
                Ok(general_log) => {
                    let (non_blocking_general, guard_general) =
                        tracing_appender::non_blocking(general_log);
                    let general_layer = fmt::layer()
                        .with_writer(non_blocking_general)
                        .with_ansi(false)
                        .with_filter(quiet_filter.clone());
                    if tracing_subscriber::registry()
                        .with(general_layer)
                        .try_init()
                        .is_ok()
                    {
                        return vec![guard_general];
                    }
                }
                Err(error) => {
                    if emit_stderr {
                        eprintln!(
                            "[Warn] Failed to initialize file logging at {}: {}",
                            log_dir.display(),
                            error
                        );
                    }
                }
            }
        }
    }

    let fallback_layer = if emit_stderr {
        fmt::layer().with_filter(quiet_filter).boxed()
    } else {
        fmt::layer()
            .with_writer(std::io::sink)
            .with_filter(quiet_filter)
            .boxed()
    };
    let _ = tracing_subscriber::registry()
        .with(fallback_layer)
        .try_init();

    Vec::new()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let output_mode = if cli.json {
        OutputMode::Json
    } else {
        OutputMode::Text
    };
    let has_cli_request = cli.input.is_some() || cli.command.is_some();
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
    let _tracing_guards = init_tracing(
        log_dirs,
        if has_cli_request { "cli" } else { "app" },
        has_cli_request,
    );

    tracing::info!("STARTING SUPERSEEDR");

    if let Some(result) = process_launcher_setup_command(&cli, output_mode) {
        if let Err(error) = result {
            if output_mode == OutputMode::Json {
                print_json_error(cli_command_name(cli.command.as_ref()), &error.to_string());
            } else {
                eprintln!("[Error] Application failed: {}", error);
            }
            std::process::exit(1);
        }
        tracing::info!("Launcher setup command processed, exiting temporary instance.");
        return Ok(());
    }

    let loaded_settings = match if has_cli_request {
        load_settings_for_cli()
    } else {
        load_settings()
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
            tracing::error!("Failed to create watch directories: {}", e);
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
        tracing::info!("Command processed, exiting temporary instance.");
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
        tracing::info!("superseedr is already running.");
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

            tracing::error!(
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
            tracing::info!("Setting private mode flag in configuration.");
            client_configs.private_client = true;
            if can_persist_startup_settings {
                if let Err(e) = config::save_settings(&client_configs) {
                    tracing::error!(
                        "Failed to save settings after setting private mode flag: {}",
                        e
                    );
                }
            }
        }
    }

    let port_file_path = PathBuf::from("/port-data/forwarded_port");
    tracing::info!("Checking for dynamic port file at {:?}", port_file_path);
    if let Ok(port_str) = fs::read_to_string(&port_file_path) {
        match port_str.trim().parse::<u16>() {
            Ok(dynamic_port) => {
                if dynamic_port > 0 {
                    tracing::info!(
                        "Successfully read dynamic port {}. Overriding settings.",
                        dynamic_port
                    );
                    client_configs.client_port = dynamic_port;
                } else {
                    tracing::warn!("Dynamic port file was empty or zero. Using config port.");
                }
            }
            Err(e) => {
                tracing::error!(
                    "Failed to parse port file content '{}': {}. Using config port.",
                    port_str,
                    e
                );
            }
        }
    } else {
        tracing::info!(
            "Dynamic file not found. Using port {} from settings.",
            client_configs.client_port
        );
    }

    if client_configs.client_id.is_empty() {
        client_configs.client_id = generate_client_id_string();
        if can_persist_startup_settings {
            if let Err(e) = config::save_settings(&client_configs) {
                tracing::error!("Failed to save settings after generating client ID: {}", e);
            }
        } else {
            tracing::info!("Generated in-memory client ID for shared follower startup.");
        }
    }

    tracing::info!("Initializing application state...");
    let mut app = App::new_with_lock(client_configs, runtime_mode, lock_file_handle).await?;
    tracing::info!("Application state initialized. Starting TUI.");

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

    if let Err(e) = app.run(&mut terminal).await {
        eprintln!("[Error] Application failed: {}", e);
    }

    cleanup_terminal()?;

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
        tracing::info!("Processing direct input: {}", direct_input);
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
        Commands::Add { inputs } => {
            let mut queued = Vec::new();
            for input in expand_add_inputs(inputs) {
                tracing::info!("Processing Add subcommand input: {}", input);
                let command_path = queue_direct_input_command(settings, &input)?;
                if output_mode == OutputMode::Text {
                    println!("Queued add command at {}", command_path.display());
                }
                queued.push(json!({
                    "input": input,
                    "command_path": command_path,
                }));
            }
            if output_mode == OutputMode::Json {
                print_success(
                    output_mode,
                    "add",
                    "Queued add command(s).",
                    json!({ "queued": queued }),
                );
            }
            Ok(())
        }
        Commands::Journal => {
            process_journal_command(output_mode)?;
            Ok(())
        }
        Commands::SetSharedConfig { path } => process_set_shared_config_command(path, output_mode),
        Commands::ClearSharedConfig => process_clear_shared_config_command(output_mode),
        Commands::ShowSharedConfig => process_show_shared_config_command(output_mode),
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
            tracing::info!("Processing StopClient command.");
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
            for target in resolved_targets {
                let info_hash_hex =
                    resolve_purge_target_info_hash(settings, &target).map_err(io::Error::other)?;
                let request = ControlRequest::Delete {
                    info_hash_hex,
                    delete_files: true,
                };

                if shared_mode && leader_is_running {
                    process_shared_control_request(
                        settings,
                        &request,
                        leader_is_running,
                        output_mode,
                    )?;
                } else if leader_is_running {
                    process_online_control_request(settings, &request, output_mode)?;
                } else {
                    process_offline_control_request(settings, &request, output_mode)?;
                }
            }
            Ok(())
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

            for request in requests {
                if shared_mode && leader_is_running {
                    process_shared_control_request(
                        settings,
                        &request,
                        leader_is_running,
                        output_mode,
                    )?;
                } else if leader_is_running {
                    process_online_control_request(settings, &request, output_mode)?;
                } else {
                    process_offline_control_request(settings, &request, output_mode)?;
                }
            }
            Ok(())
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

fn process_online_control_request(
    settings: &Settings,
    request: &ControlRequest,
    output_mode: OutputMode,
) -> io::Result<()> {
    let _ = queue_control_request_command(settings, request)?;
    print_success(
        output_mode,
        request.action_name(),
        &online_control_success_message(request),
        json!({ "queued": true, "request": request }),
    );
    Ok(())
}

fn process_shared_control_request(
    settings: &Settings,
    request: &ControlRequest,
    leader_is_running: bool,
    output_mode: OutputMode,
) -> io::Result<()> {
    let _ = queue_control_request_command(settings, request)?;
    print_queued_control_message(request, true, leader_is_running, output_mode);
    Ok(())
}

fn process_offline_control_request(
    settings: &Settings,
    request: &ControlRequest,
    output_mode: OutputMode,
) -> io::Result<()> {
    match request {
        ControlRequest::StatusNow => {
            let raw = offline_output_json(settings)?;
            return print_json_passthrough(output_mode, "status", &raw);
        }
        ControlRequest::StatusFollowStart { .. } | ControlRequest::StatusFollowStop => {
            return Err(io::Error::other(
                "Streaming status commands require a running superseedr instance",
            ));
        }
        _ => {}
    }

    let mut next_settings = settings.clone();
    let mut result = match request {
        ControlRequest::Delete {
            info_hash_hex,
            delete_files: true,
        } => apply_offline_purge(&mut next_settings, info_hash_hex),
        _ => apply_offline_control_request(&mut next_settings, request),
    };
    if result.is_ok() {
        if let Err(error) = config::save_settings(&next_settings) {
            result = Err(format!("Failed to save updated settings: {}", error));
        }
    }
    record_offline_control_journal_entry(request, &result);
    let message = result.map_err(io::Error::other)?;
    print_success(
        output_mode,
        request.action_name(),
        &message,
        json!({ "applied": true, "request": request, "message": message }),
    );
    Ok(())
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
                "{}\t{}\t{}\t{}",
                file.file_index,
                file.length,
                file.relative_path,
                file.full_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<unavailable>".to_string())
            );
        }
    }

    Ok(())
}

fn process_journal_command(output_mode: OutputMode) -> io::Result<()> {
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
                        "  {}\t{}\t{}\t{}",
                        file.file_index,
                        file.length,
                        file.relative_path,
                        file.full_path
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "<unavailable>".to_string())
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
        } => format!("ingest origin={origin:?} kind={ingest_kind:?}"),
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
        Some(Commands::Journal) => Some("journal"),
        Some(Commands::SetSharedConfig { .. }) => Some("set-shared-config"),
        Some(Commands::ClearSharedConfig) => Some("clear-shared-config"),
        Some(Commands::ShowSharedConfig) => Some("show-shared-config"),
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
        tracing::error!("Failed to save offline control journal entry: {}", error);
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
    const CLIENT_PREFIX: &str = "-SS1000-";
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

    format!("{}{}", CLIENT_PREFIX, random_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::clear_shared_config_state_for_tests;
    use tempfile::tempdir;

    fn shared_env_guard() -> &'static std::sync::Mutex<()> {
        static GUARD: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        GUARD.get_or_init(|| std::sync::Mutex::new(()))
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
    fn optional_path_json_serializes_path_or_null() {
        assert_eq!(
            optional_path_json(Some(PathBuf::from("C:\\sample\\sidecar.toml"))),
            json!("C:\\sample\\sidecar.toml")
        );
        assert_eq!(optional_path_json(None), Value::Null);
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
