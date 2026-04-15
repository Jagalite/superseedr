// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::FilePriority;
use crate::dht_service::DhtBackendKind;
use crate::fs_atomic::write_bytes_atomically;
use crate::integrations::control::{write_control_request, ControlPriorityTarget, ControlRequest};
use crate::integrations::status::status_file_path;
use clap::{Parser, Subcommand, ValueEnum};
use sha1::{Digest, Sha1};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "A BitTorrent client with local CLI automation and optional shared cluster mode.",
    long_about = None
)]
pub struct Cli {
    #[arg(long, global = true, help = "Return structured JSON output")]
    pub json: bool,

    #[arg(help = "Add a torrent file path or magnet link without using a subcommand")]
    pub input: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(about = "Add one or more torrent paths or magnet links")]
    Add {
        #[arg(
            value_name = "INPUT",
            num_args = 1..,
            help = "Torrent file path(s) or magnet link(s)"
        )]
        inputs: Vec<String>,
    },
    #[command(about = "Request graceful shutdown of the running client or shared leader")]
    StopClient,
    #[command(about = "Show the event journal")]
    Journal,
    #[command(about = "Persist the shared root used for launcher and protocol-handler starts")]
    SetSharedConfig {
        #[arg(
            value_name = "PATH",
            help = "Shared mount root or explicit superseedr-config path"
        )]
        path: PathBuf,
    },
    #[command(about = "Clear the persisted shared root launcher setting")]
    ClearSharedConfig,
    #[command(about = "Show the effective shared root selection and its source")]
    ShowSharedConfig,
    #[command(about = "Persist an explicit host identity for shared mode (optional)")]
    SetHostId {
        #[arg(
            value_name = "HOST_ID",
            help = "Stable host identity to use in shared mode"
        )]
        host_id: String,
    },
    #[command(about = "Clear the persisted shared host identity")]
    ClearHostId,
    #[command(about = "Show the effective host identity selection and its source")]
    ShowHostId,
    #[command(about = "Convert the current standalone config into layered shared config")]
    ToShared {
        #[arg(
            value_name = "PATH",
            help = "Shared mount root or explicit superseedr-config path"
        )]
        path: PathBuf,
    },
    #[command(about = "Convert the active shared config back into standalone local config")]
    ToStandalone,
    #[command(about = "List configured torrents")]
    Torrents,
    #[command(about = "Show one torrent by info hash, or resolve it from a unique file path")]
    Info {
        #[arg(
            value_name = "INFO_HASH_HEX_OR_PATH",
            help = "Torrent info hash or unique file path"
        )]
        target: String,
    },
    #[command(about = "Read status once, stream status updates, or stop status streaming")]
    Status {
        #[arg(long, help = "Continuously print updated status snapshots")]
        follow: bool,
        #[arg(long, help = "Stop runtime status streaming in standalone mode")]
        stop: bool,
        #[arg(
            long,
            value_name = "SECONDS",
            help = "Set the runtime status dump interval"
        )]
        interval: Option<u64>,
    },
    #[command(about = "Pause one or more torrents by info hash or unique file path")]
    Pause {
        #[arg(
            value_name = "INFO_HASH_HEX_OR_PATH",
            help = "One or more torrent targets"
        )]
        targets: Vec<String>,
    },
    #[command(about = "Resume one or more torrents by info hash or unique file path")]
    Resume {
        #[arg(
            value_name = "INFO_HASH_HEX_OR_PATH",
            help = "One or more torrent targets"
        )]
        targets: Vec<String>,
    },
    #[command(about = "Remove one or more torrents without deleting payload data")]
    Remove {
        #[arg(
            value_name = "INFO_HASH_HEX_OR_PATH",
            help = "One or more torrent targets"
        )]
        targets: Vec<String>,
    },
    #[command(about = "Remove one or more torrents and delete payload data when safe")]
    Purge {
        #[arg(
            value_name = "INFO_HASH_HEX_OR_PATH",
            help = "One or more torrent targets"
        )]
        targets: Vec<String>,
    },
    #[command(about = "List files for a torrent by info hash or unique file path")]
    Files {
        #[arg(
            value_name = "INFO_HASH_HEX_OR_PATH",
            help = "Torrent info hash or unique file path"
        )]
        target: String,
    },
    #[command(about = "Set file priority for a torrent by file index or relative path")]
    Priority {
        #[arg(
            value_name = "INFO_HASH_HEX_OR_PATH",
            help = "Torrent info hash or unique file path"
        )]
        target: String,
        #[arg(
            long,
            conflicts_with = "file_path",
            help = "Target a file by zero-based file index"
        )]
        file_index: Option<usize>,
        #[arg(
            long,
            conflicts_with = "file_index",
            help = "Target a file by relative file path"
        )]
        file_path: Option<String>,
        #[arg(help = "Priority to apply")]
        priority: CliPriority,
    },
    #[command(about = "Run a corpus-driven DHT benchmark without starting the full app")]
    DhtBenchmark {
        #[arg(
            value_name = "CORPUS",
            default_value = "tmp/dht_benchmark_infohashes.txt",
            help = "Path to a newline-delimited corpus of 40-char info-hash hex strings"
        )]
        corpus: PathBuf,
        #[arg(
            long,
            value_enum,
            default_value_t = CliDhtBackend::Internal,
            help = "DHT backend to benchmark"
        )]
        backend: CliDhtBackend,
        #[arg(
            long,
            default_value_t = 16,
            help = "Maximum number of concurrent lookups"
        )]
        concurrency: usize,
        #[arg(
            long,
            default_value_t = 1,
            help = "How many warmup passes to run before measuring"
        )]
        warmup_rounds: usize,
        #[arg(
            long,
            default_value_t = 1,
            help = "How many measured passes to run over the corpus"
        )]
        rounds: usize,
        #[arg(long, help = "Optional cap on how many hashes to run from the corpus")]
        limit: Option<usize>,
        #[arg(
            long,
            default_value_t = 1500,
            help = "Stop a lookup after this much idle time with no new batches"
        )]
        idle_timeout_ms: u64,
        #[arg(long, default_value_t = 6000, help = "Hard timeout per lookup")]
        lookup_timeout_ms: u64,
        #[arg(
            long,
            default_value_t = 0,
            help = "UDP port to bind for the benchmark; 0 chooses an ephemeral port"
        )]
        port: u16,
        #[arg(
            long,
            help = "Build a local IPv4 mainline testnet of this size and benchmark against it instead of the live network"
        )]
        testnet_size: Option<usize>,
        #[arg(
            long,
            help = "When using --testnet-size, build the local testnet without pre-seeding routing tables"
        )]
        testnet_unseeded: bool,
        #[arg(
            long,
            default_value_t = 48,
            help = "When using --testnet-size, preload this many synthetic IPv4 peer announcers into the local testnet (0 disables peer seeding)"
        )]
        testnet_peer_announcers: usize,
        #[arg(
            long,
            default_value_t = 0,
            help = "When using --testnet-size, drop this many local testnet DHT nodes after warmup to simulate churn before measured rounds"
        )]
        testnet_churn_drop_count: usize,
        #[arg(
            long,
            value_name = "PATH",
            help = "Optional raw DHT trace output path for deterministic/local backend comparison"
        )]
        trace_path: Option<PathBuf>,
    },
    #[command(about = "Summarize network metrics JSONL without Python")]
    AnalyzeNetworkMetrics {
        #[arg(
            value_name = "PATH",
            default_value = "tmp/network_metrics.jsonl",
            help = "Path to the JSONL metrics file"
        )]
        path: PathBuf,
        #[arg(long, help = "Optional lowercase hex info-hash filter")]
        info_hash: Option<String>,
        #[arg(
            long,
            value_name = "ISO8601",
            help = "Optional inclusive lower timestamp bound"
        )]
        from_ts: Option<String>,
        #[arg(
            long,
            value_name = "ISO8601",
            help = "Optional inclusive upper timestamp bound"
        )]
        to_ts: Option<String>,
    },
    #[command(about = "Score internal DHT stability windows from metrics JSONL")]
    AnalyzeDhtStability {
        #[arg(
            value_name = "PATH",
            default_value = "tmp/network_metrics.jsonl",
            help = "Path to the JSONL metrics file"
        )]
        path: PathBuf,
        #[arg(
            long,
            default_value_t = 1,
            help = "Window size in minutes for health classification"
        )]
        window_minutes: u32,
    },
    #[command(about = "Compare raw DHT trace logs from internal and mainline benchmark runs")]
    CompareDhtTraces {
        #[arg(value_name = "LEFT", help = "Path to the first raw trace log")]
        left: PathBuf,
        #[arg(value_name = "RIGHT", help = "Path to the second raw trace log")]
        right: PathBuf,
        #[arg(
            long,
            default_value_t = 8,
            help = "How many paired trace events to show around the first divergence"
        )]
        context: usize,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliPriority {
    Normal,
    High,
    Skip,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CliDhtBackend {
    Mainline,
    #[default]
    Internal,
}

impl From<CliDhtBackend> for DhtBackendKind {
    fn from(value: CliDhtBackend) -> Self {
        match value {
            CliDhtBackend::Mainline => DhtBackendKind::Mainline,
            CliDhtBackend::Internal => DhtBackendKind::InternalPrototype,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCommandMode {
    Snapshot,
    Follow { interval_secs: u64 },
    SetInterval { interval_secs: u64 },
    Stop,
}

impl From<CliPriority> for FilePriority {
    fn from(value: CliPriority) -> Self {
        match value {
            CliPriority::Normal => FilePriority::Normal,
            CliPriority::High => FilePriority::High,
            CliPriority::Skip => FilePriority::Skip,
        }
    }
}

pub fn write_input_command(input_str: &str, watch_path: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(watch_path)?;

    if input_str.starts_with("magnet:") {
        let hash_bytes = Sha1::digest(input_str.as_bytes());
        let file_hash_hex = hex::encode(hash_bytes);

        let final_filename = format!("{}.magnet", file_hash_hex);
        let final_path = watch_path.join(final_filename);

        tracing::info!(
            "Attempting to write magnet link atomically to final path: {:?}",
            final_path
        );
        match write_bytes_atomically(&final_path, input_str.as_bytes()) {
            Ok(_) => Ok(final_path),
            Err(e) => {
                tracing::error!("Failed to write magnet file atomically: {}", e);
                Err(e)
            }
        }
    } else {
        let torrent_path = PathBuf::from(input_str);
        match fs::canonicalize(&torrent_path) {
            Ok(absolute_path) => {
                let absolute_path_cow = absolute_path.to_string_lossy();
                write_path_command_payload(
                    absolute_path_cow.as_ref(),
                    absolute_path_cow.as_ref(),
                    watch_path,
                )
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
                Err(io::Error::new(io::ErrorKind::InvalidInput, e))
            }
        }
    }
}

pub fn write_path_command_payload(
    path_payload: &str,
    hash_key: &str,
    watch_path: &Path,
) -> io::Result<PathBuf> {
    fs::create_dir_all(watch_path)?;

    let hash_bytes = Sha1::digest(hash_key.as_bytes());
    let file_hash_hex = hex::encode(hash_bytes);
    let final_filename = format!("{}.path", file_hash_hex);
    let final_dest_path = watch_path.join(final_filename);

    tracing::info!(
        "Attempting to write torrent path atomically to final path: {:?}",
        final_dest_path
    );
    match write_bytes_atomically(&final_dest_path, path_payload.as_bytes()) {
        Ok(_) => Ok(final_dest_path),
        Err(e) => {
            tracing::error!("Failed to write path file atomically: {}", e);
            Err(e)
        }
    }
}

pub fn write_stop_command(watch_path: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(watch_path)?;
    let file_path = watch_path.join("shutdown.cmd");
    fs::write(&file_path, "STOP")?;
    Ok(file_path)
}

#[cfg(test)]
pub fn command_to_control_requests(
    command: &Commands,
) -> Result<Option<Vec<ControlRequest>>, String> {
    command_to_control_requests_with_resolver(command, |target, _| Ok(target.to_string()))
}

pub fn command_to_control_requests_with_resolver<F>(
    command: &Commands,
    mut resolve_target: F,
) -> Result<Option<Vec<ControlRequest>>, String>
where
    F: FnMut(&str, &str) -> Result<String, String>,
{
    match command {
        Commands::Status { .. } => Ok(Some(vec![status_control_request(command)?])),
        Commands::Pause { targets } => Ok(Some(
            require_cli_targets(targets, "pause")?
                .into_iter()
                .map(|target| resolve_target(&target, "pause"))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|info_hash_hex| ControlRequest::Pause { info_hash_hex })
                .collect(),
        )),
        Commands::Resume { targets } => Ok(Some(
            require_cli_targets(targets, "resume")?
                .into_iter()
                .map(|target| resolve_target(&target, "resume"))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|info_hash_hex| ControlRequest::Resume { info_hash_hex })
                .collect(),
        )),
        Commands::Remove { targets } => Ok(Some(
            require_cli_targets(targets, "remove")?
                .into_iter()
                .map(|target| resolve_target(&target, "remove"))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|info_hash_hex| ControlRequest::Delete {
                    info_hash_hex,
                    delete_files: false,
                })
                .collect(),
        )),
        Commands::Priority {
            target,
            file_index,
            file_path,
            priority,
        } => {
            let info_hash_hex = resolve_target(target, "priority")?;
            let target = if let Some(file_index) = file_index {
                ControlPriorityTarget::FileIndex(*file_index)
            } else if let Some(file_path) = file_path {
                ControlPriorityTarget::FilePath(file_path.clone())
            } else {
                return Err("Priority requires either --file-index or --file-path".to_string());
            };

            Ok(Some(vec![ControlRequest::SetFilePriority {
                info_hash_hex,
                target,
                priority: (*priority).into(),
            }]))
        }
        Commands::Add { .. }
        | Commands::StopClient
        | Commands::Journal
        | Commands::SetSharedConfig { .. }
        | Commands::ClearSharedConfig
        | Commands::ShowSharedConfig
        | Commands::SetHostId { .. }
        | Commands::ClearHostId
        | Commands::ShowHostId
        | Commands::ToShared { .. }
        | Commands::ToStandalone
        | Commands::Torrents
        | Commands::Info { .. }
        | Commands::Purge { .. }
        | Commands::Files { .. }
        | Commands::DhtBenchmark { .. }
        | Commands::AnalyzeNetworkMetrics { .. }
        | Commands::AnalyzeDhtStability { .. }
        | Commands::CompareDhtTraces { .. } => Ok(None),
    }
}

pub fn status_command_mode(command: &Commands) -> Result<StatusCommandMode, String> {
    let Commands::Status {
        follow,
        stop,
        interval,
    } = command
    else {
        return Err("Expected status command".to_string());
    };

    if *follow && *stop {
        return Err("Choose either --follow or --stop, not both".to_string());
    }
    if *stop && interval.is_some() {
        return Err("Do not use --interval together with --stop".to_string());
    }

    Ok(if *stop {
        StatusCommandMode::Stop
    } else if *follow {
        StatusCommandMode::Follow {
            interval_secs: interval.unwrap_or(5),
        }
    } else if let Some(interval_secs) = interval {
        StatusCommandMode::SetInterval {
            interval_secs: *interval_secs,
        }
    } else {
        StatusCommandMode::Snapshot
    })
}

pub fn status_control_request(command: &Commands) -> Result<ControlRequest, String> {
    Ok(match status_command_mode(command)? {
        StatusCommandMode::Snapshot => ControlRequest::StatusNow,
        StatusCommandMode::Follow { interval_secs }
        | StatusCommandMode::SetInterval { interval_secs } => {
            ControlRequest::StatusFollowStart { interval_secs }
        }
        StatusCommandMode::Stop => ControlRequest::StatusFollowStop,
    })
}

#[cfg(test)]
pub fn command_to_control_request(command: &Commands) -> Result<Option<ControlRequest>, String> {
    match command_to_control_requests(command)? {
        Some(mut requests) => {
            let request = requests
                .drain(..)
                .next()
                .ok_or_else(|| "No control requests were produced".to_string())?;
            Ok(Some(request))
        }
        None => Ok(None),
    }
}

pub fn require_cli_targets(values: &[String], command_name: &str) -> Result<Vec<String>, String> {
    let targets = values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    if targets.is_empty() {
        return Err(format!(
            "Missing target for `superseedr {}`. Use either INFO_HASH_HEX or a file path.",
            command_name
        ));
    }

    Ok(targets)
}

pub fn expand_add_inputs(inputs: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();
    for input in inputs {
        if input.starts_with("magnet:") || Path::new(input).exists() {
            expanded.push(input.clone());
            continue;
        }

        let mut split_values = input
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();

        if split_values.is_empty() {
            continue;
        }

        if split_values.len() == 1 {
            expanded.push(split_values.remove(0));
        } else {
            expanded.extend(split_values);
        }
    }
    expanded
}

pub fn write_control_command(request: &ControlRequest, watch_path: &Path) -> io::Result<PathBuf> {
    write_control_request(request, watch_path)
}

pub fn wait_for_status_json_after(
    previous_modified_at: Option<SystemTime>,
    timeout: Duration,
) -> io::Result<String> {
    let status_path = status_file_path()?;
    let deadline = std::time::Instant::now() + timeout;

    loop {
        if let Ok(metadata) = fs::metadata(&status_path) {
            let modified_at = metadata.modified().ok();
            let is_new_enough = match (previous_modified_at, modified_at) {
                (Some(previous), Some(current)) => current > previous,
                (None, Some(_)) => true,
                (_, None) => false,
            };

            if is_new_enough || previous_modified_at.is_none() {
                return fs::read_to_string(&status_path);
            }
        }

        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Timed out waiting for a fresh status dump",
            ));
        }

        thread::sleep(Duration::from_millis(200));
    }
}

