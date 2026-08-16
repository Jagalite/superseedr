// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::FilePriority;
use crate::fs_atomic::publish_bytes_atomically;
use crate::integrations::control::{write_control_request, ControlPriorityTarget, ControlRequest};
use crate::integrations::status::status_file_path;
#[cfg(feature = "synthetic-load")]
use clap::Args;
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

    #[arg(
        long,
        value_name = "INTERFACE",
        help = "Bind this client run to an operating-system network interface"
    )]
    pub network_interface: Option<String>,

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
            long,
            alias = "validate",
            help = "Trust the added torrent data as already validated"
        )]
        validated: bool,
        #[arg(
            long,
            value_name = "PATH",
            help = "Use an existing download directory for all added inputs"
        )]
        path: Option<PathBuf>,
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
    Journal {
        #[arg(
            long,
            help = "Analyze journal ingest entries that can recover missing catalog items"
        )]
        catalog_recovery: bool,
    },
    #[command(about = "Persist the shared root used for launcher and protocol-handler starts")]
    SetSharedConfig {
        #[arg(
            value_name = "PATH",
            help = "Shared mount root or explicit superseedr-config path"
        )]
        path: PathBuf,
    },
    #[command(about = "Persist strict binding to a network interface for this host")]
    SetNetworkInterface {
        #[arg(
            value_name = "INTERFACE",
            help = "Exact operating-system interface identity"
        )]
        interface: String,
    },
    #[command(about = "Clear the persisted shared root launcher setting")]
    ClearSharedConfig,
    #[command(about = "Show the effective shared root selection and its source")]
    ShowSharedConfig,
    #[command(about = "Show resolved config, log, status, journal, and watch paths")]
    ShowConfigs {
        #[arg(long, help = "Include launcher, local, and shared layer details")]
        all: bool,
    },
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
    #[command(about = "Move a torrent while the superseedr client is stopped")]
    Move {
        #[arg(value_name = "INFO_HASH_HEX", help = "Torrent info hash")]
        info_hash_hex: String,
        #[arg(value_name = "PATH", help = "Existing destination directory")]
        path: PathBuf,
    },
    #[cfg(feature = "synthetic-load")]
    #[command(about = "Run adaptive local synthetic benchmarks with bounded disk usage")]
    Benchmark(SyntheticBenchmarkArgs),
    #[cfg(feature = "synthetic-load")]
    #[command(
        name = "synthetic-load",
        hide = true,
        about = "Run a local synthetic BitTorrent load harness"
    )]
    SyntheticLoad(SyntheticLoadArgs),
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliPriority {
    Normal,
    High,
    Skip,
}

#[cfg(feature = "synthetic-load")]
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticLoadMode {
    Download,
    Upload,
    Swarm,
}

#[cfg(feature = "synthetic-load")]
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticTransport {
    Tcp,
    Utp,
    All,
}

#[cfg(feature = "synthetic-load")]
impl SyntheticTransport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Utp => "utp",
            Self::All => "all",
        }
    }
}

#[cfg(feature = "synthetic-load")]
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticTorrentFormat {
    V1,
    V2,
    Hybrid,
}

#[cfg(feature = "synthetic-load")]
impl SyntheticTorrentFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
            Self::Hybrid => "hybrid",
        }
    }
}

#[cfg(feature = "synthetic-load")]
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticLoadAddMode {
    Upfront,
    Burst,
    Staggered,
}

