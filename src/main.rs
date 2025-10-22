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

use std::path::PathBuf;

use crate::config::load_settings;
use crate::config::Settings;

use tracing_appender::rolling;

use ratatui::{backend::CrosstermBackend, Terminal};
use std::env;
use std::io::stdout;

use tracing_subscriber::{filter::LevelFilter, fmt, prelude::*};

use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

// Conditionally import the flags ONLY on non-Windows platforms
#[cfg(not(windows))]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Add { input: String },
    StopClient,
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
            let general_layer = fmt::layer()
                .with_writer(non_blocking_general)
                .with_filter(LevelFilter::INFO);
            tracing_subscriber::registry()
                .with(general_layer)
                .try_init()
        } else {
            tracing_subscriber::registry().try_init()
        }
    };

    if let Err(e) = config::create_watch_directories() {
        eprintln!(
            "[Error] Failed to create necessary application directories: {}",
            e
        );
        return Err(e.into());
    }
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        if let Some((watch_path, _)) = config::get_watch_path() {
            match command {
                Commands::StopClient => {
                    let file_path = watch_path.join("shutdown.cmd");
                    if let Err(e) = fs::write(&file_path, "STOP") {
                        tracing::error!("Failed to write stop command file: {}", e);
                    }
                }
                Commands::Add { input } => {
                    if input.starts_with("magnet:") {
                        let hash_bytes = Sha1::digest(input.as_bytes());
                        let file_hash_hex = hex::encode(hash_bytes);
                        let filename = format!("{}.magnet", file_hash_hex);
                        let file_path = watch_path.join(filename);
                        if let Err(e) = fs::write(&file_path, input.as_bytes()) {
                            tracing::error!("Failed to write magnet file: {}", e);
                        }
                    } else {
                        let torrent_path = std::path::PathBuf::from(&input);
                        match fs::canonicalize(&torrent_path) {
                            Ok(absolute_path) => {
                                let hash_bytes =
                                    Sha1::digest(absolute_path.to_string_lossy().as_bytes());
                                let file_hash_hex = hex::encode(hash_bytes);
                                let filename = format!("{}.path", file_hash_hex);
                                let dest_path = watch_path.join(filename);
                                if let Err(e) = fs::write(
                                    &dest_path,
                                    absolute_path.to_string_lossy().as_bytes(),
                                ) {
                                    tracing::error!(
                                        "Failed to write path file to command directory: {}",
                                        e
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::error!("Invalid torrent file path '{}': {}", input, e);
                            }
                        }
                    }
                }
            }
        }
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

        if client_configs.client_id.is_empty() {
            client_configs.client_id = generate_client_id_string();
        }

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = cleanup_terminal();
            original_hook(panic_info);
        }));

        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

        // This command ONLY runs on non-Windows platforms (like Linux)
        #[cfg(not(windows))]
        {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
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
    execute!(stdout(), LeaveAlternateScreen, DisableBracketedPaste,)?;

    // Corresponding cleanup ONLY for non-Windows platforms
    #[cfg(not(windows))]
    {
        execute!(stdout(), PopKeyboardEnhancementFlags)?;
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