pub fn status_file_modified_at() -> io::Result<Option<SystemTime>> {
    let status_path = status_file_path()?;
    match fs::metadata(status_path) {
        Ok(metadata) => Ok(metadata.modified().ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::fs::{self, File};
    use std::io::Write;

    // Helper to setup a temp directory if tempfile crate is missing
    fn setup_temp_dir() -> (PathBuf, impl Drop) {
        let dir = std::env::temp_dir().join(format!("superseedr_test_{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let dir_clone = dir.clone();
        // Return a dropper to clean up
        struct Cleaner(PathBuf);
        impl Drop for Cleaner {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        (dir, Cleaner(dir_clone))
    }

    #[test]
    fn test_process_input_magnet() {
        let (watch_dir, _cleaner) = setup_temp_dir();
        let magnet_link = "magnet:?xt=urn:btih:5b63529350414441534441534441534441534441";

        write_input_command(magnet_link, &watch_dir).expect("write magnet command");

        // Calculate expected hash
        let hash_bytes = Sha1::digest(magnet_link.as_bytes());
        let expected_name = format!("{}.magnet", hex::encode(hash_bytes));
        let expected_path = watch_dir.join(expected_name);

        assert!(expected_path.exists(), "Magnet file should exist");
        let content = fs::read_to_string(expected_path).unwrap();
        assert_eq!(
            content, magnet_link,
            "File content should be the magnet link"
        );
    }

    #[test]
    fn test_process_input_torrent_path() {
        let (watch_dir, _cleaner) = setup_temp_dir();

        // 1. Create a dummy torrent file to "add"
        let torrent_source_name = "test_linux.torrent";
        let torrent_source_path = watch_dir.join(torrent_source_name);
        {
            let mut f = File::create(&torrent_source_path).unwrap();
            f.write_all(b"dummy torrent content").unwrap();
        }
        let abs_source_path = fs::canonicalize(&torrent_source_path).unwrap();

        // 2. Process the path input
        write_input_command(abs_source_path.to_str().unwrap(), &watch_dir)
            .expect("write path command");

        // 3. Verify the .path file was created
        // The filename is the hash of the *path string*
        let hash_bytes = Sha1::digest(abs_source_path.to_string_lossy().as_bytes());
        let expected_name = format!("{}.path", hex::encode(hash_bytes));
        let expected_path_file = watch_dir.join(expected_name);

        assert!(expected_path_file.exists(), ".path file should be created");

        // 4. Verify content matches the source path
        let content = fs::read_to_string(expected_path_file).unwrap();
        assert_eq!(
            content,
            abs_source_path.to_string_lossy(),
            ".path file should contain the absolute path"
        );
    }

    #[test]
    fn test_process_invalid_path() {
        let (watch_dir, _cleaner) = setup_temp_dir();
        // Pass a non-existent path
        let bad_path = "/path/to/nonexistent/file.torrent";

        // Should not panic
        assert!(write_input_command(bad_path, &watch_dir).is_err());

        // Verify directory is empty (no .path file created)
        let count = fs::read_dir(&watch_dir).unwrap().count();
        assert_eq!(count, 0, "No files should be created for invalid input");
    }

    #[test]
    fn status_command_maps_to_runtime_requests() {
        let follow = Commands::Status {
            follow: true,
            stop: false,
            interval: None,
        };
        let request = status_control_request(&follow).expect("map status command");
        assert_eq!(
            request,
            ControlRequest::StatusFollowStart { interval_secs: 5 }
        );
    }

    #[test]
    fn status_interval_maps_to_runtime_request_without_follow() {
        let command = Commands::Status {
            follow: false,
            stop: false,
            interval: Some(30),
        };
        let request = status_control_request(&command).expect("map status interval");
        assert_eq!(
            request,
            ControlRequest::StatusFollowStart { interval_secs: 30 }
        );
        assert_eq!(
            status_command_mode(&command),
            Ok(StatusCommandMode::SetInterval { interval_secs: 30 })
        );
    }

    #[test]
    fn priority_requires_one_target() {
        let command = Commands::Priority {
            target: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            file_index: None,
            file_path: None,
            priority: CliPriority::High,
        };
        assert!(command_to_control_request(&command).is_err());
    }

    #[test]
    fn journal_command_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::Journal),
            Ok(None)
        ));
    }

    #[test]
    fn torrents_command_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::Torrents),
            Ok(None)
        ));
    }

    #[test]
    fn info_command_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::Info {
                target: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()
            }),
            Ok(None)
        ));
    }

    #[test]
    fn dht_benchmark_command_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::DhtBenchmark {
                corpus: PathBuf::from("tmp/dht_benchmark_infohashes.txt"),
                backend: CliDhtBackend::Internal,
                concurrency: 8,
                warmup_rounds: 1,
                rounds: 1,
                limit: Some(16),
                idle_timeout_ms: 1500,
                lookup_timeout_ms: 6000,
                port: 0,
                testnet_size: None,
                testnet_unseeded: false,
                testnet_peer_announcers: 48,
                testnet_churn_drop_count: 0,
                trace_path: None,
            }),
            Ok(None)
        ));
    }

    #[test]
    fn analyze_network_metrics_command_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::AnalyzeNetworkMetrics {
                path: PathBuf::from("tmp/network_metrics.jsonl"),
                info_hash: None,
                from_ts: None,
                to_ts: None,
            }),
            Ok(None)
        ));
    }

    #[test]
    fn compare_dht_traces_command_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::CompareDhtTraces {
                left: PathBuf::from("tmp/mainline.trace.log"),
                right: PathBuf::from("tmp/internal.trace.log"),
                context: 6,
            }),
            Ok(None)
        ));
    }

    #[test]
    fn analyze_dht_stability_command_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::AnalyzeDhtStability {
                path: PathBuf::from("tmp/network_metrics.jsonl"),
                window_minutes: 1,
            }),
            Ok(None)
        ));
    }

    #[test]
    fn remove_without_target_returns_helpful_error() {
        let error = command_to_control_request(&Commands::Remove {
            targets: Vec::new(),
        })
        .expect_err("missing target should fail");
        assert!(error.contains("Missing target"));
        assert!(error.contains("INFO_HASH_HEX"));
        assert!(error.contains("file path"));
    }

    #[test]
    fn shared_config_commands_are_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::SetSharedConfig {
                path: PathBuf::from("C:/shared-root")
            }),
            Ok(None)
        ));
        assert!(matches!(
            command_to_control_request(&Commands::ClearSharedConfig),
            Ok(None)
        ));
        assert!(matches!(
            command_to_control_request(&Commands::ShowSharedConfig),
            Ok(None)
        ));
    }

    #[test]
    fn remove_command_supports_multiple_hashes() {
        let requests = command_to_control_requests(&Commands::Remove {
            targets: vec![
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            ],
        })
        .expect("map delete commands")
        .expect("requests");
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn purge_requires_at_least_one_target() {
        let error = require_cli_targets(&[], "purge").expect_err("missing target should fail");
        assert!(error.contains("Missing target"));
    }

    #[test]
    fn add_command_expands_comma_separated_non_magnet_inputs() {
        let expanded = expand_add_inputs(&["alpha.torrent,beta.torrent".to_string()]);
        assert_eq!(
            expanded,
            vec!["alpha.torrent".to_string(), "beta.torrent".to_string()]
        );
    }

    #[test]
    fn cli_priority_command_parses_without_panicking() {
        Cli::command().debug_assert();

        let parsed = Cli::try_parse_from([
            "superseedr",
            "priority",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--file-index",
            "0",
            "skip",
        ])
        .expect("priority command should parse");

        match parsed.command.expect("subcommand") {
            Commands::Priority {
                target,
                file_index,
                file_path,
                priority,
            } => {
                assert_eq!(target, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
                assert_eq!(file_index, Some(0));
                assert_eq!(file_path, None);
                assert_eq!(priority, CliPriority::Skip);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn resolved_pause_command_supports_file_lookup() {
        let requests = command_to_control_requests_with_resolver(
            &Commands::Pause {
                targets: vec!["C:/seedbox/downloads/sample.bin".to_string()],
            },
            |target, command_name| {
                assert_eq!(target, "C:/seedbox/downloads/sample.bin");
                assert_eq!(command_name, "pause");
                Ok("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            },
        )
        .expect("map pause commands")
        .expect("requests");

        assert_eq!(
            requests,
            vec![ControlRequest::Pause {
                info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()
            }]
        );
    }

    #[test]
    fn cli_set_shared_config_command_parses_without_panicking() {
        Cli::command().debug_assert();

        let parsed = Cli::try_parse_from([
            "superseedr",
            "set-shared-config",
            "C:\\shared-root\\superseedr-config",
        ])
        .expect("set-shared-config command should parse");

        match parsed.command.expect("subcommand") {
            Commands::SetSharedConfig { path } => {
                assert_eq!(path, PathBuf::from("C:\\shared-root\\superseedr-config"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_set_host_id_command_parses_without_panicking() {
        Cli::command().debug_assert();

        let parsed = Cli::try_parse_from(["superseedr", "set-host-id", "office-node"])
            .expect("set-host-id command should parse");

        match parsed.command.expect("subcommand") {
            Commands::SetHostId { host_id } => {
                assert_eq!(host_id, "office-node");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_to_shared_command_parses_without_panicking() {
        Cli::command().debug_assert();

        let parsed = Cli::try_parse_from(["superseedr", "to-shared", "C:\\shared-root"])
            .expect("to-shared command should parse");

        match parsed.command.expect("subcommand") {
            Commands::ToShared { path } => {
                assert_eq!(path, PathBuf::from("C:\\shared-root"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