#[cfg(feature = "synthetic-load")]
#[derive(Args, Debug, Clone)]
pub struct SyntheticBenchmarkArgs {
    #[arg(long, default_value_t = 10, help = "Initial torrent count")]
    pub start_torrents: usize,
    #[arg(long, default_value_t = 100, help = "Initial synthetic peer count")]
    pub start_peers: usize,
    #[arg(long, default_value_t = 1000, help = "Maximum torrent count to try")]
    pub max_torrents: usize,
    #[arg(
        long,
        default_value_t = 100_000,
        help = "Maximum synthetic peer count to try"
    )]
    pub max_peers: usize,
    #[arg(
        long,
        default_value_t = 12,
        help = "Maximum benchmark steps per scenario"
    )]
    pub max_steps: usize,
    #[arg(
        long,
        default_value = "8GiB",
        help = "Maximum generated disk working set per benchmark step"
    )]
    pub disk_budget: String,
    #[arg(
        long,
        default_value = "8MiB",
        help = "Preferred per-torrent payload size before disk budget clamping"
    )]
    pub size_per_torrent: String,
    #[arg(long, default_value = "256KiB")]
    pub piece_size: String,
    #[arg(
        long,
        value_enum,
        default_value_t = SyntheticTorrentFormat::V1,
        help = "Torrent metainfo format generated by the synthetic harness"
    )]
    pub torrent_format: SyntheticTorrentFormat,
    #[arg(long, default_value_t = 30)]
    pub duration_secs: u64,
    #[arg(long, default_value_t = 0)]
    pub warmup_secs: u64,
    #[arg(long, default_value_t = 1000)]
    pub metrics_interval_ms: u64,
    #[arg(long, default_value_t = 1)]
    pub leecher_pipeline: usize,
    #[arg(long, default_value_t = 1.0)]
    pub target_gbps: f64,
    #[arg(
        long,
        value_enum,
        default_value_t = SyntheticTransport::All,
        help = "Transport mode used by the synthetic peer harness"
    )]
    pub transport: SyntheticTransport,
    #[command(flatten)]
    pub utp_chaos: SyntheticUdpChaosArgs,
    #[arg(long, default_value_t = 1000)]
    pub peer_add_interval_ms: u64,
    #[arg(long, default_value_t = 10)]
    pub peer_add_burst_size: usize,
    #[arg(long)]
    pub peer_connection_permits: Option<usize>,
    #[arg(long, default_value_t = 256)]
    pub disk_read_permits: usize,
    #[arg(long, default_value_t = 256)]
    pub disk_write_permits: usize,
    #[arg(long, default_value_t = 5000)]
    pub max_sample_delay_ms: u64,
    #[arg(
        long,
        default_value_t = 2,
        help = "Number of additional attempts before a benchmark issue stops a scenario"
    )]
    pub issue_retries: usize,
    #[arg(
        long,
        default_value_t = 1000,
        help = "Delay before retrying a benchmark step after an issue"
    )]
    pub retry_delay_ms: u64,
    #[arg(
        long,
        help = "Keep generated data directories after each benchmark step"
    )]
    pub keep_output: bool,
    #[arg(long, default_value = "tmp/synthetic-benchmark")]
    pub out: PathBuf,
}

#[cfg(feature = "synthetic-load")]
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct SyntheticUdpChaosArgs {
    #[arg(
        long,
        default_value_t = 0,
        help = "Deterministic UDP chaos seed for synthetic uTP runs"
    )]
    pub utp_chaos_seed: u64,
    #[arg(
        long,
        default_value_t = 0,
        help = "Outbound synthetic uTP packet loss, in packets per million"
    )]
    pub utp_chaos_loss_ppm: u32,
    #[arg(
        long,
        default_value_t = 0,
        help = "Outbound synthetic uTP packet duplication, in packets per million"
    )]
    pub utp_chaos_duplicate_ppm: u32,
    #[arg(
        long,
        default_value_t = 0,
        help = "Outbound synthetic uTP packet corruption, in packets per million"
    )]
    pub utp_chaos_corrupt_ppm: u32,
    #[arg(
        long,
        default_value_t = 0,
        help = "Outbound synthetic uTP packet reordering, in packets per million"
    )]
    pub utp_chaos_reorder_ppm: u32,
    #[arg(
        long,
        default_value_t = 0,
        help = "Maximum synthetic uTP packet delay for chaos injection"
    )]
    pub utp_chaos_max_delay_ms: u64,
}

