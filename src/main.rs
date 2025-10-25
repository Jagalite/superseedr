// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
mod command;
mod config;
mod errors;
mod networking;
mod resource_manager;
mod storage;
mod theme;
mod token_bucket;
mod torrent_file;
mod torrent_manager;
mod tracker;
mod tui;
mod tui_events;

use app::App;
use rand::Rng;

use fs2::FileExt;
use std::fs;
use std::fs::File;

use sha1::{Digest, Sha1};

use std::path::Path;
use std::path::PathBuf;

use crate::config::load_settings;
use crate::config::Settings;

use tracing_appender::rolling;

use ratatui::{backend::CrosstermBackend, Terminal};
use std::env;
use std::io::stdout;

use tracing_subscriber::filter::Targets;
use tracing_subscriber::{filter::LevelFilter, fmt, prelude::*};

use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

// Conditionally import the flags ONLY on non-Windows platforms
#[cfg(not(windows))]
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

use clap::{Parser, Subcommand};

const DEFAULT_LOG_FILTER: LevelFilter = LevelFilter::INFO;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    input: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Add { input: String },
    StopClient,
}

fn process_input(input_str: &str, watch_path: &Path) {
    if input_str.starts_with("magnet:") {
        let hash_bytes = Sha1::digest(input_str.as_bytes());
        let file_hash_hex = hex::encode(hash_bytes);
        let filename = format!("{}.magnet", file_hash_hex);
        let file_path = watch_path.join(filename);
        tracing::info!("Attempting to write magnet link to: {:?}", file_path);
        if let Err(e) = fs::write(&file_path, input_str.as_bytes()) {
            tracing::error!("Failed to write magnet file: {}", e);
        }
    } else {
        let torrent_path = std::path::PathBuf::from(input_str);
        match fs::canonicalize(&torrent_path) {
            Ok(absolute_path) => {
                let hash_bytes = Sha1::digest(absolute_path.to_string_lossy().as_bytes());
                let file_hash_hex = hex::encode(hash_bytes);
                let filename = format!("{}.path", file_hash_hex);
                let dest_path = watch_path.join(filename);
                tracing::info!("Attempting to write torrent path to: {:?}", dest_path);
                if let Err(e) = fs::write(&dest_path, absolute_path.to_string_lossy().as_bytes()) {
                    tracing::error!("Failed to write path file to command directory: {}", e);
                }
            }
            Err(e) => {
                // Don't treat as error if launched by macOS without a valid path
                if !input_str.starts_with("magnet:") {
                    // Avoid logging error for magnet links here
                    tracing::warn!(
                        "Input '{}' is not a valid torrent file path: {}",
                        input_str,
                        e
                    );
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base_data_dir = config::get_app_paths()
        .map(|(_, data_dir)| data_dir)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let log_dir = base_data_dir.join("logs");
    let general_log = rolling::never(&log_dir, "app.log");
    let (non_blocking_general, _guard_general) = tracing_appender::non_blocking(general_log);
    let _subscriber_result = {
        if fs::create_dir_all(&log_dir).is_ok() {
            // 2. Create a module-specific filter to silence the noise
            let quiet_filter = Targets::new()
                .with_default(DEFAULT_LOG_FILTER)
                .with_target("mainline::rpc::socket", LevelFilter::ERROR); // <--- Silence this module

            // 3. Create the logging layer with the compound filter
            let general_layer = fmt::layer()
                .with_writer(non_blocking_general)
                .with_filter(quiet_filter);

            tracing_subscriber::registry()
                .with(general_layer)
                .try_init()
        } else {
            tracing_subscriber::registry().try_init()
        }
    };

    tracing::info!("STARTING SUPERSEEDR");

    if let Err(e) = config::create_watch_directories() {
        tracing::error!("Failed to create watch directories: {}", e);
    }

    let cli = Cli::parse();
    let mut command_processed = false;

    if let Some(direct_input) = cli.input {
        if let Some((watch_path, _)) = config::get_watch_path() {
            tracing::info!("Processing direct input: {}", direct_input);
            process_input(&direct_input, &watch_path);
            command_processed = true;
        } else {
            tracing::error!("Could not get watch path to process direct input.");
        }
    } else if let Some(command) = cli.command {
        if let Some((watch_path, _)) = config::get_watch_path() {
            command_processed = true;
            match command {
                Commands::StopClient => {
                    tracing::info!("Processing StopClient command.");
                    let file_path = watch_path.join("shutdown.cmd");
                    if let Err(e) = fs::write(&file_path, "STOP") {
                        tracing::error!("Failed to write stop command file: {}", e);
                    }
                }
                Commands::Add { input } => {
                    tracing::info!("Processing Add subcommand input: {}", input);
                    process_input(&input, &watch_path);
                }
            }
        } else {
            tracing::error!("Could not get watch path to process subcommand.");
            command_processed = false; // Couldn't process if path failed
        }
    }
    if command_processed {
        tracing::info!("Command processed, exiting temporary instance.");
        return Ok(());
    }

    let mut proceed_to_app = true;
    let mut _lock_file_handle: Option<File> = None;

    if let Some(lock_path) = get_lock_path() {
        if let Ok(file) = File::create(&lock_path) {
            if file.try_lock_exclusive().is_ok() {
                _lock_file_handle = Some(file);
            } else {
                proceed_to_app = false;
            }
        }
    }
    if proceed_to_app {
        let mut client_configs = load_settings();

        #[cfg(all(feature = "dht", feature = "pex"))]
        {
            if client_configs.private_client {
                eprintln!("\n!!!ERROR: POTENTIAL LEAK!!!");
                eprintln!("---------------------------------");
                eprintln!("You are running the normal build of superseedr (with DHT/PEX enabled),");
                eprintln!("but your configuration file indicates you last used a private build.");
                eprintln!("\nThis safety check prevents accidental use of forbidden features on private trackers.");

                // Get the config file path to show the user
                let config_path_str = config::get_app_paths()
                    .map(|(config_dir, _)| config_dir.join("settings.toml"))
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unable to determine config path.".to_string());

                eprintln!("\nChoose an option:");
                eprintln!("  1. If you want to use the PRIVATE build (for private trackers):");
                eprintln!("     Install and run it:");
                eprintln!("       cargo install superseedr --no-default-features");
                eprintln!("       superseedr");
                eprintln!(
                    "\n  2. If you want to switch back to the NORMAL build (for public trackers):"
                );
                eprintln!("     Manually edit your configuration file:");
                eprintln!("       {}", config_path_str); // Show the path
                eprintln!(
                    "     Change the line `private_client = true` to `private_client = false`"
                );
                eprintln!("     Then, run this normal build again.");

                eprintln!("\nExiting to prevent potential tracker issues.");
                std::process::exit(1);
            }
        }

        #[cfg(not(all(feature = "dht", feature = "pex")))]
        {
            if !client_configs.private_client {
                tracing::info!("Setting private mode flag in configuration.");
                client_configs.private_client = true;
                if let Err(e) = config::save_settings(&client_configs) {
                    tracing::error!(
                        "Failed to save settings after setting private mode flag: {}",
                        e
                    );
                }
            }
        }

        if client_configs.client_id.is_empty() {
            client_configs.client_id = generate_client_id_string();
            if let Err(e) = config::save_settings(&client_configs) {
                tracing::error!("Failed to save settings after generating client ID: {}", e);
            }
        }

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = cleanup_terminal();
            original_hook(panic_info);
        }));

        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen,)?;

        // This command ONLY runs on non-Windows platforms (like Linux)
        #[cfg(not(windows))]
        {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES),
                EnableBracketedPaste
            )?;
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let mut app = App::new(client_configs).await?;
        if let Err(e) = app.run(&mut terminal).await {
            eprintln!("[Error] Application failed: {}", e);
        }

        cleanup_terminal()?;
    } else {
        println!("superseedr is already running.");
    }

    Ok(())
}

fn get_lock_path() -> Option<PathBuf> {
    let base_data_dir = config::get_app_paths()
        .map(|(_, data_dir)| data_dir)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    Some(base_data_dir.join("superseedr.lock"))
}

fn cleanup_terminal() -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    // Common cleanup for all platforms
    execute!(stdout(), LeaveAlternateScreen,)?;

    // Corresponding cleanup ONLY for non-Windows platforms
    #[cfg(not(windows))]
    {
        execute!(stdout(), PopKeyboardEnhancementFlags, DisableBracketedPaste)?;
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