#[cfg(feature = "synthetic-load")]
#[derive(Args, Debug, Clone)]
pub struct SyntheticLoadArgs {
    #[arg(long, default_value_t = 1, help = "Number of synthetic torrents")]
    pub torrents: usize,
    #[arg(
        long,
        default_value_t = 8,
        help = "Total synthetic peers per active role; swarm splits this across download and upload roles"
    )]
    pub peers: usize,
    #[arg(long, value_enum, default_value_t = SyntheticLoadMode::Download)]
    pub mode: SyntheticLoadMode,
    #[arg(
        long,
        value_enum,
        default_value_t = SyntheticLoadAddMode::Upfront,
        help = "When synthetic torrent managers are added to the run"
    )]
    pub add_mode: SyntheticLoadAddMode,
    #[arg(
        long,
        default_value_t = 1000,
        help = "Delay between staggered synthetic torrent add batches"
    )]
    pub add_interval_ms: u64,
    #[arg(
        long,
        default_value_t = 1,
        help = "Number of synthetic torrents per staggered add batch"
    )]
    pub add_burst_size: usize,
    #[arg(
        long,
        value_enum,
        default_value_t = SyntheticLoadAddMode::Upfront,
        help = "When synthetic peers are added after each torrent is active"
    )]
    pub peer_add_mode: SyntheticLoadAddMode,
    #[arg(
        long,
        default_value_t = 1000,
        help = "Delay between staggered synthetic peer add batches"
    )]
    pub peer_add_interval_ms: u64,
    #[arg(
        long,
        default_value_t = 1,
        help = "Number of synthetic peers per staggered peer add batch"
    )]
    pub peer_add_burst_size: usize,
    #[arg(long, default_value = "256MiB")]
    pub size_per_torrent: String,
    #[arg(long, default_value = "256KiB")]
    pub piece_size: String,
    #[arg(
        long,
        value_enum,
        default_value_t = SyntheticTorrentFormat::V1,
        help = "Torrent metainfo format generated by the synthetic harness"
    )]
    pub torrent_format: SyntheticTorrentFormat,
    #[arg(long, default_value_t = 30)]
    pub duration_secs: u64,
    #[arg(long, default_value_t = 12)]
    pub warmup_secs: u64,
    #[arg(long, default_value_t = 1000)]
    pub metrics_interval_ms: u64,
    #[arg(long, default_value_t = 512)]
    pub leecher_pipeline: usize,
    #[arg(long)]
    pub target_gbps: Option<f64>,
    #[arg(
        long,
        value_enum,
        default_value_t = SyntheticTransport::All,
        help = "Transport mode used by the synthetic peer harness"
    )]
    pub transport: SyntheticTransport,
    #[command(flatten)]
    pub utp_chaos: SyntheticUdpChaosArgs,
    #[arg(long)]
    pub peer_connection_permits: Option<usize>,
    #[arg(long, default_value_t = 256)]
    pub disk_read_permits: usize,
    #[arg(long, default_value_t = 256)]
    pub disk_write_permits: usize,
    #[arg(long, default_value = "tmp/synthetic-load")]
    pub out: PathBuf,
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
        match publish_bytes_atomically(&final_path, input_str.as_bytes()) {
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
    match publish_bytes_atomically(&final_dest_path, path_payload.as_bytes()) {
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
        | Commands::Journal { .. }
        | Commands::SetSharedConfig { .. }
        | Commands::SetNetworkInterface { .. }
        | Commands::ClearSharedConfig
        | Commands::ShowSharedConfig
        | Commands::ShowConfigs { .. }
        | Commands::SetHostId { .. }
        | Commands::ClearHostId
        | Commands::ShowHostId
        | Commands::ToShared { .. }
        | Commands::ToStandalone
        | Commands::Torrents
        | Commands::Info { .. }
        | Commands::Purge { .. }
        | Commands::Files { .. }
        | Commands::Move { .. } => Ok(None),
        #[cfg(feature = "synthetic-load")]
        Commands::Benchmark(_) => Ok(None),
        #[cfg(feature = "synthetic-load")]
        Commands::SyntheticLoad(_) => Ok(None),
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
        if input.starts_with("magnet:") {
            expanded.extend(split_comma_joined_magnets(input));
            continue;
        }

        if Path::new(input).exists() {
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

fn split_comma_joined_magnets(input: &str) -> Vec<String> {
    input
        .split(",magnet:?")
        .enumerate()
        .map(|(index, value)| {
            if index == 0 {
                value.to_string()
            } else {
                format!("magnet:?{}", value)
            }
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
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
    fn move_command_is_not_mapped_to_a_runtime_control_request() {
        let command = Commands::Move {
            info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            path: PathBuf::from("/tmp/fictional-downloads"),
        };

        assert_eq!(command_to_control_request(&command), Ok(None));
    }

    #[test]
    fn journal_command_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::Journal {
                catalog_recovery: false,
            }),
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
    fn set_network_interface_is_not_mapped_to_control_request() {
        assert!(matches!(
            command_to_control_request(&Commands::SetNetworkInterface {
                interface: "interface-test0".to_string(),
            }),
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
    fn add_command_expands_comma_joined_magnet_inputs() {
        let expanded = expand_add_inputs(&[concat!(
            "magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&tr=udp%3A%2F%2Ftracker.example",
            ",magnet:?xt=urn:btih:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&dn=beta"
        )
        .to_string()]);

        assert_eq!(expanded.len(), 2);
        assert!(expanded[0].starts_with("magnet:?xt=urn:btih:aaaaaaaa"));
        assert!(expanded[0].contains("&tr=udp%3A%2F%2Ftracker.example"));
        assert_eq!(
            expanded[1],
            "magnet:?xt=urn:btih:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&dn=beta"
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
    fn cli_network_interface_override_parses_for_client_startup() {
        Cli::command().debug_assert();

        let parsed = Cli::try_parse_from(["superseedr", "--network-interface", "interface-test0"])
            .expect("network interface override should parse");

        assert_eq!(parsed.network_interface.as_deref(), Some("interface-test0"));
        assert!(parsed.input.is_none());
        assert!(parsed.command.is_none());
    }

    #[test]
    fn cli_set_network_interface_command_parses() {
        Cli::command().debug_assert();

        let parsed =
            Cli::try_parse_from(["superseedr", "set-network-interface", "interface-test0"])
                .expect("set-network-interface command should parse");

        match parsed.command.expect("subcommand") {
            Commands::SetNetworkInterface { interface } => {
                assert_eq!(interface, "interface-test0");
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
    fn cli_add_command_accepts_validated_aliases() {
        Cli::command().debug_assert();

        for flag in ["--validated", "--validate"] {
            let parsed = Cli::try_parse_from(["superseedr", "add", flag, "sample.bin"])
                .expect("add command should parse validated flag");

            match parsed.command.expect("subcommand") {
                Commands::Add {
                    inputs,
                    validated,
                    path,
                } => {
                    assert!(validated);
                    assert_eq!(path, None);
                    assert_eq!(inputs, vec!["sample.bin".to_string()]);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn cli_add_command_accepts_download_path() {
        Cli::command().debug_assert();

        let parsed = Cli::try_parse_from([
            "superseedr",
            "add",
            "--path",
            "/tmp/superseedr-downloads",
            "sample.bin",
        ])
        .expect("add command should parse path option");

        match parsed.command.expect("subcommand") {
            Commands::Add {
                inputs,
                validated,
                path,
            } => {
                assert!(!validated);
                assert_eq!(path, Some(PathBuf::from("/tmp/superseedr-downloads")));
                assert_eq!(inputs, vec!["sample.bin".to_string()]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
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
